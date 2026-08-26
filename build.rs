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

    // 捆绑新版 ConPTY（取自 VS Code 内置的 node-pty 同源构建，1.25 版）：
    // Win10 内置老版 conpty 会吞掉备用屏/鼠标模式声明（?1049h/?1000h），并把
    // 宿主写入的 SGR 滚轮序列改写成乱码，导致全屏 TUI 滚轮转发失效。portable-pty
    // 会优先侧载 exe 旁的 conpty.dll，找不到才回退系统内置。复制到目标目录与
    // 测试目录（测试 exe 在 target/<profile>/deps，按应用目录搜索加载）。
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile_dir = out
        .ancestors()
        .nth(3)
        .map(std::path::Path::to_path_buf)
        .unwrap();
    for f in ["conpty.dll", "OpenConsole.exe"] {
        let src = manifest_dir.join("assets").join("conpty").join(f);
        println!("cargo:rerun-if-changed=assets/conpty/{f}");
        for dir in [profile_dir.clone(), profile_dir.join("deps")] {
            if let Err(e) = std::fs::copy(&src, dir.join(f)) {
                eprintln!("copy {f} to {:?} failed (continuing): {e}", dir);
            }
        }
    }
}