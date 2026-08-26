fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("assets").join("icon.ico");
    let mut res = winres::WindowsResource::new();
    res.set_icon(icon.to_str().unwrap());
    res.set("FileDescription", "TUI Project Manager");
    res.set("ProductName", "TUI Project Manager");
    res.set("LegalCopyright", "Copyright (C) 2026");

    // 版本注入：GitHub Actions 在构建前把发布版本号写入 version.txt（本地开发
    // 无此文件则回退 Cargo.toml 版本）。build.rs 读到后编译成 APP_VERSION，
    // 同时写入 Windows 资源文件版本信息。
    println!("cargo:rerun-if-changed=version.txt");
    let version_file = manifest_dir.join("version.txt");
    if let Ok(v) = std::fs::read_to_string(&version_file) {
        let v = v.trim();
        if !v.is_empty() {
            println!("cargo:rustc-env=APP_VERSION={v}");
            res.set("FileVersion", v);
            res.set("ProductVersion", v);
            println!("cargo:warning=version.txt = {v}");
        }
    }

    // 资源编译失败只警告不阻断：无图标/版本信息不影响功能，
    // 且允许在未安装 rc.exe 的开发机上正常构建。
    if let Err(e) = res.compile() {
        eprintln!("winres compile failed (continuing without resources): {e}");
    }
}
// 注：新版 ConPTY（assets/conpty）已改为 include_bytes! 编进主程序、首次启动
// 会话时解包到临时目录（见 session::ensure_bundled_conpty），发布仍是单文件。