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
    if let Ok(exe) = std::env::current_exe()
        && let Some(name) = exe.file_name().and_then(|n| n.to_str())
    {
        // 已在 .running（上次启动改名后本进程又由 .running 路径启动的极端情况）：
        // 不再继续套娃，保持现状即可，更新侧的 canonical_exe_path 会去掉后缀。
        if name.ends_with(".running") {
            return;
        }
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
 
/// 顶层窗口是否为给定 hwnd（判断本应用是否在前台：用户正盯着看就不弹提醒）。
pub fn app_is_foreground(hwnd: isize) -> bool {
    hwnd != 0 && unsafe { GetForegroundWindow() } == hwnd
}

/// 运行结束系统通知：PowerShell WinRT toast，免安装器（无 AUMID 注册）也能显示。
/// Win10 2004+ / Win11 会静默丢弃未注册 AppUserModelID 的 toast——非激活时的
/// 提示之所以失效，就是 AppId 没注册。启动时 ensure_toast_registered() 往
/// HKCU\Software\Classes\AppUserModelId\<AUMID>\CustomActivator 埋注册项
/// （Microsoft 文档：unpackaged 桌面应用两种注册方式之一，reg 一个 CLSID 即
/// 可；点按激活让系统处理即可，本应用不接 toast 激活回调），以后显示就走
/// 这个固定 AUMID。CREATE_NO_WINDOW 启动，不闪控制台。
pub const TOAST_AUMID: &str = "TUIProjectManager.Toast";

/// STARTUP 时幂等注册 toast AUMID（best-effort，失败静默：下次启动再试）。
pub fn ensure_toast_registered() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = format!(
        "$k = 'HKCU:\\Software\\Classes\\AppUserModelId\\{aumid}'; \
         New-Item -Force -Path $k | Out-Null; \
         New-ItemProperty -Force -Path $k -Name CustomActivator -Value '{{00000000-0000-0000-0000-000000000000}}' -PropertyType String | Out-Null",
        aumid = TOAST_AUMID
    );
    let _ = std::process::Command::new("powershell")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("-NoProfile")
        .arg("-WindowStyle")
        .arg("Hidden")
        .arg("-Command")
        .arg(script)
        .spawn();
}

pub fn notify_run_finished(title: &str, heading: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let t = title.replace('\'', "''");
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
         [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null; \
         $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $n = $x.GetElementsByTagName('text'); \
         $n.Item(0).AppendChild($x.CreateTextNode('{heading}')) > $null; \
         $n.Item(1).AppendChild($x.CreateTextNode('{t}')) > $null; \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{aumid}').Show([Windows.UI.Notifications.ToastNotification]::new($x))",
        t = t,
        heading = heading,
        aumid = TOAST_AUMID
    );
    let _ = std::process::Command::new("powershell")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("-NoProfile")
        .arg("-WindowStyle")
        .arg("Hidden")
        .arg("-Command")
        .arg(script)
        .spawn();
}

/// 任务栏闪烁提醒：FLASHW_ALL | FLASHW_TIMERNOFG = 持续闪到窗口回到前台。
/// 窗口本身在前台时 FlashWindowEx 自动不闪，无需额外判断。
#[repr(C)]
struct FlashWInfo {
    cb_size: u32,
    hwnd: isize,
    dw_flags: u32,
    u_count: u32,
    dw_timeout: u32,
}
const FLASHW_ALL: u32 = 0x3;
const FLASHW_TIMERNOFG: u32 = 0xC;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> isize;
    fn FlashWindowEx(pfwi: *mut FlashWInfo) -> i32;
}

/// 开始任务栏闪烁（hwnd=0 时静默返回）。
pub fn flash_taskbar(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let mut info = FlashWInfo {
        cb_size: std::mem::size_of::<FlashWInfo>() as u32,
        hwnd,
        dw_flags: FLASHW_ALL | FLASHW_TIMERNOFG,
        u_count: 0,   // TIMERNOFG 下忽略次数
        dw_timeout: 0, // 0 = 系统默认闪烁速度
    };
    unsafe { FlashWindowEx(&mut info) };
}

fn main() -> eframe::Result {
    // 全进程 panic 钩子：任何线程 panic（页签 reader/渲染/解析线程）都记到崩溃日志，
    // 不静默吞掉。日志写在 exe 同级 crash.log，供事后定位到底哪个页签/线程崩了。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.to_string();
        eprintln!("[PANIC] {msg} (thread {:?})", std::thread::current().name());
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("crash.log"))
                    .and_then(|mut f| {
                        use std::io::Write;
                        writeln!(f, "[{:?}] {msg} (thread {:?})",
                            std::time::SystemTime::now(),
                            std::thread::current().name())
                    });
        }
        default_hook(info);
    }));
    unlock_exe();
    ensure_toast_registered();
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
    let renderer = if prefer_glow() {
        eframe::Renderer::Glow
    } else {
        eframe::Renderer::Wgpu
    };
    let mut options = eframe::NativeOptions {
        renderer,
        viewport,
        ..Default::default()
    };
    #[cfg(windows)]
    if renderer == eframe::Renderer::Wgpu
        && let eframe::egui_wgpu::WgpuSetup::CreateNew(create_new) =
            &mut options.wgpu_options.wgpu_setup
    {
        create_new.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }
    let title = format!("TUI 项目管理器 v{}", app_version());
    // DX12 不可用（如 Win7/无 DX12 驱动）时 wgpu 初始化返回 Err → 自动回退 glow，
    // 避免“启动不了”。失败的 Err 分支不吞掉：仍会向上传播。
    let mut glow_fallback = options.clone();
    glow_fallback.renderer = eframe::Renderer::Glow;
    match eframe::run_native(&title, options, Box::new(create_app)) {
        Ok(()) => Ok(()),
        Err(err) if renderer == eframe::Renderer::Wgpu => {
            // 无控制台可看时记入 crash.log（与 panic 钩子同一目录）
            eprintln!("[wgpu] 初始化失败，自动回退 glow：{err}");
            eframe::run_native(&title, glow_fallback, Box::new(create_app))
        }
        Err(err) => Err(err),
    }
}

//// 渲染后端选择：AMD/NVIDIA 默认 glow（同帧输出，内存比 DX12 省 ~200MB）；
/// Intel iGPU 走 wgpu/DX12——其 OpenGL/Vulkan 老驱动会闪退（ig9icd64.dll /
/// igvk64.dll，0xc0000005），DX12 用微软运行时+厂商驱动可绕开。
/// TPM_RENDERER=glow|wgpu 显式强制，跳过检测。
fn prefer_glow() -> bool {
    match std::env::var("TPM_RENDERER").as_deref() {
        Ok("glow") => return true,
        Ok("wgpu") => return false,
        _ => {}
    }
    #[cfg(target_os = "windows")]
    {
        // wgpu::Instance::new() 在本机（Intel UHD 630 + AMD 混合）无窗口句柄下建即崩
        // （0xc0000005），枚举更会加载老 Intel igvk64.dll 闪退，catch_unwind 都抓不住。
        // 改用 WMI 纯文本查询（与 toast 同款 powershell 调用）：只读系统信息，
        // 不加载任何 GPU 驱动。Intel 现像卡 → wgpu/DX12，否则 glow。
        // 检测失败 → 当 Intel 处理 → 老 wgpu/DX12 稳妥路径（glow 只是省内存的优化，
        // 拿不准时宁能不优化不能崩）。
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = "Get-CimInstance Win32_VideoController | %{ $_.Name }";
        let names = match std::process::Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", script])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_ascii_lowercase(),
            Err(_) => String::new(),
        };
        let has_intel = names.contains("intel");
        let has_discrete =
            names.contains("nvidia") || names.contains("geforce") || names.contains("amd")
                || names.contains("radeon");
        // 有独显（AMD/NVIDIA）→ glow：独显 OGL 驱动稳定，兼省显存。
        // 纯 Intel / 未知 → wgpu/DX12：集显老 OGL 驱动会闪退（ig9icd64.dll），
        // DX12 用微软运行时+厂商驱动绕开；检测失败宁可不优化不能崩。
        !has_intel || has_discrete
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

/// 创建应用实例；run_native 可能调用两次（wgpu 失败回退 glow）。
fn create_app(
    cc: &eframe::CreationContext<'_>,
) -> Result<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(Box::new(app::ClientApp::new(cc)))
}
