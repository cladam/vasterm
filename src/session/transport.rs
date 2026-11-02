use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use super::cleanup::{self, CleanupControl};
use super::diagnostics::{WorkerDiagnostics, increment, nanoseconds};
use super::{GridSize, MAX_INPUT_BYTES, TerminalSize, Wakeup};

pub(super) const CHUNK_BYTES: usize = 4_096;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
pub const FINAL_DRAIN_TIME: Duration = Duration::from_secs(2);
const EOF_EXIT_GRACE: Duration = Duration::from_millis(200);

pub(super) struct Input {
    pub bytes: Vec<u8>,
    pub accepted_at: Option<Instant>,
}

pub(super) struct WorkerContext<'a> {
    pub cancelled: &'a AtomicBool,
    pub child_exited: &'a AtomicBool,
    pub wakeup: &'a Wakeup,
    pub diagnostics: &'a WorkerDiagnostics,
    pub started_at: Instant,
    pub cleanup: &'a CleanupControl,
    pub cleanup_errors: &'a SyncSender<String>,
}

impl WorkerContext<'_> {
    fn wake(&self) {
        increment(&self.diagnostics.wakeups);
        (self.wakeup)();
    }
}

pub(super) enum Output {
    Started(u32),
    Bytes(Vec<u8>),
    Resized(TerminalSize),
    Exited(ExitStatus),
}

pub(super) fn run(
    command: CommandBuilder,
    size: GridSize,
    commands: (Receiver<Input>, Receiver<TerminalSize>),
    output: &SyncSender<Output>,
    context: &WorkerContext<'_>,
) -> Result<ExitStatus> {
    run_process(PtyProcess::spawn(command, size)?, commands, output, context)
}

fn run_process(
    mut process: PtyProcess,
    commands: (Receiver<Input>, Receiver<TerminalSize>),
    output: &SyncSender<Output>,
    context: &WorkerContext<'_>,
) -> Result<ExitStatus> {
    let result = process.transfer(commands.0, commands.1, output, context);
    loop {
        match process.stop() {
            Ok(status) => {
                context.child_exited.store(true, Ordering::Release);
                context.cleanup.failed.store(false, Ordering::Release);
                return result.map(|_| status);
            }
            Err(error) => {
                if context.cleanup.abandoning.load(Ordering::Acquire) {
                    return Err(error).context("session cleanup failed during drop");
                }
                let message = match &result {
                    Err(cause) => format!("{cause:#}; session cleanup also failed: {error:#}"),
                    Ok(()) => format!("session cleanup failed: {error:#}"),
                };
                context.cleanup.failed.store(true, Ordering::Release);
                let mut message = message;
                loop {
                    match context.cleanup_errors.try_send(message) {
                        Ok(()) => break,
                        Err(TrySendError::Full(pending)) => message = pending,
                        Err(TrySendError::Disconnected(_)) => {
                            bail!("cleanup error receiver disconnected: {error:#}");
                        }
                    }
                    if context.cleanup.abandoning.load(Ordering::Acquire) {
                        return Err(error)
                            .context("session cleanup abandoned before reporting failure");
                    }
                    context.wake();
                    thread::sleep(POLL_INTERVAL);
                }
                context.wake();
                while !context.cleanup.retry.swap(false, Ordering::AcqRel) {
                    if context.cleanup.abandoning.load(Ordering::Acquire) {
                        return Err(error).context("session cleanup abandoned after failure");
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                context.cleanup.failed.store(false, Ordering::Release);
            }
        }
    }
}

struct PtyProcess {
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    exit_status: Option<ExitStatus>,
    foreground: Option<libc::pid_t>,
    session_id: libc::pid_t,
}

impl PtyProcess {
    fn spawn(command: CommandBuilder, size: GridSize) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: size.rows(),
                cols: size.columns(),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open PTY")?;
        let fd = pair
            .master
            .as_raw_fd()
            .context("the PTY backend does not expose a Unix descriptor")?;
        set_nonblocking(fd).context("configure non-blocking PTY I/O")?;
        let reader = pair.master.try_clone_reader().context("open PTY reader")?;
        let writer = pair.master.take_writer().context("open PTY writer")?;
        let child = pair
            .slave
            .spawn_command(command)
            .context("spawn session child")?;
        drop(pair.slave);
        let session_id = child.process_id().context("session child has no PID")? as libc::pid_t;
        Ok(Self {
            child,
            reader,
            writer,
            master: pair.master,
            exit_status: None,
            foreground: None,
            session_id,
        })
    }

    fn transfer(
        &mut self,
        input: Receiver<Input>,
        resize: Receiver<TerminalSize>,
        output: &SyncSender<Output>,
        context: &WorkerContext<'_>,
    ) -> Result<()> {
        let pid = self
            .child
            .process_id()
            .context("session child has no PID")?;
        let fd = self
            .master
            .as_raw_fd()
            .context("PTY descriptor unavailable")?;
        let mut pending_output = Some(Output::Started(pid));
        let mut pending_input = None;
        let mut eof = false;
        let mut finishing = false;
        let mut exited_at = None;
        let mut eof_at = None;

        loop {
            if context.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            if exited_at.is_some_and(|start: Instant| start.elapsed() >= FINAL_DRAIN_TIME) {
                bail!(
                    "final PTY output drain exceeded two seconds; descendant may still hold the PTY or output is backpressured; final output may be incomplete"
                );
            }

            if let Some(event) = pending_output.take() {
                match output.try_send(event) {
                    Ok(()) => {
                        context.wake();
                        if finishing {
                            return Ok(());
                        }
                    }
                    Err(TrySendError::Full(event)) => {
                        increment(&context.diagnostics.backpressure_retries);
                        pending_output = Some(event);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        bail!("session output receiver disconnected")
                    }
                }
            }

            if self.exit_status.is_none() {
                self.exit_status = self.child.try_wait().context("check session child exit")?;
                if self.exit_status.is_some() {
                    exited_at = Some(Instant::now());
                    context.child_exited.store(true, Ordering::Release);
                }
            }

            if pending_output.is_none() && self.exit_status.is_none() {
                match resize.try_recv() {
                    Ok(size) => {
                        self.master.resize(size.pty_size()?).context("resize PTY")?;
                        pending_output = Some(Output::Resized(size));
                        continue;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => bail!("session resize sender disconnected"),
                }
            }

            if self.exit_status.is_none() {
                if pending_input.is_none() {
                    match input.try_recv() {
                        Ok(input) => pending_input = Some(PendingWrite::new(input)),
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => {
                            bail!("session input sender disconnected")
                        }
                    }
                }
                if let Some(write) = pending_input.as_mut()
                    && write.advance(&mut self.writer).context("write PTY input")?
                {
                    if let Some(accepted_at) = write.accepted_at {
                        let delivered_at = Instant::now();
                        let latency = nanoseconds(delivered_at.duration_since(accepted_at));
                        context
                            .diagnostics
                            .last_input_latency_ns
                            .store(latency, Ordering::Relaxed);
                        context
                            .diagnostics
                            .max_input_latency_ns
                            .fetch_max(latency, Ordering::Relaxed);
                        context.diagnostics.last_input_delivered_at_ns.store(
                            nanoseconds(delivered_at.duration_since(context.started_at)),
                            Ordering::Relaxed,
                        );
                        increment(&context.diagnostics.delivered_inputs);
                    }
                    pending_input = None;
                }
            }

            if pending_output.is_none() && !eof {
                let mut bytes = vec![0; CHUNK_BYTES];
                match self.reader.read(&mut bytes) {
                    Ok(0) => {
                        eof = true;
                        eof_at = Some(Instant::now());
                    }
                    Ok(count) => {
                        bytes.truncate(count);
                        pending_output = Some(Output::Bytes(bytes));
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                        // Unix PTY masters can report the closed slave as EIO instead of read(0).
                        eof = true;
                        eof_at = Some(Instant::now());
                    }
                    Err(error) => return Err(error).context("read PTY output"),
                }
            }
            if self.exit_status.is_none()
                && eof_at.is_some_and(|start| start.elapsed() >= EOF_EXIT_GRACE)
            {
                bail!(
                    "PTY reached EOF but the owned child is still running; terminating the session"
                );
            }

            if let Some(status) = self.exit_status.as_ref()
                && eof
                && pending_output.is_none()
            {
                pending_output = Some(Output::Exited(status.clone()));
                finishing = true;
                continue;
            }

            wait_ready(
                fd,
                pending_output.is_none() && !eof,
                pending_input.is_some() && self.exit_status.is_none(),
            )?;
        }
    }

    fn stop(&mut self) -> Result<ExitStatus> {
        cleanup::stop(
            &mut *self.child,
            &*self.master,
            &mut self.exit_status,
            &mut self.foreground,
            self.session_id,
        )
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("Vasterm: session cleanup failed: {error:#}");
            if let Err(error) =
                cleanup::stop_child(&mut *self.child, &mut self.exit_status, self.session_id)
            {
                eprintln!("Vasterm: emergency direct-child cleanup failed: {error:#}");
            }
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // The master owns this descriptor throughout these fcntl calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait_ready(fd: RawFd, read: bool, write: bool) -> Result<()> {
    if !read && !write {
        thread::sleep(POLL_INTERVAL);
        return Ok(());
    }
    let mut descriptor = libc::pollfd {
        fd,
        events: if read { libc::POLLIN } else { 0 } | if write { libc::POLLOUT } else { 0 },
        revents: 0,
    };
    // The descriptor remains owned by the worker, and poll borrows this one-element array.
    let result = unsafe { libc::poll(&mut descriptor, 1, POLL_INTERVAL.as_millis() as i32) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error).context("poll PTY");
        }
    }
    if descriptor.revents & libc::POLLNVAL != 0 {
        bail!("poll reported an invalid PTY descriptor");
    }
    Ok(())
}

struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
    accepted_at: Option<Instant>,
}

impl PendingWrite {
    fn new(input: Input) -> Self {
        debug_assert!(input.bytes.len() <= MAX_INPUT_BYTES);
        Self {
            bytes: input.bytes,
            offset: 0,
            accepted_at: input.accepted_at,
        }
    }

    fn advance(&mut self, writer: &mut impl Write) -> io::Result<bool> {
        match writer.write(&self.bytes[self.offset..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(count) => self.offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
        Ok(self.offset == self.bytes.len())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::Arc;

    struct FailingIo;

    struct RetainedSlave {
        noisy: bool,
    }

    impl Read for RetainedSlave {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.noisy {
                bytes.fill(b'x');
                Ok(bytes.len())
            } else {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
    }

    impl Read for FailingIo {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected read failure"))
        }
    }

    impl Write for FailingIo {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected write failure"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct UnavailableDescriptor {
        master: Option<Box<dyn MasterPty + Send>>,
        unavailable: Arc<AtomicBool>,
    }

    impl UnavailableDescriptor {
        fn master(&self) -> &dyn MasterPty {
            self.master.as_deref().unwrap()
        }
    }

    impl MasterPty for UnavailableDescriptor {
        fn resize(&self, size: PtySize) -> Result<()> {
            self.master().resize(size)
        }
        fn get_size(&self) -> Result<PtySize> {
            self.master().get_size()
        }
        fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
            self.master().try_clone_reader()
        }
        fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
            self.master().take_writer()
        }
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            self.master().process_group_leader()
        }
        fn as_raw_fd(&self) -> Option<RawFd> {
            if self.unavailable.load(Ordering::Acquire) {
                None
            } else {
                self.master().as_raw_fd()
            }
        }
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            self.master().tty_name()
        }
    }

    pub(in crate::session) enum Fault {
        Read,
        Write,
        Descriptor(Arc<AtomicBool>),
        RetainedSlave(bool),
    }

    pub(in crate::session) fn run_with_fault(
        fault: Fault,
        commands: (Receiver<Input>, Receiver<TerminalSize>),
        output: &SyncSender<Output>,
        context: &WorkerContext<'_>,
        pid_sender: SyncSender<u32>,
    ) -> Result<ExitStatus> {
        let command = CommandBuilder::new(if matches!(fault, Fault::RetainedSlave(_)) {
            "/usr/bin/true"
        } else {
            "/bin/cat"
        });
        let mut process = PtyProcess::spawn(command, GridSize::default())?;
        let pid = process.child.process_id().unwrap();
        pid_sender.send(pid).unwrap();
        match fault {
            Fault::Read => process.reader = Box::new(FailingIo),
            Fault::Write => process.writer = Box::new(FailingIo),
            Fault::RetainedSlave(noisy) => process.reader = Box::new(RetainedSlave { noisy }),
            Fault::Descriptor(unavailable) => {
                let master = std::mem::replace(
                    &mut process.master,
                    Box::new(UnavailableDescriptor {
                        master: None,
                        unavailable,
                    }),
                );
                let wrapper: &mut dyn MasterPty = process.master.as_mut();
                wrapper
                    .downcast_mut::<UnavailableDescriptor>()
                    .unwrap()
                    .master = Some(master);
            }
        }
        let result = run_process(process, commands, output, context);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        result
    }

    #[test]
    fn partial_writes_and_backpressure_preserve_bytes() {
        struct Writer {
            calls: usize,
            bytes: Vec<u8>,
        }
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls == 2 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.bytes.push(bytes[0]);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Writer {
            calls: 0,
            bytes: Vec::new(),
        };
        let mut pending = PendingWrite::new(Input {
            bytes: b"abc".to_vec(),
            accepted_at: None,
        });
        assert!(!pending.advance(&mut writer).unwrap());
        assert!(!pending.advance(&mut writer).unwrap());
        assert!(!pending.advance(&mut writer).unwrap());
        assert!(pending.advance(&mut writer).unwrap());
        assert_eq!(writer.bytes, b"abc");
    }

    #[test]
    fn write_failure_is_returned_to_the_worker() {
        let mut pending = PendingWrite::new(Input {
            bytes: b"input".to_vec(),
            accepted_at: None,
        });
        let error = pending
            .advance(&mut io::Cursor::new(&mut [0u8; 0][..]))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    }
}
