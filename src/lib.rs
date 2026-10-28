pub mod session;

use std::sync::Arc;

use eframe::egui;

use session::{GridSize, Session, SessionState, local_shell_command};

pub struct VastermApp {
    session: Option<Session>,
    startup_error: Option<String>,
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
            },
            Err(error) => Self {
                session: None,
                startup_error: Some(format!("{error:#}")),
            },
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Vasterm");
            ui.label("A vast workspace for terminals.");
            ui.separator();
            ui.heading("M1: One dependable local terminal");
            if let Some(error) = &self.startup_error {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("Session failed: {error}"));
            }
            if let Some(session) = self.session.as_mut() {
                let color = if matches!(session.state(), SessionState::Failed(_)) {
                    egui::Color32::LIGHT_RED
                } else {
                    ui.visuals().text_color()
                };
                ui.colored_label(color, session.state().to_string());
                ui.label(format!(
                    "Terminal grid: {} columns x {} rows",
                    session.size().columns(),
                    session.size().rows()
                ));
                if matches!(
                    session.state(),
                    SessionState::Starting | SessionState::Running { .. }
                ) && ui.button("Stop session").clicked()
                    && let Err(error) = session.shutdown()
                {
                    self.startup_error = Some(format!("Could not stop session: {error:#}"));
                }
            }
            ui.label("Terminal drawing and keyboard input arrive in M1.3.");
            ui.separator();
            ui.label(format!(
                "Display scale: {:.2} physical pixels per UI point",
                ui.ctx().pixels_per_point()
            ));
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

    #[test]
    fn startup_failure_is_visible_in_the_ui() {
        let context = egui::Context::default();
        let mut app = VastermApp {
            session: None,
            startup_error: Some("test shell could not start".into()),
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
