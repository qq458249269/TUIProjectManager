#![windows_subsystem = "windows"]

mod app;
mod config;
mod session;
mod terminal;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 420.0]),
        ..Default::default()
    };

    eframe::run_native(
        "TUI 项目管理器",
        options,
        Box::new(|cc| Ok(Box::new(app::ClientApp::new(cc)))),
    )
}
