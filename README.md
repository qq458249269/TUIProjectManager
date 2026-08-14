# TUI 项目管理器

一个 Windows GUI 项目启动器：在一个窗口里管理你的项目列表，并在内嵌的终端页签中于各项目目录运行 TUI 程序（nvim / lazygit / htop / cmd / bash 等）。

![egui](https://img.shields.io/badge/UI-egui%2Feframe-orange)
![Rust](https://img.shields.io/badge/Rust-1.97-blue)
![Windows](https://img.shields.io/badge/Platform-Windows%20x64-lightgrey)

## 功能

- **项目列表**：左侧选择项目 → 点击「启动」在内嵌终端页签中运行配置的 TUI 命令。
- **添加项目**：支持名称 + 路径（原生文件夹选择窗口「浏览…」），名称留空时自动用路径最后一段。
- **项目管理**：重命名 / 改路径 / 删除，配置自动保存到 exe 同级的 `config/config.json`。
- **多 TUI 命令**：在「设置」中可配置多个命令（nvim / lazygit / cmd …），点选其中一个作为启动命令。
- **内嵌终端**：键盘事件直接透传（支持 Ctrl+C / Ctrl+V 粘贴等），每个项目一个会话页签。
- **Ctrl+B 页签管理**：在终端页签内按 `Ctrl+B` 进入前缀模式管理页签。

## 使用

直接运行 `tui-project-manager.exe`（Release 构建无需额外依赖，双击打开 GUI，无命令行窗口）。

### 首页

| 按钮 | 说明 |
| --- | --- |
| `＋ 添加` | 添加项目（名称 + 路径 + 浏览…） |
| `▶ 启动 (内嵌页签)` | 在项目目录运行当前选中的 TUI 命令 |
| `重命名` / `改路径` / `删除` | 管理选中项目 |
| `⚙ 设置` | 配置 TUI 启动命令（可添加多个、选择一个） |

### 会话页签

在终端页签内按 `Ctrl+B` 进入前缀模式：

| 按键 | 功能 |
| --- | --- |
| `h` | 回到首页 |
| `n` / `p` | 下一个 / 上一个会话 |
| `x` | 关闭当前会话 |
| `b` | 发送 Ctrl+B 给终端程序 |
| `Esc` | 取消 |

## 配置

配置自动保存到 exe 同级的 `config/config.json`，首次运行会自动生成。

```json
{
  "projects": [
    { "name": "my-project", "path": "D:\\code\\my-project" }
  ],
  "settings": {
    "tui_commands": ["nvim", "lazygit", "cmd"],
    "tui_command": "nvim"
  }
}
```

## 从源码构建

```bash
# 需 Rust 1.97+；Windows 上建议 MSVC 工具链
cargo build --release
```

产物：`target/release/tui-project-manager.exe`

## 技术栈

- UI: [egui / eframe](https://github.com/emilk/egui)（OpenGL/Glow 渲染）
- 终端: [alacritty_terminal](https://github.com/alacritty/alacritty)（解析终端输出）
- 伪终端: [portable-pty](https://github.com/wez/wezterm)（winpty/conpty）
- 原生对话框: [rfd](https://github.com/Polpua/rfd)

## 发布

GitHub Actions 在每次 push 到 `main` 时自动构建 Windows x64 版本并发布 pre-release：
https://github.com/qq458249269/TUIProjectManager/releases

可在 Actions 页面手动触发（workflow_dispatch）并指定版本号。
