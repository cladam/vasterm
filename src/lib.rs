mod input;
pub mod session;
mod surface;

use std::sync::Arc;

use eframe::egui;

use session::{GridSize, Session, SessionState, local_shell_command};

pub struct VastermApp {
    session: Option<Session>,
    startup_error: Option<String>,
    surface: surface::TerminalSurface,
    interaction_error: Option<String>,
}

impl VastermApp {
    pub fn new(context: &egui::Context) -> Self {
        let context = context.clone();
        let result = local_shell_command().and_then(|command| {
            Session::spawn(
                command,
                GridSize::default(),
                Arc::new(move || context.request_repaint()),
            )
        });
        match result {
            Ok(session) => Self {
                session: Some(session),
                startup_error: None,
                surface: Default::default(),
                interaction_error: None,
            },
            Err(error) => Self {
                session: None,
                startup_error: Some(format!("{error:#}")),
                surface: Default::default(),
                interaction_error: None,
            },
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.startup_error {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("Session failed: {error}"));
            }
            if let Some(session) = self.session.as_mut() {
                let color = if matches!(session.state(), SessionState::Failed(_)) {
                    egui::Color32::LIGHT_RED
                } else {
                    ui.visuals().text_color()
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, session.state().to_string());
                    ui.label(format!(
                        "{} x {} | {:.2} px/point",
                        session.size().columns(),
                        session.size().rows(),
                        ui.ctx().pixels_per_point()
                    ));
                    if matches!(
                        session.state(),
                        SessionState::Starting | SessionState::Running { .. }
                    ) && ui.button("Stop session").clicked()
                        && let Err(error) = session.shutdown()
                    {
                        self.interaction_error = Some(format!("Could not stop session: {error:#}"));
                    }
                });
            }
            if let Some(error) = &self.interaction_error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
                if ui.button("Dismiss error").clicked() {
                    self.interaction_error = None;
                }
            }
            ui.separator();
            if let Some(session) = self.session.as_mut()
                && let Err(error) = self.surface.show(ui, session)
            {
                self.interaction_error = Some(format!("Terminal interaction failed: {error:#}"));
                ui.ctx().request_repaint();
            }
        });
    }
}

impl eframe::App for VastermApp {
    fn logic(&mut self, _context: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(session) = self.session.as_mut() {
            session.poll();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }

    fn on_exit(&mut self) {
        if let Some(session) = self.session.as_mut()
            && let Err(error) = session.shutdown()
        {
            eprintln!("Vasterm: shutdown failed: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

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
            assert!(Instant::now() < deadline, "timed out: {}", session.state());
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
        let expected = b"a\xc3\xa9\x03\x1bOA\r\x7f\t\x1b\x1bOH\x1bOF\x04";
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
            let mut output = context.run_ui(raw, |ui| surface.show(ui, session).unwrap());
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
    fn startup_failure_is_visible_in_the_ui() {
        let context = egui::Context::default();
        let mut app = VastermApp {
            session: None,
            startup_error: Some("test shell could not start".into()),
            surface: Default::default(),
            interaction_error: None,
        };
        let mut output = context.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        // This headless test inspects paint output without uploading textures.
        output.textures_delta.clear();

        assert!(output.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Text(text)
                if text.galley.text() == "Session failed: test shell could not start")
        }));
    }
}
