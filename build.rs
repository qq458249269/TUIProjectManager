fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("assets").join("icon.ico");
    let mut res = winres::WindowsResource::new();
    res.set_icon(icon.to_str().unwrap());
    res.set("FileDescription", "TUI Project Manager");
    res.set("ProductName", "TUI Project Manager");
    res.set("LegalCopyright", "Copyright (C) 2026");
    // 资源编译失败只警告不阻断：无图标/版本信息不影响功能，
    // 且允许在未安装 rc.exe 的开发机上正常构建。
    if let Err(e) = res.compile() {
        eprintln!("winres compile failed (continuing without resources): {e}");
    }
}
