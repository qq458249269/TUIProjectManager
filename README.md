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
- **内嵌终端**：多页签并排，退出时记录打开中的页签目录，下次启动自动恢复。
- **深浅主题**：右下角一键切换，暗色 TUI 输出自动映射为浅色主题可读配色；原生标题栏固定黑色（DWM），不随深浅切换。
- **复制粘贴**：右键弹菜单（复制 / 粘贴 / 清空输入），不再右键直接粘贴以免误触；Ctrl+C 有选区时复制、无选区发 SIGINT；多行粘贴支持括号粘贴（应用按字面插入）或转 `\r`（shell 逐行执行）。
- **拖文件/粘贴文件到终端**：从资源管理器拖文件进终端，或复制文件后 `Ctrl+V` / 右键「粘贴」，把文件的**相对路径**（相对会话目录）粘贴到输入行；目录外的文件保留绝对路径，含空格的路径自动加引号。
- **页签右键菜单**：页签上右键可「打开目录（资源管理器）」「在 VS Code 打开」（依赖 `code` CLI 已加入 PATH）。
- **页签崩溃隔离**：单个页签渲染崩溃会被 `catch_unwind` 捕获并关闭，弹窗询问是否重新打开，不影响其他页签与整个软件。
- **滚动查看历史**：滚轮回看滚动缓冲，左键拖拽选择文本；全屏 TUI（opencode /
  jcode 等）也支持滚轮回看历史——这类应用用绝对定位整屏重绘、不产生换行滚动，
  且 Windows ConPTY 会改写宿主写入的 SGR/X10 鼠标序列与备用屏切换序列（滚轮
  转发给应用不可靠），改为通用地自攒视口历史行：滚轮先滚仿真器原生缓冲，滚不动
  时回看自攒历史，滚到底回到实时。
- **检查更新**：启动与新开页签时自动检查 GitHub Release 版本；右下角「⋯ 更多」折叠菜单（打开用户目录 / 打开软件目录 / 检查更新）可手动触发；提示只显示在窗口底部状态栏（下载链接），不弹窗。
- **刷新率**：默认 10fps 固定刷新，设置中可输入 10-60 FPS。

## 使用

直接运行 `tui-project-manager.exe`（Release 构建无需额外依赖，双击打开 GUI，无命令行窗口）。

### 首页

| 按钮 | 说明 |
| --- | --- |
| `＋ 添加` | 添加项目（名称 + 路径 + 浏览…） |
| `▶ 启动 (内嵌页签)` | 在项目目录运行当前选中的 TUI 命令 |
| `重命名` / `改路径` / `删除` | 管理选中项目 |
| `⚙ 设置` | 配置 TUI 启动命令（可添加多个、选择一个）、界面刷新帧率（10-60 FPS）、查看配置文件路径 |
| 右下角「⋯ 更多」 | 折叠菜单：打开用户目录 / 打开软件目录 / 检查更新 |

### 终端页签

| 操作 | 功能 |
| --- | --- |
| `滚轮` | 翻看滚动缓冲（历史输出）；全屏 TUI 下回看自攒的视口历史，滚到底回到实时 |
| `左键拖拽` + `右键` | 选中 → 右键复制 |
| `右键` | 弹出菜单：复制选中文本 / 粘贴 / 清空输入（清空等效按住退格清除全部内容） |
| `页签右键` | 弹出菜单：打开目录 / 在 VS Code 打开 |
| `Ctrl+C` | 有选区时复制；无选区时发 SIGINT 给终端程序 |
| `Ctrl+V` / `Ctrl+Shift+V` | 粘贴（多行自动处理换行；剪贴板中是文件时粘贴文件相对路径） |
| `拖放文件到终端` | 把文件相对路径（相对会话目录）粘贴到输入行 |
| `×`（页签右侧） | 关闭会话 |

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

需要 Rust（stable，1.97+）与 C 链接器。Windows 下两种工具链任选：

- **MSVC**（推荐）：安装 [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/)（含链接器与 `rc.exe`）
- **GNU**（w64devkit）：下载 [w64devkit](https://github.com/skeeto/w64devkit/releases) 解压后把 `bin` 加入 PATH

```bash
# 本地开发构建（快、带调试信息）
cargo build

# 发布构建（体积优先：opt-level=z + LTO + strip；不用 panic=abort，保证页签崩溃可被 catch_unwind 隔离）
cargo build --release
```

产物：`target/debug/tui-project-manager.exe` 或 `target/release/tui-project-manager.exe`

> 无 `rc.exe` 时 `build.rs` 的资源编译（图标/版本信息）会警告并跳过，不影响功能。

### 版本号注入

exe 标题栏与「检查更新」用的版本号来源：`version.txt`（仓库根目录）→ `build.rs` 读取注入 `APP_VERSION` 与 Windows 资源 `FileVersion`；文件不存在则回退 `Cargo.toml` 版本。

```bash
# 例：发布前写入版本号再构建（version.txt 已 gitignore，不会被误提交）
echo "2026.01.15.0042" > version.txt
cargo build --release
rm version.txt
```

## 发布（GitHub Actions）

`.github/workflows/build-win-x64.yml` 自动完成：计算版本号 → **写入 `version.txt`** → 构建 → （可选）代码签名 → 上传产物 → 发布 pre-release tag。

- **触发**：push 到 `main`（自动版本号：日期取今天，当天已发布过才自增末尾段，跨天从 `…0001` 重新开始）；或 Actions 页面手动触发并指定版本号
- **互斥**：同一时间只允许一个构建运行，新触发自动取消在跑的旧构建
- **产物**：`tui-project-manager-win-x64-<版本号>` artifact + 同名 Release

## 技术栈

- UI: [egui / eframe](https://github.com/emilk/egui)（OpenGL/Glow 渲染）
- 终端: [alacritty_terminal](https://github.com/alacritty/alacritty)（解析终端输出）
- 伪终端: [portable-pty](https://github.com/wez/wezterm)（winpty/conpty）
- 原生对话框: [rfd](https://github.com/Polpua/rfd)
