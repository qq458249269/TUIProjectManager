#![windows_subsystem = "windows"]

mod app;
mod config;
mod session;
mod terminal;

use eframe::egui;

fn main() -> eframe::Result {
    let config = config::load();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1100.0, 720.0])
        .with_min_inner_size([760.0, 420.0]);
    // 恢复上次的窗口位置/大小/最大化状态。
    if let Some(pos) = config.window.pos {
        viewport = viewport.with_position(pos);
    }
    if let Some(size) = config.window.size {
        viewport = viewport.with_inner_size(size);
    }
    if config.window.maximized {
        viewport = viewport.with_maximized(true);
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        &format!("TUI 项目管理器 v{}", env!("CARGO_PKG_VERSION")),
        options,
        Box::new(|cc| Ok(Box::new(app::ClientApp::new(cc)))),
    )
}
