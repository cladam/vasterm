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
        Box::new(|_| Ok(Box::new(vasterm::VastermApp))),
    )
}
