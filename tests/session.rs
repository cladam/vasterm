use std::io::{self, BufRead, Read, Write};
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use portable_pty::CommandBuilder;
use vasterm::session::{GridSize, Session, SessionState, TerminalSize};

// Load completion is not a throughput gate on shared CI runners.
const LOAD_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);

fn fixture(scenario: &str) -> Session {
    fixture_with_wakeup(scenario, GridSize::default(), Arc::new(|| {}))
}

fn fixture_with_wakeup(
    scenario: &str,
    size: GridSize,
    wakeup: vasterm::session::Wakeup,
) -> Session {
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args(["--exact", "pty_child_fixture", "--nocapture"]);
    command.env("VASTERM_TEST_SCENARIO", scenario);
    Session::spawn(command, size, wakeup).unwrap()
}

fn wait_on_wakeup(session: &mut Session, wakeups: &Receiver<()>, ready: impl Fn(&Session) -> bool) {
    wait_on_wakeup_with_timeout(session, wakeups, Duration::from_secs(5), ready);
}

fn wait_on_wakeup_with_timeout(
    session: &mut Session,
    wakeups: &Receiver<()>,
    timeout: Duration,
    ready: impl Fn(&Session) -> bool,
) {
    let deadline = Instant::now() + timeout;
    while !ready(session) {
        assert!(
            Instant::now() < deadline,
            "session completion timed out: {}; diagnostics: {:?}",
            session.state(),
            session.diagnostics()
        );
        wakeups
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("no repaint requested for pending session work");
        session.poll();
        assert!(
            !matches!(session.state(), SessionState::Failed(_)),
            "{}",
            session.state()
        );
    }
}

fn repaint_channel() -> (vasterm::session::Wakeup, Receiver<()>) {
    let (wakeup, wakes) = mpsc::sync_channel(1);
    (
        Arc::new(move || {
            match wakeup.try_send(()) {
                // A full slot already schedules a pass, like egui's coalesced repaint requests.
                Ok(()) | Err(mpsc::TrySendError::Full(())) => {}
                Err(mpsc::TrySendError::Disconnected(())) => {
                    panic!("test wake receiver disconnected")
                }
            }
        }),
        wakes,
    )
}

fn wait_for(session: &mut Session, ready: impl Fn(&Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        session.poll();
        if ready(session) {
            return;
        }
        assert!(
            !matches!(session.state(), SessionState::Failed(_)),
            "{}",
            session.state()
        );
        assert!(
            Instant::now() < deadline,
            "timeout: {}; grid: {}",
            session.state(),
            screen(session)
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn screen(session: &Session) -> String {
    session
        .terminal()
        .grid()
        .display_iter()
        .map(|cell| cell.c)
        .collect()
}

#[test]
fn pty_bytes_reach_the_grid_and_binary_input_reaches_the_child() {
    let mut session = fixture("roundtrip");
    wait_for(&mut session, |s| screen(s).contains("READY"));
    let cell = &session.terminal().grid()[Point::new(Line(0), Column(0))];
    assert_eq!(cell.c, 'R');
    assert_eq!(cell.fg, Color::Named(NamedColor::Red));
    session
        .send_input(&[0x00, 0x1b, 0xc3, 0xa9, b'\n'])
        .unwrap();
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(screen(&session).contains("INPUT:001bc3a90a"));
    assert!(matches!(session.state(), SessionState::Exited(status) if status.exit_code() == 7));
}

#[test]
fn emulator_protocol_responses_are_received_by_the_child() {
    let mut session = fixture("queries");
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(screen(&session).contains("QUERIES_OK"));
    assert!(matches!(session.state(), SessionState::Exited(status) if status.success()));
    assert_eq!(
        session.diagnostics().delivered_inputs,
        0,
        "protocol replies are not user inputs"
    );
}

#[test]
fn a_burst_of_queries_reaches_the_child_without_filling_the_input_queue() {
    let mut session = fixture("query-burst");
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(screen(&session).contains("QUERY_BURST_OK"));
    assert!(matches!(session.state(), SessionState::Exited(status) if status.success()));
}

#[test]
fn child_receives_initial_grid_and_truthful_environment() {
    let mut session = fixture("environment");
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(screen(&session).contains("ENVIRONMENT_OK"));
    assert!(matches!(session.state(), SessionState::Exited(status) if status.success()));
}

#[test]
fn resize_agrees_with_kernel_grid_and_emulator_pixel_queries() {
    let mut session = fixture("resize");
    wait_for(&mut session, |s| screen(s).contains("READY"));
    let size = TerminalSize {
        grid: GridSize::new(100, 30).unwrap(),
        cell_width: 8,
        cell_height: 16,
    };
    session.resize(size).unwrap();
    assert_eq!(
        session.size(),
        GridSize::default(),
        "resize is not yet acknowledged"
    );
    wait_for(&mut session, |s| s.size() == size.grid);
    session.send_input(b"x").unwrap();
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(screen(&session).contains("RESIZE_OK"));
    assert!(matches!(session.state(), SessionState::Exited(status) if status.success()));
}

#[test]
fn final_output_and_status_are_preserved_after_the_child_exits() {
    let mut session = fixture("final-output");
    // Let the child exit before the UI consumes its buffered output.
    thread::sleep(Duration::from_millis(150));
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(screen(&session).contains("FINAL_OUTPUT"));
    assert!(matches!(session.state(), SessionState::Exited(status) if status.exit_code() == 9));
}

#[test]
fn unanswered_queries_from_an_exited_child_do_not_corrupt_its_exit_status() {
    let mut session = fixture("late-query");
    thread::sleep(Duration::from_millis(150));
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(matches!(session.state(), SessionState::Exited(status) if status.exit_code() == 9));
}

#[test]
fn startup_failure_becomes_an_actionable_failed_state() {
    let command = CommandBuilder::new("/vasterm-test-does-not-exist/shell");
    let mut session = Session::spawn(command, GridSize::default(), Arc::new(|| {})).unwrap();
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Failed(_))
    });
    assert!(
        matches!(session.state(), SessionState::Failed(message) if message.contains("spawn session child"))
    );
}

#[test]
fn stopping_an_idle_child_reaps_it_and_joins_the_worker() {
    let mut session = fixture("idle");
    wait_for(&mut session, |s| screen(s).contains("READY"));
    let pid = match session.state() {
        SessionState::Running { pid } => *pid,
        state => panic!("unexpected state: {state:?}"),
    };
    let start = Instant::now();
    session.shutdown().unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(matches!(session.state(), SessionState::Closed));
    assert_reaped(pid);
    assert!(session.send_input(b"after close").is_err());
    session.shutdown().unwrap();
}

#[test]
fn dropping_a_live_session_also_reaps_the_child() {
    let mut session = fixture("idle");
    wait_for(&mut session, |s| screen(s).contains("READY"));
    let pid = match session.state() {
        SessionState::Running { pid } => *pid,
        state => panic!("unexpected state: {state:?}"),
    };
    drop(session);
    assert_reaped(pid);
}

#[test]
fn shutdown_escalates_when_the_child_ignores_hangup() {
    let mut session = fixture("ignore-hangup");
    wait_for(&mut session, |s| screen(s).contains("READY"));
    let pid = match session.state() {
        SessionState::Running { pid } => *pid,
        state => panic!("unexpected state: {state:?}"),
    };
    let start = Instant::now();
    session.shutdown().unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_reaped(pid);
}

#[test]
fn output_backpressure_does_not_block_shutdown() {
    let mut session = fixture("flood");
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Running { .. })
    });
    // Deliberately stop consuming output so the bounded queue fills.
    thread::sleep(Duration::from_millis(150));
    let start = Instant::now();
    session.request_shutdown();
    assert!(start.elapsed() < Duration::from_millis(50));
    wait_for(&mut session, Session::cleanup_complete);
    assert!(matches!(session.state(), SessionState::Closed));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn output_backpressure_does_not_prevent_input_delivery() {
    let mut session = fixture("backpressure-input");
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Running { .. })
    });
    let pid = match session.state() {
        SessionState::Running { pid } => *pid,
        state => panic!("unexpected state: {state:?}"),
    };
    thread::sleep(Duration::from_millis(150));
    session.send_input(b"x").unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // Signal 0 only checks this test's exact child PID; do not drain UI output yet.
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result == -1 {
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "input was not delivered under output backpressure"
        );
        thread::sleep(Duration::from_millis(5));
    }
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(matches!(session.state(), SessionState::Exited(status) if status.exit_code() == 12));
    let stats = session.diagnostics();
    assert!(stats.output_backpressure_retries > 0);
    assert_eq!(stats.delivered_inputs, 1);
    assert!(stats.max_input_latency < Duration::from_secs(1));
    assert!(stats.last_input_delivered_at <= stats.elapsed);
}

#[test]
fn saturated_output_preserves_retained_lines_and_update_bounds() {
    assert_saturated_output(GridSize::default());
}

#[test]
fn saturated_output_on_a_larger_grid_preserves_lines_and_parse_bounds() {
    assert_saturated_output(GridSize::new(240, 80).unwrap());
}

fn assert_saturated_output(size: GridSize) {
    let (wakeup, wakes) = repaint_channel();
    let mut session = fixture_with_wakeup("numbered-output", size, wakeup);
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Running { .. })
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    // Do not drain the output queue until the worker proves it is backpressured.
    while session.diagnostics().output_backpressure_retries == 0 {
        assert!(
            Instant::now() < deadline,
            "worker did not reach output backpressure"
        );
        thread::sleep(Duration::from_millis(5));
    }
    wait_on_wakeup_with_timeout(&mut session, &wakes, LOAD_COMPLETION_TIMEOUT, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(matches!(session.state(), SessionState::Exited(status) if status.success()));
    let stats = session.diagnostics();
    assert!(stats.max_update_bytes <= vasterm::session::UPDATE_BYTES);
    assert!(stats.max_parse_bytes <= vasterm::session::PARSE_BYTES);
    assert!(stats.processed_bytes >= 30_000 * 8);
    let grid = session.terminal().grid();
    assert_eq!(grid.history_size(), vasterm::session::SCROLLBACK_LINES);
    let retained_count = grid.total_lines() - 1; // The last row contains the final marker.
    for (offset, number) in (30_000 - retained_count + 1..=30_000).enumerate() {
        let row = Line(offset as i32 - grid.history_size() as i32);
        let text: String = (0..6)
            .map(|column| grid[Point::new(row, Column(column))].c)
            .collect();
        assert_eq!(text, format!("{number:06}"), "retained row {offset}");
    }
    assert!(screen(&session).contains("NUMBERED_DONE"));
    eprintln!(
        "numbered-output {}x{}: {} bytes, max update {} bytes, max poll {:.3} ms, max chunk {:.3} ms, max parse {:.3} ms ({} bytes), {} backpressure retries",
        size.columns(),
        size.rows(),
        stats.processed_bytes,
        stats.max_update_bytes,
        stats.max_poll_duration.as_secs_f64() * 1_000.0,
        stats.max_chunk_duration.as_secs_f64() * 1_000.0,
        stats.max_parse_duration.as_secs_f64() * 1_000.0,
        stats.max_parse_bytes,
        stats.output_backpressure_retries,
    );
}

#[test]
fn idle_worker_and_diagnostic_reads_do_not_schedule_ui_work() {
    let mut session = fixture("idle");
    wait_for(&mut session, |s| screen(s).contains("READY"));
    // Allow the final output publication to finish before comparing wake counters.
    thread::sleep(Duration::from_millis(50));
    session.poll();
    let before = session.diagnostics();
    thread::sleep(Duration::from_millis(150));
    for _ in 0..100 {
        let stats = session.diagnostics();
        assert_eq!(stats.worker_wakeups, before.worker_wakeups);
        assert_eq!(stats.budget_followups, before.budget_followups);
    }
    session.shutdown().unwrap();
}

#[test]
fn output_resize_queries_and_exit_progress_only_on_requested_wakeups() {
    let (wakeup, wakes) = repaint_channel();
    let mut session = fixture_with_wakeup("resize", GridSize::default(), wakeup);
    wait_on_wakeup(&mut session, &wakes, |s| screen(s).contains("READY"));
    let size = TerminalSize {
        grid: GridSize::new(100, 30).unwrap(),
        cell_width: 8,
        cell_height: 16,
    };
    session.resize(size).unwrap();
    wait_on_wakeup(&mut session, &wakes, |s| s.size() == size.grid);
    session.send_input(b"x").unwrap();
    wait_on_wakeup(&mut session, &wakes, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    assert!(screen(&session).contains("RESIZE_OK"));
    assert!(matches!(session.state(), SessionState::Exited(status) if status.success()));
}

#[test]
fn saturated_output_budget_continues_without_a_polling_timer() {
    let (wakeup, wakes) = repaint_channel();
    let mut session = fixture_with_wakeup("numbered-output", GridSize::default(), wakeup);
    // Keep the UI asleep until output backpressure proves a real queued backlog.
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.diagnostics().output_backpressure_retries == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    wait_on_wakeup_with_timeout(&mut session, &wakes, LOAD_COMPLETION_TIMEOUT, |s| {
        matches!(s.state(), SessionState::Exited(_))
    });
    let stats = session.diagnostics();
    assert!(stats.budget_followups > 0);
    assert!(stats.max_parse_bytes <= vasterm::session::PARSE_BYTES);
    assert!(stats.max_update_bytes <= vasterm::session::UPDATE_BYTES);
    assert!(screen(&session).contains("NUMBERED_DONE"));
    assert!(matches!(session.state(), SessionState::Exited(status) if status.success()));
}

#[test]
fn startup_failure_wakes_an_idle_ui() {
    let wakeups = Arc::new(AtomicUsize::new(0));
    let count = wakeups.clone();
    let mut session = Session::spawn(
        CommandBuilder::new("/vasterm-test-does-not-exist/shell"),
        GridSize::default(),
        Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }),
    )
    .unwrap();
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Failed(_))
    });
    assert!(wakeups.load(Ordering::Relaxed) > 0);
}

fn assert_reaped(pid: u32) {
    let mut status = 0;
    // Only inspect the exact child PID created by this test; ECHILD proves it was reaped.
    let result = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
    assert_eq!(result, -1, "child {pid} was not reaped");
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn requested_shutdown_is_nonblocking_idempotent_and_rejects_new_work() {
    let mut session = fixture("ignore-hangup");
    wait_for(&mut session, |s| screen(s).contains("READY"));
    let SessionState::Running { pid } = *session.state() else {
        panic!("{}", session.state())
    };
    let started = Instant::now();
    session.request_shutdown();
    assert!(started.elapsed() < Duration::from_millis(50));
    assert!(matches!(session.state(), SessionState::Closing));
    assert!(!session.cleanup_complete());
    assert!(session.send_input(b"x").is_err());
    assert!(
        session
            .resize(TerminalSize {
                grid: GridSize::default(),
                cell_width: 0,
                cell_height: 0,
            })
            .is_err()
    );
    session.request_shutdown();
    session.poll();
    assert!(!session.cleanup_complete());
    wait_for(&mut session, Session::cleanup_complete);
    assert!(matches!(session.state(), SessionState::Closed));
    assert!(started.elapsed() >= vasterm::session::SHUTDOWN_GRACE);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_reaped(pid);
}

#[test]
fn requested_shutdown_can_cancel_startup() {
    let mut session = fixture("idle");
    session.request_shutdown();
    wait_for(&mut session, Session::cleanup_complete);
    assert!(matches!(session.state(), SessionState::Closed));
}

#[test]
fn requested_shutdown_terminates_the_owned_foreground_job() {
    for scenario in [
        "foreground",
        "foreground-ignore-hup",
        "foreground-leaderless",
    ] {
        let mut session = fixture(scenario);
        wait_for(&mut session, |s| screen(s).contains(":READY"));
        let SessionState::Running { pid } = *session.state() else {
            panic!("{}", session.state())
        };
        let contents = screen(&session);
        let job: i32 = contents
            .split("JOB:")
            .nth(1)
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let group: i32 = contents
            .split("GROUP:")
            .nth(1)
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(job, pid as i32);
        assert_eq!(unsafe { libc::getsid(job) }, pid as i32);
        assert_eq!(unsafe { libc::getpgid(job) }, group);
        if scenario == "foreground-leaderless" {
            assert_ne!(job, group);
            assert_eq!(unsafe { libc::getsid(group) }, -1);
        }
        session.request_shutdown();
        wait_for(&mut session, Session::cleanup_complete);
        assert!(matches!(session.state(), SessionState::Closed));
        assert_reaped(pid);
        assert_eq!(
            unsafe { libc::kill(job, 0) },
            -1,
            "foreground job remains alive"
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}

#[test]
fn eof_with_a_live_child_is_an_error_and_reaps_the_child() {
    let mut session = fixture("live-eof");
    wait_for(&mut session, |s| screen(s).contains("READY"));
    let SessionState::Running { pid } = *session.state() else {
        panic!("{}", session.state())
    };
    wait_for(&mut session, |s| {
        matches!(s.state(), SessionState::Failed(_))
    });
    assert!(matches!(session.state(), SessionState::Failed(message)
        if message.contains("PTY reached EOF") && message.contains("still running")));
    assert!(session.cleanup_complete());
    assert_reaped(pid);
}

static HANGUP_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn record_hangup(_: libc::c_int) {
    HANGUP_RECEIVED.store(true, Ordering::Relaxed);
}

fn spawn_foreground_job(group: i32, ignore_hangup: bool) -> std::process::Child {
    let mut job = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "pty_child_fixture", "--nocapture"])
        .env("VASTERM_TEST_SCENARIO", "foreground-job")
        .env(
            "VASTERM_TEST_IGNORE_HUP",
            if ignore_hangup { "1" } else { "0" },
        )
        .process_group(group)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut job_output = io::BufReader::new(job.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("JOB_READY") {
        line.clear();
        assert_ne!(job_output.read_line(&mut line).unwrap(), 0);
    }
    job
}

struct DescendantGuard {
    pid: i32,
    session: i32,
}

impl Drop for DescendantGuard {
    fn drop(&mut self) {
        if unsafe { libc::getsid(self.pid) } == self.session
            && unsafe { libc::getpgid(self.pid) } == self.pid
        {
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
            }
        }
    }
}

#[test]
fn descendants_retaining_slave_descriptors_do_not_prevent_bounded_completion() {
    for scenario in ["held-descendant-parent", "noisy-descendant-parent"] {
        let started = Instant::now();
        let mut session = fixture(scenario);
        wait_for(&mut session, |s| screen(s).contains(":READY"));
        let contents = screen(&session);
        let descendant: i32 = contents
            .split("DESC:")
            .nth(1)
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let root: u32 = contents
            .split("ROOT:")
            .nth(1)
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let guard = DescendantGuard {
            pid: descendant,
            session: root as i32,
        };
        assert_eq!(unsafe { libc::getsid(descendant) }, root as i32);
        // Stop UI consumption to include the backpressured case.
        if scenario == "noisy-descendant-parent" {
            thread::sleep(Duration::from_millis(2_200));
        }
        wait_for(&mut session, |s| {
            matches!(s.state(), SessionState::Failed(_) | SessionState::Exited(_))
        });
        match session.state() {
            SessionState::Failed(error) => {
                assert!(
                    error.contains("drain exceeded two seconds") && error.contains("incomplete")
                );
                assert!(started.elapsed() >= Duration::from_secs(2));
            }
            // macOS can revoke the slave on leader exit despite retained descriptors.
            SessionState::Exited(status) => assert!(status.success()),
            state => panic!("{state}"),
        }
        assert!(started.elapsed() < Duration::from_millis(3_500));
        wait_for(&mut session, Session::cleanup_complete);
        assert!(session.cleanup_complete());
        assert_reaped(root);
        // This is a background, not foreground, group. The test owns its cleanup;
        // Vasterm neither promises to terminate it nor can reap this grandchild.
        drop(guard);
    }
}

// Spawn this test executable as the PTY child, without introducing a shipped fixture binary.
#[test]
fn pty_child_fixture() {
    let Ok(scenario) = std::env::var("VASTERM_TEST_SCENARIO") else {
        return;
    };
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    // stdin belongs to the PTY slave; initialize termios before switching it to raw mode.
    unsafe {
        assert_eq!(libc::tcgetattr(0, settings.as_mut_ptr()), 0);
        let mut settings = settings.assume_init();
        libc::cfmakeraw(&mut settings);
        assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &settings), 0);
    }
    let mut stdout = io::stdout();
    stdout.write_all(b"\x1b[2J\x1b[H").unwrap();
    match scenario.as_str() {
        "roundtrip" => {
            stdout.write_all(b"\x1b[31mREADY\x1b[0m\r\n").unwrap();
            stdout.flush().unwrap();
            let mut input = [0; 5];
            io::stdin().read_exact(&mut input).unwrap();
            write!(stdout, "INPUT:{}", hex(&input)).unwrap();
            stdout.flush().unwrap();
            std::process::exit(7);
        }
        "queries" => {
            for (query, expected) in [
                (&b"\x1b[4;7H\x1b[6n"[..], &b"\x1b[4;7R"[..]),
                (&b"\x1b[18t"[..], &b"\x1b[8;24;80t"[..]),
                (
                    &b"\x1b]4;1;?\x07"[..],
                    &b"\x1b]4;1;rgb:cdcd/0000/0000\x07"[..],
                ),
                (&b"\x1b[14t"[..], &b"\x1b[4;0;0t"[..]),
            ] {
                stdout.write_all(query).unwrap();
                stdout.flush().unwrap();
                let mut response = vec![0; expected.len()];
                io::stdin().read_exact(&mut response).unwrap();
                assert_eq!(response, expected);
            }
            stdout.write_all(b"\r\nQUERIES_OK").unwrap();
        }
        "environment" => {
            assert_eq!(std::env::var("TERM").unwrap(), "xterm-256color");
            assert_eq!(std::env::var("COLORTERM").unwrap(), "truecolor");
            assert_eq!(std::env::var("TERM_PROGRAM").unwrap(), "vasterm");
            let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
            // The owned stdin slave supports querying the initial kernel window size.
            unsafe {
                assert_eq!(libc::ioctl(0, libc::TIOCGWINSZ, size.as_mut_ptr()), 0);
                let size = size.assume_init();
                assert_eq!((size.ws_col, size.ws_row), (80, 24));
            }
            stdout.write_all(b"ENVIRONMENT_OK").unwrap();
        }
        "resize" => {
            stdout.write_all(b"READY").unwrap();
            stdout.flush().unwrap();
            let mut input = [0];
            io::stdin().read_exact(&mut input).unwrap();
            assert_eq!(input, *b"x");
            let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
            // Query this fixture's owned PTY slave after the UI-side acknowledgment.
            unsafe {
                assert_eq!(libc::ioctl(0, libc::TIOCGWINSZ, size.as_mut_ptr()), 0);
                let size = size.assume_init();
                assert_eq!((size.ws_col, size.ws_row), (100, 30));
                assert_eq!((size.ws_xpixel, size.ws_ypixel), (800, 480));
            }
            stdout.write_all(b"\x1b[18t\x1b[14t").unwrap();
            stdout.flush().unwrap();
            let expected = b"\x1b[8;30;100t\x1b[4;480;800t";
            let mut response = vec![0; expected.len()];
            io::stdin().read_exact(&mut response).unwrap();
            assert_eq!(response, expected);
            stdout.write_all(b"\r\nRESIZE_OK").unwrap();
        }
        "query-burst" => {
            stdout.write_all(&b"\x1b[6n".repeat(1_000)).unwrap();
            stdout.flush().unwrap();
            let expected = b"\x1b[1;1R".repeat(1_000);
            let mut response = vec![0; expected.len()];
            io::stdin().read_exact(&mut response).unwrap();
            assert_eq!(response, expected);
            stdout.write_all(b"\r\nQUERY_BURST_OK").unwrap();
        }
        "final-output" => {
            for _ in 0..100 {
                stdout.write_all(&[b'x'; 2000]).unwrap();
            }
            stdout.write_all(b"\r\nFINAL_OUTPUT").unwrap();
            stdout.flush().unwrap();
            std::process::exit(9);
        }
        "numbered-output" => {
            for number in 1..=30_000 {
                write!(stdout, "{number:06}\r\n").unwrap();
            }
            stdout.write_all(b"NUMBERED_DONE").unwrap();
        }
        "late-query" => {
            stdout.write_all(b"\x1b[6n").unwrap();
            stdout.flush().unwrap();
            std::process::exit(9);
        }
        "idle" | "ignore-hangup" => {
            if scenario == "ignore-hangup" {
                // Change only this fixture child's signal disposition.
                unsafe {
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                }
            }
            stdout.write_all(b"READY").unwrap();
            stdout.flush().unwrap();
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        "live-eof" => {
            stdout.write_all(b"READY").unwrap();
            stdout.flush().unwrap();
            thread::sleep(Duration::from_millis(100));
            unsafe {
                for fd in 0..=2 {
                    assert_eq!(libc::close(fd), 0);
                }
            }
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        "foreground" | "foreground-ignore-hup" | "foreground-leaderless" => {
            unsafe {
                libc::signal(
                    libc::SIGHUP,
                    record_hangup as *const () as libc::sighandler_t,
                );
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            }
            let mut job = spawn_foreground_job(0, scenario != "foreground");
            let group = job.id() as i32;
            unsafe {
                assert_eq!(libc::tcsetpgrp(0, group), 0);
            }
            if scenario == "foreground-leaderless" {
                let member = spawn_foreground_job(group, true);
                job.kill().unwrap();
                job.wait().unwrap();
                job = member;
            }
            write!(stdout, "GROUP:{group}:JOB:{}:READY", job.id()).unwrap();
            stdout.flush().unwrap();
            loop {
                if HANGUP_RECEIVED.load(Ordering::Relaxed) && job.try_wait().unwrap().is_some() {
                    std::process::exit(0);
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
        "foreground-job" => {
            unsafe {
                libc::signal(
                    libc::SIGHUP,
                    if std::env::var("VASTERM_TEST_IGNORE_HUP").unwrap() == "1" {
                        libc::SIG_IGN
                    } else {
                        libc::SIG_DFL
                    },
                );
            }
            stdout.write_all(b"JOB_READY\n").unwrap();
            stdout.flush().unwrap();
            // A bounded fixture lifetime prevents an orphan after an assertion failure.
            thread::sleep(Duration::from_secs(5));
        }
        "held-descendant-parent" | "noisy-descendant-parent" => {
            unsafe {
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            }
            let mut descendant = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "pty_child_fixture", "--nocapture"])
                .env("VASTERM_TEST_SCENARIO", "bounded-descendant")
                .env(
                    "VASTERM_TEST_NOISY",
                    if scenario == "noisy-descendant-parent" {
                        "1"
                    } else {
                        "0"
                    },
                )
                .stdout(std::process::Stdio::piped())
                .process_group(0)
                .spawn()
                .unwrap();
            let mut ready = io::BufReader::new(descendant.stdout.take().unwrap());
            let mut line = String::new();
            while !line.contains("DESC_READY") {
                line.clear();
                assert_ne!(ready.read_line(&mut line).unwrap(), 0);
            }
            write!(
                stdout,
                "DESC:{}:ROOT:{}:READY",
                descendant.id(),
                std::process::id()
            )
            .unwrap();
            stdout.flush().unwrap();
            std::process::exit(0);
        }
        "bounded-descendant" => {
            stdout.write_all(b"DESC_READY\n").unwrap();
            stdout.flush().unwrap();
            thread::sleep(Duration::from_millis(150));
            let deadline = Instant::now() + Duration::from_secs(4);
            let noisy = std::env::var("VASTERM_TEST_NOISY").unwrap() == "1";
            while Instant::now() < deadline {
                if noisy && io::stderr().write_all(&[b'x'; 4096]).is_err() {
                    std::process::exit(0);
                }
                thread::sleep(Duration::from_millis(5));
            }
            std::process::exit(0);
        }
        "flood" => {
            let bytes = [b'x'; 4096];
            loop {
                stdout.write_all(&bytes).unwrap();
                stdout.flush().unwrap();
            }
        }
        "backpressure-input" => {
            thread::spawn(|| {
                let mut stdout = io::stdout();
                loop {
                    stdout.write_all(&[b'x'; 4096]).unwrap();
                    stdout.flush().unwrap();
                }
            });
            let mut input = [0];
            io::stdin().read_exact(&mut input).unwrap();
            assert_eq!(input, *b"x");
            std::process::exit(12);
        }
        _ => panic!("unknown test scenario: {scenario}"),
    }
    stdout.flush().unwrap();
    std::process::exit(0);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
