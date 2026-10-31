use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use portable_pty::CommandBuilder;
use vasterm::session::{GridSize, Session, SessionState, TerminalSize};

fn fixture(scenario: &str) -> Session {
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args(["--exact", "pty_child_fixture", "--nocapture"]);
    command.env("VASTERM_TEST_SCENARIO", scenario);
    Session::spawn(command, GridSize::default(), Arc::new(|| {})).unwrap()
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
    session.shutdown().unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(2);
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
