#![windows_subsystem = "windows"]

mod app;
mod config;
mod session;
mod term_gl;
mod terminal;

use eframe::egui;

/// 版本号：GitHub Actions 构建前把发布版本号写入 version.txt，由 build.rs 注入
/// APP_VERSION；本地开发没有该文件时回退到 Cargo.toml 的版本。
pub fn app_version() -> &'static str {
    option_env!("APP_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

// 启动时解除 exe 文件锁定，让安装器/CI 可以覆盖写入新版本（unlock_exe）。
// 步骤：
//   1. 清理上次崩溃残留的 .running 文件
//   2. 重命名 `app.exe` → `app.exe.running`（rename 不需要写权限，进程运行中也能成功，原名立即空出）
//   3. 复制 `app.exe.running` → `app.exe`（目录里始终有一份可用的 exe，安装器可通过 .running 判断旧版本是否在运行）
// 当前进程通过 OS 旧句柄继续执行 app.exe.running，不受影响。
// .running 加隐藏+系统属性：Windows 锁定运行中的映像文件，「不生成 running 文件」
// 只有手动映射 PE 一条路（杀软必报、风险极高），所以改为让它对用户不可见。
#[cfg(windows)]
unsafe extern "system" {
    fn SetFileAttributesW(lpfilename: *const u16, dwfileattributes: u32) -> i32;
}
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

fn unlock_exe() {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(name) = exe.file_name().and_then(|n| n.to_str()) {
            if let Some(parent) = exe.parent() {
                let _ = std::fs::remove_file(parent.join(format!("{name}.running")));
            }
            let running = exe.with_file_name(format!("{name}.running"));
            let _ = std::fs::rename(&exe, &running);
            let _ = std::fs::copy(&running, &exe);
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt;
                let wide: Vec<u16> =
                    running.as_os_str().encode_wide().chain(Some(0)).collect();
                unsafe {
                    SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
                }
            }
        }
    }
}

fn main() -> eframe::Result {
    unlock_exe();
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
