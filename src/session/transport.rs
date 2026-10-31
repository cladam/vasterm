use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use super::{GridSize, MAX_INPUT_BYTES, TerminalSize, Wakeup};

pub(super) const CHUNK_BYTES: usize = 4_096;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(super) enum Output {
    Started(u32),
    Bytes(Vec<u8>),
    Resized(TerminalSize),
    Exited(ExitStatus),
}

pub(super) fn run(
    command: CommandBuilder,
    size: GridSize,
    commands: (Receiver<Vec<u8>>, Receiver<TerminalSize>),
    output: &SyncSender<Output>,
    cancelled: &AtomicBool,
    child_exited: &AtomicBool,
    wakeup: &Wakeup,
) -> Result<ExitStatus> {
    let mut process = PtyProcess::spawn(command, size)?;
    let result = process.transfer(
        commands.0,
        commands.1,
        output,
        cancelled,
        child_exited,
        wakeup,
    );
    let cleanup = process.stop();
    if cleanup.is_ok() {
        child_exited.store(true, Ordering::Release);
    }
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("session cleanup also failed: {cleanup:#}")))
        }
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), status) => status,
    }
}

struct PtyProcess {
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    exit_status: Option<ExitStatus>,
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
        Ok(Self {
            child,
            reader,
            writer,
            master: pair.master,
            exit_status: None,
        })
    }

    fn transfer(
        &mut self,
        input: Receiver<Vec<u8>>,
        resize: Receiver<TerminalSize>,
        output: &SyncSender<Output>,
        cancelled: &AtomicBool,
        child_exited: &AtomicBool,
        wakeup: &Wakeup,
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

        loop {
            if cancelled.load(Ordering::Acquire) {
                return Ok(());
            }

            if let Some(event) = pending_output.take() {
                match output.try_send(event) {
                    Ok(()) => {
                        wakeup();
                        if finishing {
                            return Ok(());
                        }
                    }
                    Err(TrySendError::Full(event)) => pending_output = Some(event),
                    Err(TrySendError::Disconnected(_)) => {
                        bail!("session output receiver disconnected")
                    }
                }
            }

            if self.exit_status.is_none() {
                self.exit_status = self.child.try_wait().context("check session child exit")?;
                if self.exit_status.is_some() {
                    child_exited.store(true, Ordering::Release);
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
                        Ok(bytes) => pending_input = Some(PendingWrite::new(bytes)),
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => {
                            bail!("session input sender disconnected")
                        }
                    }
                }
                if let Some(write) = pending_input.as_mut()
                    && write.advance(&mut self.writer).context("write PTY input")?
                {
                    pending_input = None;
                }
            }

            if pending_output.is_none() && !eof {
                let mut bytes = vec![0; CHUNK_BYTES];
                match self.reader.read(&mut bytes) {
                    Ok(0) => eof = true,
                    Ok(count) => {
                        bytes.truncate(count);
                        pending_output = Some(Output::Bytes(bytes));
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        // Do not wait forever for a descendant holding the slave after shell exit.
                        eof = self.exit_status.is_some();
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("read PTY output"),
                }
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
        if let Some(status) = self.exit_status.as_ref() {
            return Ok(status.clone());
        }
        if let Some(status) = self
            .child
            .try_wait()
            .context("check child before shutdown")?
        {
            self.exit_status = Some(status.clone());
            return Ok(status);
        }
        if let Err(error) = self.child.kill() {
            if let Some(status) = self
                .child
                .try_wait()
                .context("check child after termination race")?
            {
                self.exit_status = Some(status.clone());
                return Ok(status);
            }
            return Err(error).context("terminate session child");
        }
        let status = self.child.wait().context("reap session child")?;
        self.exit_status = Some(status.clone());
        Ok(status)
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("Vasterm: session cleanup failed: {error:#}");
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
}

impl PendingWrite {
    fn new(bytes: Vec<u8>) -> Self {
        debug_assert!(bytes.len() <= MAX_INPUT_BYTES);
        Self { bytes, offset: 0 }
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
mod tests {
    use super::*;

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
        let mut pending = PendingWrite::new(b"abc".to_vec());
        assert!(!pending.advance(&mut writer).unwrap());
        assert!(!pending.advance(&mut writer).unwrap());
        assert!(!pending.advance(&mut writer).unwrap());
        assert!(pending.advance(&mut writer).unwrap());
        assert_eq!(writer.bytes, b"abc");
    }

    #[test]
    fn write_failure_is_returned_to_the_worker() {
        let mut pending = PendingWrite::new(b"input".to_vec());
        let error = pending
            .advance(&mut io::Cursor::new(&mut [0u8; 0][..]))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    }
}
