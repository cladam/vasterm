use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use portable_pty::{Child, ExitStatus, MasterPty};

pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);
const CHECK_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Default)]
pub(super) struct CleanupControl {
    pub retry: AtomicBool,
    pub failed: AtomicBool,
    pub abandoning: AtomicBool,
}

impl CleanupControl {
    pub fn request_retry(&self) {
        if self.failed.load(Ordering::Acquire) {
            self.retry.store(true, Ordering::Release);
        }
    }
}

pub(super) fn stop(
    child: &mut dyn Child,
    master: &dyn MasterPty,
    status: &mut Option<ExitStatus>,
    foreground: &mut Option<libc::pid_t>,
    session_id: libc::pid_t,
) -> Result<ExitStatus> {
    if status.is_none() {
        *status = child.try_wait().context("check child before cleanup")?;
    }
    if let Some(status) = status.as_ref()
        && foreground.is_none()
    {
        return Ok(status.clone());
    }
    if foreground.is_none() {
        let group = master.process_group_leader();
        if let Some(group) = group {
            validate_group(master, group, session_id)?;
            *foreground = Some(group);
        }
    }

    if let Some(group) = *foreground {
        signal_group(master, group, session_id, libc::SIGHUP)?;
    }
    if status.is_none() && *foreground != Some(session_id) {
        signal_child(session_id, libc::SIGHUP)?;
    }
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        if status.is_none() {
            *status = child.try_wait().context("reap child during SIGHUP grace")?;
        }
        if let Some(status) = status.as_ref()
            && !foreground_alive(*foreground)?
        {
            *foreground = None;
            return Ok(status.clone());
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(CHECK_INTERVAL);
    }

    if let Some(group) = *foreground {
        signal_group(master, group, session_id, libc::SIGKILL)?;
        // Give a HUP-aware shell a chance to reap its now-terminated foreground job.
        thread::sleep(CHECK_INTERVAL);
    }
    if status.is_none() {
        *status = child
            .try_wait()
            .context("check child before forced termination")?;
        if status.is_none() {
            signal_child(session_id, libc::SIGKILL)?;
        }
    }
    let deadline = Instant::now() + REAP_TIMEOUT;
    loop {
        if status.is_none() {
            *status = child.try_wait().context("reap forcibly terminated child")?;
        }
        if let Some(status) = status.as_ref()
            && !foreground_alive(*foreground)?
        {
            *foreground = None;
            return Ok(status.clone());
        }
        if Instant::now() >= deadline {
            bail!(
                "cleanup timed out: owned child or foreground group has not terminated; retry cleanup"
            );
        }
        thread::sleep(CHECK_INTERVAL);
    }
}

fn signal_child(pid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    ensure!(
        pid > 1 && pid != unsafe { libc::getpid() },
        "refusing an unsafe child PID"
    );
    // The direct child has not been reaped, so its PID cannot be reused before this signal.
    let result = unsafe { libc::kill(pid, signal) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("signal owned session child");
        }
    }
    Ok(())
}

pub(super) fn stop_child(
    child: &mut dyn Child,
    status: &mut Option<ExitStatus>,
    pid: libc::pid_t,
) -> Result<()> {
    if status.is_none() {
        *status = child
            .try_wait()
            .context("check child during emergency cleanup")?;
    }
    if status.is_some() {
        return Ok(());
    }
    // Never signal an unvalidated group in the Drop safety net.
    signal_child(pid, libc::SIGKILL)?;
    let deadline = Instant::now() + REAP_TIMEOUT;
    loop {
        *status = child
            .try_wait()
            .context("reap child during emergency cleanup")?;
        if status.is_some() {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "emergency direct-child reaping timed out"
        );
        thread::sleep(CHECK_INTERVAL);
    }
}

fn group_alive(group: libc::pid_t) -> Result<bool> {
    ensure!(
        group > 1 && group != unsafe { libc::getpgrp() },
        "refusing an unsafe foreground group"
    );
    // Signal zero only inspects the saved, session-owned group.
    if unsafe { libc::kill(-group, 0) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error).context("inspect foreground process group")
    }
}

fn foreground_alive(group: Option<libc::pid_t>) -> Result<bool> {
    group
        .map(group_alive)
        .transpose()
        .map(|alive| alive.unwrap_or(false))
}

fn validate_group(
    master: &dyn MasterPty,
    group: libc::pid_t,
    session_id: libc::pid_t,
) -> Result<()> {
    ensure!(
        group > 1 && group != unsafe { libc::getpgrp() },
        "refusing an unsafe foreground group"
    );
    let fd = master
        .as_raw_fd()
        .context("PTY descriptor unavailable during cleanup")?;
    // tcsetpgrp only permits groups in the controlling terminal's session.
    let tty_session = unsafe { libc::tcgetsid(fd) };
    let leader_session = unsafe { libc::getsid(group) };
    ensure!(
        (tty_session == session_id && master.process_group_leader() == Some(group))
            || (leader_session == session_id && unsafe { libc::getpgid(group) } == group),
        "foreground group {group} cannot be validated against owned session {session_id}; refusing to signal it"
    );
    Ok(())
}

fn signal_group(
    master: &dyn MasterPty,
    group: libc::pid_t,
    session_id: libc::pid_t,
    signal: libc::c_int,
) -> Result<()> {
    if !group_alive(group)? {
        return Ok(());
    }
    validate_group(master, group, session_id)?;
    // Only the captured group is targeted; ownership is revalidated before escalation/retry.
    if unsafe { libc::kill(-group, signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("signal verified foreground group");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_process_identifiers_are_rejected_without_signaling() {
        for pid in [0, 1, unsafe { libc::getpid() }] {
            assert!(signal_child(pid, libc::SIGHUP).is_err());
        }
        for group in [0, 1, unsafe { libc::getpgrp() }] {
            assert!(group_alive(group).is_err());
        }
    }
}
