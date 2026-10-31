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

use terminal::TerminalState;
pub use terminal::{GridSize, SCROLLBACK_LINES, TerminalEvents, TerminalSize, default_color};
use transport::Output;

pub const MAX_INPUT_BYTES: usize = 4_096;
pub const QUEUE_CHUNKS: usize = 32;
const UPDATE_BYTES: usize = 64 * 1_024;
const UPDATE_TIME: Duration = Duration::from_millis(4);
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
    input: SyncSender<Vec<u8>>,
    resize: SyncSender<TerminalSize>,
    resizing: bool,
    output: Receiver<Output>,
    cancelled: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<ExitStatus>>>,
    wakeup: Wakeup,
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
        let worker = thread::Builder::new()
            .name("vasterm-pty".into())
            .spawn(move || {
                let result = transport::run(
                    command,
                    size,
                    (input_rx, resize_rx),
                    &output_tx,
                    &worker_cancelled,
                    &worker_child_exited,
                    &worker_wakeup,
                );
                // Disconnection means all PTY work and cleanup have finished.
                drop(output_tx);
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
            cancelled,
            child_exited,
            worker: Some(worker),
            wakeup,
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
        enqueue_input(&self.input, bytes)
    }

    /// Updates UI-owned state with a soft time budget checked between output chunks.
    pub fn poll(&mut self) {
        let start = Instant::now();
        let mut processed = 0;
        let mut disconnected = false;
        while processed < UPDATE_BYTES && start.elapsed() < UPDATE_TIME {
            match self.output.try_recv() {
                Ok(Output::Started(pid)) => {
                    if !matches!(self.state, SessionState::Failed(_) | SessionState::Closed) {
                        self.state = SessionState::Running { pid };
                    }
                }
                Ok(Output::Bytes(bytes)) => {
                    processed += bytes.len();
                    if !matches!(self.state, SessionState::Failed(_) | SessionState::Closed)
                        && let Err(error) = self.terminal.advance(&bytes, |response| {
                            // Draining a dead peer's final output requires no protocol reply.
                            if self.child_exited.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            let result = enqueue_input(&self.input, response);
                            if self.child_exited.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            result.context("send terminal response")
                        })
                    {
                        self.fail(format!("{error:#}"));
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
        if processed >= UPDATE_BYTES || start.elapsed() >= UPDATE_TIME {
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

fn enqueue_input(input: &SyncSender<Vec<u8>>, bytes: &[u8]) -> Result<()> {
    ensure!(!bytes.is_empty(), "cannot send empty terminal input");
    ensure!(
        bytes.len() <= MAX_INPUT_BYTES,
        "terminal input exceeds {MAX_INPUT_BYTES} bytes"
    );
    match input.try_send(bytes.to_vec()) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => bail!("terminal input queue is full; input was not accepted"),
        Err(TrySendError::Disconnected(_)) => bail!("terminal input worker disconnected"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> (Session, Receiver<Vec<u8>>, SyncSender<Output>) {
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
                cancelled: Arc::new(AtomicBool::new(false)),
                child_exited: Arc::new(AtomicBool::new(false)),
                worker: None,
                wakeup: Arc::new(|| {}),
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
        assert!(enqueue_input(&input, &[]).is_err());
        assert!(enqueue_input(&input, &vec![0; MAX_INPUT_BYTES + 1]).is_err());
        enqueue_input(&input, b"first").unwrap();
        assert!(
            enqueue_input(&input, b"second")
                .unwrap_err()
                .to_string()
                .contains("full")
        );
        assert_eq!(output.recv().unwrap(), b"first");
        drop(output);
        assert!(
            enqueue_input(&input, b"third")
                .unwrap_err()
                .to_string()
                .contains("disconnected")
        );
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
