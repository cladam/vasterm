use eframe::egui;

const SESSION_STATUS: &str = "No shell is running.";

#[derive(Default)]
pub struct VastermApp;

impl eframe::App for VastermApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        show_bootstrap(ui);
    }
}

fn show_bootstrap(ui: &mut egui::Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.heading("Vasterm");
        ui.label("A vast workspace for terminals.");
        ui.separator();
        ui.heading("M1: One dependable local terminal");
        ui.label("Native application bootstrap");
        ui.label(SESSION_STATUS);
        ui.label("The next slice connects a local shell to the terminal emulator.");
        ui.separator();
        ui.label(format!(
            "Display scale: {:.2} physical pixels per UI point",
            ui.ctx().pixels_per_point()
        ));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_paints_an_explicit_no_session_status() {
        let context = egui::Context::default();
        let mut output = context.run_ui(egui::RawInput::default(), show_bootstrap);
        // This headless test inspects paint output without uploading textures.
        output.textures_delta.clear();

        assert!(output.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Text(text)
                if text.galley.text() == SESSION_STATUS)
        }));
    }
}
