mod diagnostics;
mod terminal;
mod transport;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use anyhow::{Context, Result, bail, ensure};
use portable_pty::{CommandBuilder, ExitStatus};

pub use diagnostics::SessionDiagnostics;
use diagnostics::WorkerDiagnostics;
use terminal::TerminalState;
pub use terminal::{GridSize, SCROLLBACK_LINES, TerminalEvents, TerminalSize, default_color};
use transport::Output;

pub const MAX_INPUT_BYTES: usize = 4_096;
pub const QUEUE_CHUNKS: usize = 32;
pub const UPDATE_BYTES: usize = 64 * 1_024;
pub const UPDATE_TIME: Duration = Duration::from_millis(4);
/// Maximum bytes parsed before checking the soft update deadline again.
pub const PARSE_BYTES: usize = 256;
/// Schedule UI work promptly; the callback also runs on the PTY worker.
pub type Wakeup = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone)]
pub enum SessionState {
    Starting,
    Running { pid: u32 },
    Draining,
    Exited(ExitStatus),
    Failed(String),
    Closed,
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => write!(f, "Starting local session..."),
            Self::Running { pid } => write!(f, "Session running (PID {pid})"),
            Self::Draining => write!(f, "Child exited; processing final terminal output..."),
            Self::Exited(status) => write!(f, "Session exited: {status}"),
            Self::Failed(error) => write!(f, "Session failed: {error}"),
            Self::Closed => write!(f, "Session closed"),
        }
    }
}

pub fn local_shell_command() -> Result<CommandBuilder> {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .context("SHELL is unset; set it to the path of your local shell")?;
    let mut command = CommandBuilder::new(shell);
    command.arg("-i");
    Ok(command)
}

fn configure_environment(command: &mut CommandBuilder) {
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "vasterm");
    command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    for key in ["LINES", "COLUMNS", "WINDOWID", "ALACRITTY_WINDOW_ID"] {
        command.env_remove(key);
    }
}

pub struct Session {
    terminal: TerminalState,
    state: SessionState,
    size: GridSize,
    input: SyncSender<transport::Input>,
    resize: SyncSender<TerminalSize>,
    resizing: bool,
    output: Receiver<Output>,
    pending_bytes: Option<(Vec<u8>, usize)>,
    pending_output: Option<Output>,
    cancelled: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<ExitStatus>>>,
    wakeup: Wakeup,
    started_at: Instant,
    diagnostics: SessionDiagnostics,
    worker_diagnostics: Arc<WorkerDiagnostics>,
}

impl Session {
    /// Starts asynchronously. Child startup failures are reported through `state` after `poll`.
    pub fn spawn(mut command: CommandBuilder, size: GridSize, wakeup: Wakeup) -> Result<Self> {
        configure_environment(&mut command);
        let (input, input_rx) = mpsc::sync_channel(QUEUE_CHUNKS);
        let (resize, resize_rx) = mpsc::sync_channel(1);
        let (output_tx, output) = mpsc::sync_channel(QUEUE_CHUNKS);
        let cancelled = Arc::new(AtomicBool::new(false));
        let child_exited = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker_child_exited = child_exited.clone();
        let worker_wakeup = wakeup.clone();
        let started_at = Instant::now();
        let worker_diagnostics = Arc::new(WorkerDiagnostics::default());
        let measurements = worker_diagnostics.clone();
        let worker = thread::Builder::new()
            .name("vasterm-pty".into())
            .spawn(move || {
                let result = transport::run(
                    command,
                    size,
                    (input_rx, resize_rx),
                    &output_tx,
                    &transport::WorkerContext {
                        cancelled: &worker_cancelled,
                        child_exited: &worker_child_exited,
                        wakeup: &worker_wakeup,
                        diagnostics: &measurements,
                        started_at,
                    },
                );
                // Disconnection means all PTY work and cleanup have finished.
                drop(output_tx);
                diagnostics::increment(&measurements.wakeups);
                worker_wakeup();
                result
            })
            .context("start session worker")?;
        Ok(Self {
            terminal: TerminalState::new(size),
            state: SessionState::Starting,
            size,
            input,
            resize,
            resizing: false,
            output,
            pending_bytes: None,
            pending_output: None,
            cancelled,
            child_exited,
            worker: Some(worker),
            wakeup,
            started_at,
            diagnostics: SessionDiagnostics::default(),
            worker_diagnostics,
        })
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    pub fn terminal(&self) -> &Term<TerminalEvents> {
        &self.terminal.core
    }

    /// Samples counters without requesting repaint or retaining terminal/input contents.
    pub fn diagnostics(&self) -> SessionDiagnostics {
        self.worker_diagnostics.snapshot(SessionDiagnostics {
            elapsed: self.started_at.elapsed(),
            ..self.diagnostics
        })
    }

    /// Requests a worker-side resize. The model changes only after PTY success.
    /// While a resize is in flight, callers should retry their latest dimensions.
    pub fn resize(&mut self, size: TerminalSize) -> Result<()> {
        size.pty_size()?;
        if self.resizing || size == self.terminal.size {
            return Ok(());
        }
        ensure!(
            matches!(
                self.state,
                SessionState::Starting | SessionState::Running { .. }
            ),
            "cannot resize an inactive session"
        );
        self.resize.try_send(size).context("queue PTY resize")?;
        self.resizing = true;
        Ok(())
    }

    /// Accepts one bounded input message, or returns an error without queuing any of it.
    pub fn send_input(&self, bytes: &[u8]) -> Result<()> {
        ensure!(
            matches!(self.state, SessionState::Running { .. }),
            "session is not running"
        );
        ensure!(
            !self.child_exited.load(Ordering::Acquire),
            "session child has exited"
        );
        enqueue_input(&self.input, bytes, Some(Instant::now()))
    }

    /// Updates UI-owned state with a soft time budget checked between bounded parse slices.
    pub fn poll(&mut self) {
        self.poll_with_clock(Instant::now);
    }

    fn poll_with_clock(&mut self, mut now: impl FnMut() -> Instant) {
        let start = now();
        let mut processed = 0;
        let mut disconnected = false;
        let mut budget_exhausted = false;
        loop {
            if processed >= UPDATE_BYTES || now().duration_since(start) >= UPDATE_TIME {
                budget_exhausted = true;
                break;
            }
            let (event, offset) = if let Some((bytes, offset)) = self.pending_bytes.take() {
                (Ok(Output::Bytes(bytes)), offset)
            } else {
                (
                    self.pending_output
                        .take()
                        .map(Ok)
                        .unwrap_or_else(|| self.output.try_recv()),
                    0,
                )
            };
            match event {
                Ok(Output::Started(pid)) => {
                    if !matches!(self.state, SessionState::Failed(_) | SessionState::Closed) {
                        self.state = SessionState::Running { pid };
                    }
                }
                Ok(Output::Bytes(bytes)) => {
                    if bytes.len() > transport::CHUNK_BYTES {
                        self.fail("PTY output chunk exceeds configured byte limit".into());
                        break;
                    }
                    let allowance = (bytes.len() - offset).min(UPDATE_BYTES - processed);
                    let chunk_start = now();
                    let mut boundary = chunk_start;
                    let count =
                        if matches!(self.state, SessionState::Failed(_) | SessionState::Closed) {
                            allowance
                        } else {
                            match self.terminal.advance_with_budget(
                                &bytes[offset..offset + allowance],
                                |response| {
                                    // Draining a dead peer's final output requires no protocol reply.
                                    if self.child_exited.load(Ordering::Acquire) {
                                        return Ok(());
                                    }
                                    let result = enqueue_input(&self.input, response, None);
                                    if self.child_exited.load(Ordering::Acquire) {
                                        return Ok(());
                                    }
                                    result.context("send terminal response")
                                },
                                |slice_bytes| {
                                    let end = now();
                                    self.diagnostics.max_parse_duration = self
                                        .diagnostics
                                        .max_parse_duration
                                        .max(end.duration_since(boundary));
                                    self.diagnostics.max_parse_bytes =
                                        self.diagnostics.max_parse_bytes.max(slice_bytes);
                                    boundary = end;
                                    end.duration_since(start) < UPDATE_TIME
                                },
                            ) {
                                Ok(count) => count,
                                Err(error) => {
                                    self.fail(format!("{error:#}"));
                                    allowance
                                }
                            }
                        };
                    processed += count;
                    self.diagnostics.max_chunk_duration = self
                        .diagnostics
                        .max_chunk_duration
                        .max(now().duration_since(chunk_start));
                    if offset + count < bytes.len() {
                        self.pending_bytes = Some((bytes, offset + count));
                    }
                }
                Ok(Output::Resized(size)) => {
                    self.resizing = false;
                    if !matches!(self.state, SessionState::Failed(_) | SessionState::Closed) {
                        self.terminal.core.resize(size.grid);
                        self.terminal.size = size;
                        self.size = size.grid;
                    }
                }
                Ok(Output::Exited(status)) => {
                    if !matches!(self.state, SessionState::Failed(_) | SessionState::Closed) {
                        self.state = SessionState::Exited(status);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if budget_exhausted && self.pending_bytes.is_none() && self.pending_output.is_none() {
            // A publication after this empty check has its own worker wakeup.
            match self.output.try_recv() {
                Ok(event) => self.pending_output = Some(event),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => disconnected = true,
            }
        }
        if (disconnected
            || self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.is_finished()))
            && let Err(error) = self.join_worker()
        {
            self.fail(format!("{error:#}"));
        }
        if self.child_exited.load(Ordering::Acquire)
            && matches!(self.state, SessionState::Running { .. })
        {
            self.state = SessionState::Draining;
        }
        let duration = now().duration_since(start);
        self.diagnostics.polls = self.diagnostics.polls.saturating_add(1);
        self.diagnostics.processed_bytes = self
            .diagnostics
            .processed_bytes
            .saturating_add(processed as u64);
        self.diagnostics.last_update_bytes = processed;
        self.diagnostics.max_update_bytes = self.diagnostics.max_update_bytes.max(processed);
        self.diagnostics.last_poll_duration = duration;
        self.diagnostics.max_poll_duration = self.diagnostics.max_poll_duration.max(duration);
        if budget_exhausted && (self.pending_bytes.is_some() || self.pending_output.is_some()) {
            self.diagnostics.budget_followups = self.diagnostics.budget_followups.saturating_add(1);
            (self.wakeup)();
        }
    }

    /// Cancels pending I/O, joins the worker, and reaps the child; buffered output is abandoned.
    pub fn shutdown(&mut self) -> Result<()> {
        self.cancelled.store(true, Ordering::Release);
        if let Err(error) = self.join_worker() {
            self.fail(format!("{error:#}"));
            return Err(error);
        }
        if !matches!(
            self.state,
            SessionState::Failed(_) | SessionState::Exited(_)
        ) {
            self.state = SessionState::Closed;
        }
        Ok(())
    }

    fn fail(&mut self, error: String) {
        self.cancelled.store(true, Ordering::Release);
        self.state = SessionState::Failed(error);
    }

    fn join_worker(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("session worker panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("Vasterm: could not close session: {error:#}");
        }
    }
}

fn enqueue_input(
    input: &SyncSender<transport::Input>,
    bytes: &[u8],
    accepted_at: Option<Instant>,
) -> Result<()> {
    ensure!(!bytes.is_empty(), "cannot send empty terminal input");
    ensure!(
        bytes.len() <= MAX_INPUT_BYTES,
        "terminal input exceeds {MAX_INPUT_BYTES} bytes"
    );
    match input.try_send(transport::Input {
        bytes: bytes.to_vec(),
        accepted_at,
    }) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => bail!("terminal input queue is full; input was not accepted"),
        Err(TrySendError::Disconnected(_)) => bail!("terminal input worker disconnected"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> (Session, Receiver<transport::Input>, SyncSender<Output>) {
        let (input, input_rx) = mpsc::sync_channel(QUEUE_CHUNKS);
        let (resize, _resize_rx) = mpsc::sync_channel(1);
        let (output_tx, output) = mpsc::sync_channel(QUEUE_CHUNKS);
        let size = GridSize::default();
        (
            Session {
                terminal: TerminalState::new(size),
                state: SessionState::Running { pid: 1 },
                size,
                input,
                resize,
                resizing: false,
                output,
                pending_bytes: None,
                pending_output: None,
                cancelled: Arc::new(AtomicBool::new(false)),
                child_exited: Arc::new(AtomicBool::new(false)),
                worker: None,
                wakeup: Arc::new(|| {}),
                started_at: Instant::now(),
                diagnostics: SessionDiagnostics::default(),
                worker_diagnostics: Arc::new(WorkerDiagnostics::default()),
            },
            input_rx,
            output_tx,
        )
    }

    #[test]
    fn a_live_response_transport_failure_is_visible_in_session_state() {
        let (mut session, input_rx, output) = test_session();
        drop(input_rx);
        output.send(Output::Bytes(b"\x1b[6n".to_vec())).unwrap();
        session.poll();
        assert!(matches!(session.state(), SessionState::Failed(message)
            if message.contains("send terminal response") && message.contains("disconnected")));
        assert!(session.cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn worker_io_errors_are_visible_in_session_state() {
        let (mut session, _input_rx, _output) = test_session();
        session.worker = Some(thread::spawn(|| {
            bail!("read PTY output: injected I/O failure")
        }));
        while !session.worker.as_ref().unwrap().is_finished() {
            thread::yield_now();
        }
        session.poll();
        assert!(matches!(session.state(), SessionState::Failed(message)
            if message.contains("read PTY output: injected I/O failure")));
    }

    #[test]
    fn resize_changes_the_model_only_on_acknowledgment() {
        let (mut session, _input, output) = test_session();
        let (resize, requests) = mpsc::sync_channel(1);
        session.resize = resize;
        let size = TerminalSize {
            grid: GridSize::new(90, 30).unwrap(),
            cell_width: 9,
            cell_height: 18,
        };
        session.resize(size).unwrap();
        assert_eq!(requests.recv().unwrap(), size);
        assert_eq!(session.size(), GridSize::default());
        let latest = TerminalSize {
            cell_width: 18,
            ..size
        };
        session.resize(latest).unwrap();
        assert!(
            requests.try_recv().is_err(),
            "only one resize may be in flight"
        );
        output.send(Output::Resized(size)).unwrap();
        session.poll();
        assert_eq!(session.size(), size.grid);
        assert_eq!(session.terminal.size, size);
        session.resize(latest).unwrap();
        assert_eq!(
            requests.recv().unwrap(),
            latest,
            "scale-only changes reach the PTY"
        );
        output.send(Output::Resized(latest)).unwrap();
        session.poll();
        session.resize(latest).unwrap();
        assert!(
            requests.try_recv().is_err(),
            "unchanged dimensions do not cause resize loops"
        );
    }

    #[test]
    fn invalid_resize_is_rejected_and_worker_resize_errors_are_visible() {
        let (mut session, _input, _output) = test_session();
        let before = session.size();
        assert!(
            session
                .resize(TerminalSize {
                    grid: before,
                    cell_width: u16::MAX,
                    cell_height: 16,
                })
                .is_err()
        );
        session.worker = Some(thread::spawn(|| bail!("resize PTY: injected I/O failure")));
        while !session.worker.as_ref().unwrap().is_finished() {
            thread::yield_now();
        }
        session.poll();
        assert!(
            matches!(session.state(), SessionState::Failed(message) if message.contains("resize PTY"))
        );
        assert_eq!(session.size(), before);
    }

    #[test]
    fn input_limits_and_queue_pressure_are_explicit_errors() {
        let (input, output) = mpsc::sync_channel(1);
        assert!(enqueue_input(&input, &[], None).is_err());
        assert!(enqueue_input(&input, &vec![0; MAX_INPUT_BYTES + 1], None).is_err());
        enqueue_input(&input, b"first", None).unwrap();
        assert!(
            enqueue_input(&input, b"second", None)
                .unwrap_err()
                .to_string()
                .contains("full")
        );
        assert_eq!(output.recv().unwrap().bytes, b"first");
        drop(output);
        assert!(
            enqueue_input(&input, b"third", None)
                .unwrap_err()
                .to_string()
                .contains("disconnected")
        );
    }

    #[test]
    fn full_output_queue_resumes_losslessly_with_a_hard_update_byte_cap() {
        let (mut session, _input, output) = test_session();
        assert_eq!(UPDATE_BYTES % transport::CHUNK_BYTES, 0);
        for index in 0..QUEUE_CHUNKS {
            output
                .try_send(Output::Bytes(vec![
                    b'a' + index as u8;
                    transport::CHUNK_BYTES
                ]))
                .unwrap();
        }
        assert!(matches!(
            output.try_send(Output::Bytes(vec![b'z'; transport::CHUNK_BYTES])),
            Err(TrySendError::Full(_))
        ));
        let clock = Instant::now();
        session.poll_with_clock(|| clock);
        let stats = session.diagnostics();
        assert_eq!(stats.last_update_bytes, UPDATE_BYTES);
        assert_eq!(stats.budget_followups, 1);
        // The second half must still be in the real bounded output channel, in order.
        for index in UPDATE_BYTES / transport::CHUNK_BYTES..QUEUE_CHUNKS {
            match session
                .pending_output
                .take()
                .unwrap_or_else(|| session.output.try_recv().unwrap())
            {
                Output::Bytes(bytes) => {
                    assert_eq!(bytes, vec![b'a' + index as u8; transport::CHUNK_BYTES])
                }
                _ => panic!("unexpected control message"),
            }
        }
        assert!(matches!(
            session.output.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(!matches!(session.state(), SessionState::Failed(_)));
    }

    #[test]
    fn soft_time_budget_preserves_remaining_chunks_and_records_overrun() {
        let (mut session, _input, output) = test_session();
        output.send(Output::Bytes(b"first".to_vec())).unwrap();
        output.send(Output::Bytes(b"second".to_vec())).unwrap();
        let start = Instant::now();
        let mut calls = 0;
        session.poll_with_clock(|| {
            calls += 1;
            // Start, budget check, chunk start, then an expensive chunk completes.
            if calls <= 3 {
                start
            } else {
                start + UPDATE_TIME + Duration::from_millis(1)
            }
        });
        let stats = session.diagnostics();
        assert_eq!(stats.last_update_bytes, 5);
        assert_eq!(
            stats.max_chunk_duration,
            UPDATE_TIME + Duration::from_millis(1)
        );
        assert_eq!(stats.budget_followups, 1);
        session.poll_with_clock(|| start);
        assert_eq!(session.diagnostics().last_update_bytes, 6);
        assert_eq!(session.diagnostics().processed_bytes, 11);
    }

    #[test]
    fn uneven_chunks_cannot_exceed_the_byte_cap_or_reorder_resize() {
        let (mut session, _input, output) = test_session();
        output.send(Output::Bytes(b"x".to_vec())).unwrap();
        for _ in 0..UPDATE_BYTES / transport::CHUNK_BYTES {
            output
                .send(Output::Bytes(vec![b'y'; transport::CHUNK_BYTES]))
                .unwrap();
        }
        let size = TerminalSize {
            grid: GridSize::new(90, 30).unwrap(),
            ..Default::default()
        };
        output.send(Output::Resized(size)).unwrap();
        let now = Instant::now();
        session.poll_with_clock(|| now);
        assert_eq!(session.diagnostics().last_update_bytes, UPDATE_BYTES);
        assert_eq!(
            session.pending_bytes.as_ref().unwrap().1,
            transport::CHUNK_BYTES - 1
        );
        assert_eq!(session.size(), GridSize::default());
        session.poll_with_clock(|| now);
        assert_eq!(session.diagnostics().last_update_bytes, 1);
        assert!(session.pending_bytes.is_none());
        assert_eq!(session.size(), size.grid);
        assert_eq!(
            session.diagnostics().processed_bytes,
            UPDATE_BYTES as u64 + 1
        );
    }

    #[test]
    fn split_budget_boundary_preserves_utf8_and_query_time_dimensions() {
        let (mut session, input, output) = test_session();
        output.send(Output::Bytes(vec![0])).unwrap();
        for _ in 0..UPDATE_BYTES / transport::CHUNK_BYTES - 1 {
            output
                .send(Output::Bytes(vec![0; transport::CHUNK_BYTES]))
                .unwrap();
        }
        let mut final_chunk = vec![0; transport::CHUNK_BYTES];
        final_chunk[transport::CHUNK_BYTES - 2..].copy_from_slice(b"\xc3\xa9");
        output.send(Output::Bytes(final_chunk)).unwrap();
        output.send(Output::Bytes(b"\x1b[18t".to_vec())).unwrap();
        let resized = TerminalSize {
            grid: GridSize::new(90, 30).unwrap(),
            ..Default::default()
        };
        output.send(Output::Resized(resized)).unwrap();
        let clock = Instant::now();
        session.poll_with_clock(|| clock);
        assert_eq!(session.diagnostics().last_update_bytes, UPDATE_BYTES);
        session.poll_with_clock(|| clock);
        assert_eq!(
            session.terminal().grid()[alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(0),
                alacritty_terminal::index::Column(0)
            )]
            .c,
            '\u{e9}'
        );
        let reply = input.try_recv().unwrap();
        assert_eq!(reply.bytes, b"\x1b[8;24;80t");
        assert!(reply.accepted_at.is_none());
        assert_eq!(session.size(), resized.grid);
    }

    #[test]
    fn idle_polling_and_sampling_do_not_request_repaint() {
        use std::sync::atomic::AtomicUsize;
        let (mut session, _input, _output) = test_session();
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = wakes.clone();
        session.wakeup = Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        });
        let clock = Instant::now();
        for _ in 0..100 {
            session.poll_with_clock(|| clock);
            let stats = session.diagnostics();
            assert_eq!(stats.budget_followups, 0);
            assert_eq!(stats.processed_bytes, 0);
        }
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn time_budget_stops_inside_a_chunk_without_reading_later_resize() {
        let (mut session, _input, output) = test_session();
        output
            .send(Output::Bytes(vec![b'x'; transport::CHUNK_BYTES]))
            .unwrap();
        let resized = TerminalSize {
            grid: GridSize::new(90, 30).unwrap(),
            ..Default::default()
        };
        output.send(Output::Resized(resized)).unwrap();
        let start = Instant::now();
        let mut calls = 0;
        session.poll_with_clock(|| {
            calls += 1;
            if calls <= 3 {
                start
            } else {
                start + UPDATE_TIME
            }
        });
        assert_eq!(session.diagnostics().last_update_bytes, PARSE_BYTES);
        assert_eq!(session.diagnostics().max_parse_bytes, PARSE_BYTES);
        assert_eq!(session.pending_bytes.as_ref().unwrap().1, PARSE_BYTES);
        assert_eq!(session.size(), GridSize::default());
        session.poll_with_clock(|| start);
        assert_eq!(
            session.diagnostics().processed_bytes,
            transport::CHUNK_BYTES as u64
        );
        assert_eq!(session.size(), resized.grid);
    }

    #[test]
    fn exhausted_budget_with_no_backlog_does_not_schedule_an_empty_pass() {
        let (mut session, _input, output) = test_session();
        for _ in 0..UPDATE_BYTES / transport::CHUNK_BYTES {
            output
                .send(Output::Bytes(vec![0; transport::CHUNK_BYTES]))
                .unwrap();
        }
        let clock = Instant::now();
        session.poll_with_clock(|| clock);
        assert_eq!(session.diagnostics().last_update_bytes, UPDATE_BYTES);
        assert_eq!(session.diagnostics().budget_followups, 0);
        assert!(session.pending_output.is_none());
        let mut calls = 0;
        session.poll_with_clock(|| {
            calls += 1;
            if calls == 1 {
                clock
            } else {
                clock + UPDATE_TIME
            }
        });
        assert_eq!(session.diagnostics().last_update_bytes, 0);
        assert_eq!(session.diagnostics().budget_followups, 0);
    }

    #[test]
    fn publication_on_either_side_of_backlog_probe_is_not_lost() {
        use std::sync::atomic::AtomicUsize;
        for publish_on_call in [2, 3] {
            let (mut session, _input, output) = test_session();
            let wakes = Arc::new(AtomicUsize::new(0));
            let worker_wakes = wakes.clone();
            let ui_wakes = wakes.clone();
            session.wakeup = Arc::new(move || {
                ui_wakes.fetch_add(1, Ordering::Relaxed);
            });
            let start = Instant::now();
            let mut calls = 0;
            session.poll_with_clock(|| {
                calls += 1;
                if calls == publish_on_call {
                    output.send(Output::Bytes(b"late".to_vec())).unwrap();
                    // Match the transport's publication-before-wakeup ordering.
                    worker_wakes.fetch_add(1, Ordering::Relaxed);
                }
                if calls == 1 {
                    start
                } else {
                    start + UPDATE_TIME
                }
            });
            assert!(wakes.load(Ordering::Relaxed) > 0);
            assert_eq!(
                session.diagnostics().budget_followups,
                u64::from(publish_on_call == 2)
            );
            session.poll_with_clock(|| start);
            assert_eq!(session.diagnostics().processed_bytes, 4);
        }
    }

    #[test]
    fn configured_input_queue_accepts_exactly_32_maximum_chunks() {
        let (session, receiver, _output) = test_session();
        for _ in 0..QUEUE_CHUNKS {
            session.send_input(&vec![b'x'; MAX_INPUT_BYTES]).unwrap();
        }
        assert!(
            session
                .send_input(b"overflow")
                .unwrap_err()
                .to_string()
                .contains("full")
        );
        for _ in 0..QUEUE_CHUNKS {
            let input = receiver.try_recv().unwrap();
            assert_eq!(input.bytes.len(), MAX_INPUT_BYTES);
            assert!(input.accepted_at.is_some());
        }
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn oversized_output_is_a_visible_failure_not_a_budget_overrun() {
        let (mut session, _input, output) = test_session();
        output
            .send(Output::Bytes(vec![0; transport::CHUNK_BYTES + 1]))
            .unwrap();
        session.poll();
        assert!(
            matches!(session.state(), SessionState::Failed(message) if message.contains("chunk"))
        );
        assert_eq!(session.diagnostics().processed_bytes, 0);
    }

    #[test]
    fn child_environment_does_not_impersonate_the_parent_terminal() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("TERM", "parent-terminal");
        command.env("COLUMNS", "200");
        command.env("LINES", "99");
        configure_environment(&mut command);
        assert_eq!(command.get_env("TERM"), Some("xterm-256color".as_ref()));
        assert_eq!(command.get_env("TERM_PROGRAM"), Some("vasterm".as_ref()));
        assert_eq!(command.get_env("COLUMNS"), None);
        assert_eq!(command.get_env("LINES"), None);
    }
}
