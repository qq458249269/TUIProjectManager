#![windows_subsystem = "windows"]

mod app;
mod config;
mod session;
mod terminal;

use eframe::egui;

/// 版本号：GitHub Actions 构建前把发布版本号写入 version.txt，由 build.rs 注入
/// APP_VERSION；本地开发没有该文件时回退到 Cargo.toml 的版本。
pub fn app_version() -> &'static str {
    option_env!("APP_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

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
        &format!("TUI 项目管理器 v{}", app_version()),
        options,
        Box::new(|cc| Ok(Box::new(app::ClientApp::new(cc)))),
    )
}
