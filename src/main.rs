fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([400.0, 240.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Vasterm",
        options,
        Box::new(|context| {
            #[cfg(target_os = "macos")]
            {
                use winit::platform::macos::{OptionAsAlt, WindowExtMacOS};
                let window = context.winit_window().ok_or_else(|| {
                    std::io::Error::other(
                        "native window unavailable; cannot configure Option-as-Meta",
                    )
                })?;
                window.set_option_as_alt(OptionAsAlt::OnlyLeft);
            }
            Ok(Box::new(vasterm::VastermApp::new(&context.egui_ctx)))
        }),
    )
}
