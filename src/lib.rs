mod input;
pub mod session;
mod surface;
pub mod workspace;

use std::time::{Duration, Instant};

use eframe::egui;

pub use workspace::{Pane, PaneId, Workspace};

use session::{
    MAX_INPUT_BYTES, PARSE_BYTES, QUEUE_CHUNKS, SCROLLBACK_LINES, Session, UPDATE_BYTES,
    UPDATE_TIME,
};

pub struct VastermApp {
    pub workspace: Workspace,
    pub interaction_error: Option<String>,
    diagnostics: UiDiagnostics,
    pub closing_window: bool,
}

#[derive(Default)]
struct UiDiagnostics {
    logic_passes: u64,
    ui_passes: u64,
    max_logic_duration: Duration,
    max_ui_duration: Duration,
}

impl VastermApp {
    pub fn new(context: &egui::Context) -> Self {
        Self {
            workspace: Workspace::two_panes(context),
            interaction_error: None,
            diagnostics: UiDiagnostics::default(),
            closing_window: false,
        }
    }

    pub fn from_workspace(workspace: Workspace) -> Self {
        Self {
            workspace,
            interaction_error: None,
            diagnostics: UiDiagnostics::default(),
            closing_window: false,
        }
    }

    pub fn with_single_session(session: Session, closing_window: bool) -> Self {
        Self {
            workspace: Workspace::from_panes(vec![Pane::with_session(PaneId(0), session)]),
            interaction_error: None,
            diagnostics: UiDiagnostics::default(),
            closing_window,
        }
    }

    pub fn with_startup_error(error: String) -> Self {
        Self {
            workspace: Workspace::from_panes(vec![Pane::with_error(PaneId(0), error)]),
            interaction_error: None,
            diagnostics: UiDiagnostics::default(),
            closing_window: false,
        }
    }

    fn handle_window_close(&mut self, context: &egui::Context) {
        if context.input(|input| input.viewport().close_requested()) {
            if self.workspace.all_cleanups_complete() && !self.closing_window {
                return;
            }
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.closing_window {
                self.workspace.request_shutdown_all();
                self.closing_window = true;
            }
        }
        if self.closing_window {
            if let Some(failed_msg) = self.workspace.any_cleanup_failed() {
                self.interaction_error = Some(format!(
                    "Window remains open: {failed_msg}. Retry cleanup if offered, or close again once cleanup is complete."
                ));
                self.closing_window = false;
                context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            } else if self.workspace.all_cleanups_complete() {
                self.closing_window = false;
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        self.handle_window_close(ui.ctx());
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.interaction_error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
                if ui.button("Dismiss error").clicked() {
                    self.interaction_error = None;
                }
            }
            if !self.workspace.panes.is_empty() {
                ui.collapsing("Load diagnostics", |ui| {
                    ui.label(format!(
                        "Logic passes: {} | UI passes: {} | Max logic/UI: {:.3}/{:.3} ms",
                        self.diagnostics.logic_passes,
                        self.diagnostics.ui_passes,
                        self.diagnostics.max_logic_duration.as_secs_f64() * 1_000.0,
                        self.diagnostics.max_ui_duration.as_secs_f64() * 1_000.0,
                    ));
                    ui.label(format!(
                        "Input queue: {} x {} bytes | History: {} lines | Update: {} bytes / soft {} ms | Parse slice: {} bytes",
                        QUEUE_CHUNKS, MAX_INPUT_BYTES, SCROLLBACK_LINES,
                        UPDATE_BYTES, UPDATE_TIME.as_millis(), PARSE_BYTES,
                    ));
                    ui.label("Passive counters; no repaint timer. PTY delivery is not proof of child receipt.");
                    for (i, pane) in self.workspace.panes.iter().enumerate() {
                        if let Some(session) = &pane.session {
                            ui.separator();
                            ui.label(format!("Pane {}:", i + 1));
                            let stats = session.diagnostics();
                            ui.label(format!(
                                "Worker wakes: {} | Budget follow-ups: {} | Backpressure retries: {}",
                                stats.worker_wakeups, stats.budget_followups, stats.output_backpressure_retries,
                            ));
                            ui.label(format!(
                                "Processed: {} bytes | Last/max update: {}/{} bytes",
                                stats.processed_bytes, stats.last_update_bytes, stats.max_update_bytes,
                            ));
                            ui.label(format!(
                                "Max poll/chunk/parse slice: {:.3}/{:.3}/{:.3} ms",
                                stats.max_poll_duration.as_secs_f64() * 1_000.0,
                                stats.max_chunk_duration.as_secs_f64() * 1_000.0,
                                stats.max_parse_duration.as_secs_f64() * 1_000.0,
                            ));
                            ui.label(format!(
                                "Delivered user inputs: {} | Last/max delivery latency: {:.3}/{:.3} ms | Last delivery: {:.3} s after session start",
                                stats.delivered_inputs,
                                stats.last_input_latency.as_secs_f64() * 1_000.0,
                                stats.max_input_latency.as_secs_f64() * 1_000.0,
                                stats.last_input_delivered_at.as_secs_f64(),
                            ));
                        }
                    }
                });
            }
            ui.separator();
            self.workspace.show(ui);
        });
    }
}

impl eframe::App for VastermApp {
    fn logic(&mut self, _context: &egui::Context, _frame: &mut eframe::Frame) {
        let start = Instant::now();
        self.diagnostics.logic_passes = self.diagnostics.logic_passes.saturating_add(1);
        self.workspace.poll();
        self.diagnostics.max_logic_duration =
            self.diagnostics.max_logic_duration.max(start.elapsed());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let start = Instant::now();
        self.diagnostics.ui_passes = self.diagnostics.ui_passes.saturating_add(1);
        self.show(ui);
        self.diagnostics.max_ui_duration = self.diagnostics.max_ui_duration.max(start.elapsed());
    }

    fn on_exit(&mut self) {
        self.workspace.request_shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{GridSize, SessionState};
    use std::sync::Arc;
    use std::thread;

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
                "timed out: {}; grid: {:?}",
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
    fn focused_surface_delivers_text_ime_and_mode_aware_keys_to_the_child() {
        use egui::{Event, ImeEvent, Key, Modifiers};
        use portable_pty::CommandBuilder;
        let expected = b"a\xc3\xa9\x03\x1bOA\r\x7f\t\x1b\x1bOH\x1bOF\x1bb\x1bf\x04";
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            &format!(
                "stty raw -echo; printf '\\033[2J\\033[H\\033[?1hREADY'; \
             received=$(dd bs=1 count={} 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
             printf '\\r\\nINPUT:%s' \"$received\"",
                expected.len()
            ),
        ]);
        let mut session = Session::spawn(command, GridSize::default(), Arc::new(|| {})).unwrap();
        wait_for(&mut session, |s| screen(s).contains("READY"));
        let context = egui::Context::default();
        let mut surface = surface::TerminalSurface::default();
        let mut frame = |session: &mut Session, events: Vec<Event>, focused| {
            let raw = egui::RawInput {
                events,
                focused,
                ..Default::default()
            };
            let mut output = context.run_ui(raw, |ui| {
                surface
                    .show(ui, session, egui::Id::unique("terminal-surface"), true)
                    .unwrap()
            });
            output.textures_delta.clear();
        };
        frame(&mut session, vec![], true);
        frame(
            &mut session,
            vec![Event::Text("must not leak".into())],
            false,
        );
        let terminal_id = context.memory(|memory| memory.focused()).unwrap();
        context.memory_mut(|memory| memory.request_focus(egui::Id::unique("another-widget")));
        frame(
            &mut session,
            vec![Event::Text("nor should this".into())],
            true,
        );
        context.memory_mut(|memory| memory.request_focus(terminal_id));
        frame(&mut session, vec![], true);
        let key = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        frame(
            &mut session,
            vec![
                key(Key::A, Modifiers::NONE),
                Event::Text("a".into()),
                Event::Ime(ImeEvent::Preedit {
                    text: "candidate".into(),
                    active_range_chars: None,
                }),
                Event::Ime(ImeEvent::Commit("é".into())),
                key(Key::C, Modifiers::CTRL),
                key(Key::ArrowUp, Modifiers::NONE),
                key(Key::Enter, Modifiers::NONE),
                key(Key::Backspace, Modifiers::NONE),
                key(Key::Tab, Modifiers::NONE),
                key(Key::Escape, Modifiers::NONE),
                key(Key::Home, Modifiers::NONE),
                key(Key::End, Modifiers::NONE),
                key(Key::AltLeft, Modifiers::ALT),
                key(Key::B, Modifiers::ALT),
                Event::Text("b".into()),
                key(Key::F, Modifiers::ALT),
                Event::Text("f".into()),
                Event::Key {
                    key: Key::AltLeft,
                    physical_key: Some(Key::AltLeft),
                    pressed: false,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
                key(Key::D, Modifiers::CTRL),
            ],
            true,
        );
        wait_for(&mut session, |s| {
            matches!(s.state(), SessionState::Exited(_))
        });
        let hex: String = expected.iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(
            screen(&session).contains(&format!("INPUT:{hex}")),
            "{}",
            screen(&session)
        );
        assert!(matches!(session.state(), SessionState::Exited(status) if status.success()));
    }

    #[test]
    fn load_diagnostics_are_available_without_an_interaction_error() {
        let context = egui::Context::default();
        let mut session = Session::spawn(
            portable_pty::CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        wait_for(&mut session, |session| {
            matches!(session.state(), SessionState::Running { .. })
        });
        let mut app = VastermApp::with_single_session(session, false);
        let mut output = context.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(output.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Text(text)
                if text.galley.text() == "Load diagnostics")
        }));
        app.workspace.panes[0]
            .session
            .as_mut()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn startup_failure_is_visible_in_the_ui() {
        let context = egui::Context::default();
        let mut app = VastermApp::with_startup_error("test shell could not start".into());
        let mut output = context.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        // This headless test inspects paint output without uploading textures.
        output.textures_delta.clear();

        assert!(output.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Text(text)
                if text.galley.text() == "Session failed: test shell could not start")
        }));
    }

    fn close_input() -> egui::RawInput {
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        input
    }

    #[test]
    fn native_close_is_cancelled_until_worker_cleanup_finishes() {
        let context = egui::Context::default();
        let session = Session::spawn(
            portable_pty::CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let mut app = VastermApp::with_single_session(session, false);
        let mut output = context.run_ui(close_input(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        assert!(app.closing_window);
        wait_for(
            app.workspace.panes[0].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
        let mut output = context.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
        assert!(!app.closing_window);
    }

    #[test]
    fn native_close_keeps_a_failed_session_visible_until_a_second_close_request() {
        let context = egui::Context::default();
        let session = Session::spawn(
            portable_pty::CommandBuilder::new("/vasterm-test-does-not-exist/shell"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let mut app = VastermApp::with_single_session(session, false);
        let mut output = context.run_ui(close_input(), |ui| app.show(ui));
        output.textures_delta.clear();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.workspace.panes[0]
            .session
            .as_ref()
            .unwrap()
            .cleanup_complete()
        {
            app.workspace.panes[0].session.as_mut().unwrap().poll();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let mut output = context.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        assert!(
            !output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text)
            if text.galley.text().contains("Window remains open:") && text.galley.text().contains("spawn session child"))));
        let mut output = context.run_ui(close_input(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(
            !output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
    }

    #[test]
    fn two_panes_spawn_and_run_independently() {
        let context = egui::Context::default();
        let session1 = Session::spawn(
            portable_pty::CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let session2 = Session::spawn(
            portable_pty::CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let mut app = VastermApp::from_workspace(Workspace::from_panes(vec![
            Pane::with_session(PaneId(0), session1),
            Pane::with_session(PaneId(1), session2),
        ]));
        wait_for(app.workspace.panes[0].session.as_mut().unwrap(), |s| {
            matches!(s.state(), SessionState::Running { .. })
        });
        wait_for(app.workspace.panes[1].session.as_mut().unwrap(), |s| {
            matches!(s.state(), SessionState::Running { .. })
        });
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = context.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();
        assert_eq!(app.workspace.panes.len(), 2);
        assert!(matches!(
            app.workspace.panes[0].session.as_ref().unwrap().state(),
            SessionState::Running { .. }
        ));
        assert!(matches!(
            app.workspace.panes[1].session.as_ref().unwrap().state(),
            SessionState::Running { .. }
        ));
        assert!(
            app.workspace.panes[0]
                .session
                .as_ref()
                .unwrap()
                .size()
                .columns()
                > 10
        );
        assert!(
            app.workspace.panes[1]
                .session
                .as_ref()
                .unwrap()
                .size()
                .columns()
                > 10
        );
        app.workspace.request_shutdown_all();
        wait_for(
            app.workspace.panes[0].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
        wait_for(
            app.workspace.panes[1].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
    }

    #[test]
    fn native_close_waits_for_all_sessions_to_finish_cleanup() {
        let context = egui::Context::default();
        let session1 = Session::spawn(
            portable_pty::CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let session2 = Session::spawn(
            portable_pty::CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let mut app = VastermApp::from_workspace(Workspace::from_panes(vec![
            Pane::with_session(PaneId(0), session1),
            Pane::with_session(PaneId(1), session2),
        ]));
        let mut output = context.run_ui(close_input(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        assert!(app.closing_window);

        wait_for(
            app.workspace.panes[0].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
        wait_for(
            app.workspace.panes[1].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );

        let mut output = context.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
        assert!(!app.closing_window);
    }

    #[test]
    fn window_resize_resizes_both_panes_independently() {
        let context = egui::Context::default();
        let session1 = Session::spawn(
            portable_pty::CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let session2 = Session::spawn(
            portable_pty::CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let mut app = VastermApp::from_workspace(Workspace::from_panes(vec![
            Pane::with_session(PaneId(0), session1),
            Pane::with_session(PaneId(1), session2),
        ]));

        let raw1 = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut output = context.run_ui(raw1, |ui| app.show(ui));
        output.textures_delta.clear();

        let deadline = Instant::now() + Duration::from_secs(3);
        while app.workspace.panes[0].session.as_ref().unwrap().size() == GridSize::default()
            || app.workspace.panes[1].session.as_ref().unwrap().size() == GridSize::default()
        {
            app.workspace.poll();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let size1_p0 = app.workspace.panes[0].session.as_ref().unwrap().size();
        let size1_p1 = app.workspace.panes[1].session.as_ref().unwrap().size();

        let raw2 = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1600.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = context.run_ui(raw2, |ui| app.show(ui));
        output.textures_delta.clear();

        let deadline = Instant::now() + Duration::from_secs(3);
        while app.workspace.panes[0].session.as_ref().unwrap().size() == size1_p0
            || app.workspace.panes[1].session.as_ref().unwrap().size() == size1_p1
        {
            app.workspace.poll();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let size2_p0 = app.workspace.panes[0].session.as_ref().unwrap().size();
        let size2_p1 = app.workspace.panes[1].session.as_ref().unwrap().size();

        assert!(size2_p0.columns() > size1_p0.columns());
        assert!(size2_p1.columns() > size1_p1.columns());
        assert!(size2_p0.rows() > size1_p0.rows());
        assert!(size2_p1.rows() > size1_p1.rows());

        app.workspace.request_shutdown_all();
        wait_for(
            app.workspace.panes[0].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
        wait_for(
            app.workspace.panes[1].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
    }
    #[test]
    fn two_panes_isolate_text_ime_meta_navigation_and_interrupts() {
        use egui::{Event, ImeEvent, Key, Modifiers};
        use portable_pty::CommandBuilder;

        let expected_p0 = b"a\xc3\xa9\x03\x1bOA\r\x7f\t\x1b\x1bOH\x1bOF\x1bb\x1bf\x04";
        let mut command0 = CommandBuilder::new("/bin/sh");
        command0.args([
            "-c",
            &format!(
                "stty raw -echo; printf '\\033[2J\\033[H\\033[?1hREADY0'; \
             received=$(dd bs=1 count={} 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
             printf '\\r\\nINPUT0:%s' \"$received\"",
                expected_p0.len()
            ),
        ]);
        let session0 = Session::spawn(command0, GridSize::default(), Arc::new(|| {})).unwrap();

        let expected_p1 = b"z\xc3\xa8\x03\x04";
        let mut command1 = CommandBuilder::new("/bin/sh");
        command1.args([
            "-c",
            &format!(
                "stty raw -echo; printf '\\033[2J\\033[H\\033[?1hREADY1'; \
             received=$(dd bs=1 count={} 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
             printf '\\r\\nINPUT1:%s' \"$received\"",
                expected_p1.len()
            ),
        ]);
        let session1 = Session::spawn(command1, GridSize::default(), Arc::new(|| {})).unwrap();

        let mut app = VastermApp::from_workspace(Workspace::from_panes(vec![
            Pane::with_session(PaneId(0), session0),
            Pane::with_session(PaneId(1), session1),
        ]));

        wait_for(app.workspace.panes[0].session.as_mut().unwrap(), |s| {
            screen(s).contains("READY0")
        });
        wait_for(app.workspace.panes[1].session.as_mut().unwrap(), |s| {
            screen(s).contains("READY1")
        });

        let context = egui::Context::default();
        let screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1200.0, 800.0),
        ));

        // Initialize app and settle initial focus.
        let raw = egui::RawInput {
            focused: true,
            screen_rect,
            ..Default::default()
        };
        let mut output = context.run_ui(raw.clone(), |ui| app.show(ui));
        output.textures_delta.clear();
        let mut output = context.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();

        let p0_surface_id = app.workspace.panes[0].surface_id();
        let p1_surface_id = app.workspace.panes[1].surface_id();

        assert!(
            context.memory(|m| m.has_focus(p0_surface_id)),
            "Pane 0 must have initial focus"
        );
        assert!(
            !context.memory(|m| m.has_focus(p1_surface_id)),
            "Pane 1 must not have initial focus"
        );

        // Deliver text, IME, interrupt, navigation, and Meta keys to Pane 0.
        let key = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let p0_events = vec![
            key(Key::A, Modifiers::NONE),
            Event::Text("a".into()),
            Event::Ime(ImeEvent::Preedit {
                text: "candidate".into(),
                active_range_chars: None,
            }),
            Event::Ime(ImeEvent::Commit("é".into())),
            key(Key::C, Modifiers::CTRL),
            key(Key::ArrowUp, Modifiers::NONE),
            key(Key::Enter, Modifiers::NONE),
            key(Key::Backspace, Modifiers::NONE),
            key(Key::Tab, Modifiers::NONE),
            key(Key::Escape, Modifiers::NONE),
            key(Key::Home, Modifiers::NONE),
            key(Key::End, Modifiers::NONE),
            key(Key::AltLeft, Modifiers::ALT),
            key(Key::B, Modifiers::ALT),
            Event::Text("b".into()),
            key(Key::F, Modifiers::ALT),
            Event::Text("f".into()),
            Event::Key {
                key: Key::AltLeft,
                physical_key: Some(Key::AltLeft),
                pressed: false,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            key(Key::D, Modifiers::CTRL),
        ];

        let raw = egui::RawInput {
            events: p0_events,
            focused: true,
            screen_rect,
            ..Default::default()
        };
        let mut output = context.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();

        wait_for(app.workspace.panes[0].session.as_mut().unwrap(), |s| {
            matches!(s.state(), SessionState::Exited(_))
        });

        let hex_p0: String = expected_p0.iter().map(|b| format!("{b:02x}")).collect();
        assert!(
            screen(app.workspace.panes[0].session.as_ref().unwrap())
                .contains(&format!("INPUT0:{hex_p0}")),
            "{}",
            screen(app.workspace.panes[0].session.as_ref().unwrap())
        );

        // Assert zero bytes leaked to Pane 1: Pane 1 is still running and waiting for input.
        assert!(matches!(
            app.workspace.panes[1].session.as_ref().unwrap().state(),
            SessionState::Running { .. }
        ));
        assert!(
            !screen(app.workspace.panes[1].session.as_ref().unwrap()).contains("INPUT1:"),
            "Pane 1 must not have received any leaked input: {}",
            screen(app.workspace.panes[1].session.as_ref().unwrap())
        );
        // Switch focus to Pane 1 by clicking on Pane 1.
        let click_pos = egui::pos2(900.0, 400.0);
        let raw = egui::RawInput {
            events: vec![
                Event::PointerMoved(click_pos),
                Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            focused: true,
            screen_rect,
            ..Default::default()
        };
        let mut output = context.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();

        assert!(
            context.memory(|m| m.has_focus(p1_surface_id)),
            "Pane 1 must be focused after click"
        );
        assert!(
            !context.memory(|m| m.has_focus(p0_surface_id)),
            "Pane 0 must no longer have focus"
        );

        // Deliver input to Pane 1.
        let p1_events = vec![
            key(Key::Z, Modifiers::NONE),
            Event::Text("z".into()),
            Event::Ime(ImeEvent::Preedit {
                text: "candidate".into(),
                active_range_chars: None,
            }),
            Event::Ime(ImeEvent::Commit("è".into())),
            key(Key::C, Modifiers::CTRL),
            key(Key::D, Modifiers::CTRL),
        ];
        let raw = egui::RawInput {
            events: p1_events,
            focused: true,
            screen_rect,
            ..Default::default()
        };
        let mut output = context.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();

        wait_for(app.workspace.panes[1].session.as_mut().unwrap(), |s| {
            matches!(s.state(), SessionState::Exited(_))
        });

        let hex_p1: String = expected_p1.iter().map(|b| format!("{b:02x}")).collect();
        assert!(
            screen(app.workspace.panes[1].session.as_ref().unwrap())
                .contains(&format!("INPUT1:{hex_p1}")),
            "{}",
            screen(app.workspace.panes[1].session.as_ref().unwrap())
        );

        app.workspace.request_shutdown_all();
        wait_for(
            app.workspace.panes[0].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
        wait_for(
            app.workspace.panes[1].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
    }
    #[test]
    fn clicking_pane_switches_focus_and_updates_visual_cues() {
        use egui::{Event, Modifiers, PointerButton, Pos2};
        use portable_pty::CommandBuilder;

        let context = egui::Context::default();
        let session0 = Session::spawn(
            CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let session1 = Session::spawn(
            CommandBuilder::new("/bin/cat"),
            GridSize::default(),
            Arc::new(|| {}),
        )
        .unwrap();

        let mut app = VastermApp::from_workspace(Workspace::from_panes(vec![
            Pane::with_session(PaneId(0), session0),
            Pane::with_session(PaneId(1), session1),
        ]));

        wait_for(app.workspace.panes[0].session.as_mut().unwrap(), |s| {
            matches!(s.state(), SessionState::Running { .. })
        });
        wait_for(app.workspace.panes[1].session.as_mut().unwrap(), |s| {
            matches!(s.state(), SessionState::Running { .. })
        });

        let screen_rect = Some(egui::Rect::from_min_size(
            Pos2::ZERO,
            egui::vec2(1200.0, 800.0),
        ));

        let p0_surface_id = app.workspace.panes[0].surface_id();
        let p1_surface_id = app.workspace.panes[1].surface_id();

        // Frame 1: Initial display. Pane 0 has default focus.
        let raw1 = egui::RawInput {
            focused: true,
            screen_rect,
            ..Default::default()
        };
        let mut output1 = context.run_ui(raw1.clone(), |ui| app.show(ui));
        output1.textures_delta.clear();
        let mut output1 = context.run_ui(raw1, |ui| app.show(ui));
        output1.textures_delta.clear();

        assert!(context.memory(|m| m.has_focus(p0_surface_id)));
        assert!(!context.memory(|m| m.has_focus(p1_surface_id)));

        let (selection_color, noninteractive_color) = context.options(|o| {
            (
                o.dark_style.visuals.selection.bg_fill,
                o.dark_style.visuals.widgets.noninteractive.bg_stroke.color,
            )
        });
        let has_active_stroke = output1.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Rect(rect)
                if rect.stroke.width == 2.0 && rect.stroke.color == selection_color)
        });
        let has_inactive_stroke = output1.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Rect(rect)
                if rect.stroke.width == 1.0 && rect.stroke.color == noninteractive_color)
        });
        assert!(
            has_active_stroke,
            "Pane 0 should have active selection border stroke"
        );
        assert!(
            has_inactive_stroke,
            "Pane 1 should have inactive border stroke"
        );

        // Frame 2: Click on Pane 1 (e.g. at (900.0, 400.0)).
        let click_p1 = egui::pos2(900.0, 400.0);
        let raw2 = egui::RawInput {
            events: vec![
                Event::PointerMoved(click_p1),
                Event::PointerButton {
                    pos: click_p1,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: click_p1,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            focused: true,
            screen_rect,
            ..Default::default()
        };
        let mut output2 = context.run_ui(raw2, |ui| app.show(ui));
        output2.textures_delta.clear();

        assert!(
            context.memory(|m| m.has_focus(p1_surface_id)),
            "Pane 1 must have focus after click"
        );
        assert!(
            !context.memory(|m| m.has_focus(p0_surface_id)),
            "Pane 0 must lose focus"
        );

        // Frame 3: Click back on Pane 0 (e.g. at (300.0, 400.0)).
        let click_p0 = egui::pos2(300.0, 400.0);
        let raw3 = egui::RawInput {
            events: vec![
                Event::PointerMoved(click_p0),
                Event::PointerButton {
                    pos: click_p0,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: click_p0,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            focused: true,
            screen_rect,
            ..Default::default()
        };
        let mut output3 = context.run_ui(raw3, |ui| app.show(ui));
        output3.textures_delta.clear();

        assert!(
            context.memory(|m| m.has_focus(p0_surface_id)),
            "Pane 0 must have focus after clicking back"
        );
        assert!(
            !context.memory(|m| m.has_focus(p1_surface_id)),
            "Pane 1 must lose focus"
        );

        app.workspace.request_shutdown_all();
        wait_for(
            app.workspace.panes[0].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
        wait_for(
            app.workspace.panes[1].session.as_mut().unwrap(),
            Session::cleanup_complete,
        );
    }
}
