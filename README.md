# TUI 项目管理器

一个 Windows GUI 项目启动器：在一个窗口里管理你的项目列表，并在内嵌的终端页签中于各项目目录运行 TUI 程序（nvim / lazygit / htop / cmd / bash 等）。

![egui](https://img.shields.io/badge/UI-egui%2Feframe-orange)
![Rust](https://img.shields.io/badge/Rust-1.97-blue)
![Windows](https://img.shields.io/badge/Platform-Windows%20x64-lightgrey)

## 功能

- **项目列表**：左侧选择项目 → 点击「启动」在内嵌终端页签中运行配置的 TUI 命令。
- **添加项目**：支持名称 + 路径（原生文件夹选择窗口「浏览…」），名称留空时自动用路径最后一段。
- **项目管理**：重命名 / 改路径 / 删除，配置自动保存到 exe 同级的 `config/config.json`。
- **多 TUI 命令**：在「设置」中可配置多个命令（nvim / lazygit / cmd …），点选其中一个作为启动命令；pi / opencode 自动安装装好后也会自动加进这个列表。
- **内嵌终端**：多页签并排，退出时记录打开中的页签目录，下次启动自动恢复。
- **深浅主题**：右下角一键切换，暗色 TUI 输出自动映射为浅色主题可读配色；原生标题栏固定黑色（DWM），不随深浅切换。
- **复制粘贴**：右键弹菜单（复制 / 粘贴 / 清空输入），不再右键直接粘贴以免误触；Ctrl+C 有选区时复制、无选区发 SIGINT；多行粘贴支持括号粘贴（应用按字面插入）或转 `\r`（shell 逐行执行）。
- **拖文件/粘贴文件到终端**：从资源管理器拖文件进终端，或复制文件后 `Ctrl+V` / 右键「粘贴」，把文件的**相对路径**（相对会话目录）粘贴到输入行；目录外的文件保留绝对路径，含空格的路径自动加引号。
- **页签右键菜单**：页签上右键可「打开目录（资源管理器）」「在 VS Code 打开」（依赖 `code` CLI 已加入 PATH）。
- **页签崩溃隔离**：单个页签渲染崩溃会被 `catch_unwind` 捕获并关闭，弹窗询问是否重新打开，不影响其他页签与整个软件。
- **滚动查看历史**：滚轮**一格 = 一次翻页**（与 PageUp/PageDown 同量；按原始档位计数，
  不做平滑拖尾），左键拖拽选择文本；全屏 TUI（opencode /
  jcode 等）的滚轮作为真实滚动事件转发给应用（SGR/X10 编码自动匹配，无鼠标
  上报的备用屏应用译成 PgUp/PgDn），不缓存任何内容——单文件内置新版 ConPTY
  （Win10 内置老版会吞掉鼠标模式声明、改写 SGR 序列导致转发失效），首次运行
  解包到临时目录。触摸板是连续位移：主屏按行滚（备用屏每帧至多一次翻页）。
- **检查更新**：启动与新开页签时自动检查 GitHub Release 版本（默认走国内镜像代理源，ghfast / gh-proxy / moeyy 等镜像 CDN 轮流加速、延迟低，全挂才回退直连 GitHub）；**所有 curl/PS 请求一律不带任何代理**（`direct()`：`--noproxy *` + 清掉 http_proxy/https_proxy/all_proxy 等环境变量；PS 通道脚本里置 `[Net.WebRequest]::DefaultWebProxy=$null`）——镜像源不是「代理加速」，历史上本机 Clash 残留端口（.curlrc）已让所有源集体失败过，且走同一个代理端口限速会让 10 条链一起跌破 `--speed-limit 4096` 速度地板、集体被判死；
只有网络类失败才按节奏重试；自更新下载**封顶 5 次**（退避 3s→6s→12s→24s→30s，可随时按「✕ 取消」即刻收手），不再无限重试；重试期的状态栏文案**不复位** `downloading` 状态（复位会让「✕ 取消」按钮消失，而下载线程还在后台重试 → 用户按不到停止，这就是「下载重试被无限循环无法停止」的成因）。
  查找顺序：**本软件 exe 所在目录**（运行时由 `current_exe()` 得出，不硬编码，即「打开软件目录」看到的那层）→ 该目录下的同名子目录（`pi\pi.exe`，pi 目录安装的常见摆法）→ 设置页里配置的额外路径（exe 完整路径或目录均可）→ PATH（可在设置页关掉）。全局安装那份仍作兜底。上述同级目录路径会在启动时自动写进 `settings.tool_paths`，各机器各存一份。
- **刷新率**：固定 10fps（曾开放 30/60 FPS 档，高帧率持续重绘会干扰 Windows 悬停激活窗口，已移除）；进程有输出/交互时按 10fps 刷新（cmd 式事件唤醒，回显延迟≈单帧 vsync），全部静止时自动降到 500ms 慢心跳省电（不再恒定满帧空转）。

## 使用

直接运行 `tui-project-manager.exe`（Release 构建无需额外依赖，双击打开 GUI，无命令行窗口）。

### 首页

| 按钮 | 说明 |
| --- | --- |
| `＋ 添加` | 添加项目（名称 + 路径 + 浏览…） |
| `▶ 启动 (内嵌页签)` | 在项目目录运行当前选中的 TUI 命令 |
| `重命名` / `改路径` / `删除` | 管理选中项目 |
| `⚙ 设置`（右下角「⋯ 更多」里） | 配置 TUI 启动命令（可添加多个、选择一个）、查看配置文件路径；内容超出一屏时出滚动条，配置被本程序别处改动（如自动安装后新增启动命令）时本页热更新 |
| 右下角「⋯ 更多」 | 折叠菜单（状态栏右侧固定簇只剩这一个按钮）：`⚙ 设置` / `🔄 检查更新`（同时检查 pi / opencode）/ `📂 打开用户目录` / `📂 打开软件目录` / 深浅色（深色 → 浅色 → 跟随系统 轮转）。**点菜单里的项菜单不关**，点菜单外面（或 Esc、再点「⋯ 更多」）才隐藏 |
| 设置页「工具更新路径」 | pi / opencode 的查找位置（exe 完整路径或目录，✅/⬜ 标存在与否，可添加/移除/打开）+ 切换是否扫 PATH；启动时自动补齐本软件同级目录下的默认项 |
| 设置页「供应商配置」 | 三个页签：pi（`~/.pi/agent/models.json`）/ oh-my-pi（`~/.omp/agent/models.yml`）/ opencode（`~/.config/opencode/opencode.json`），可增删供应商与模型、改 baseUrl/apiKey |

### 终端页签

| 操作 | 功能 |
| --- | --- |
| `滚轮` | 翻看滚动缓冲（历史输出），一格 = 一次翻页（触摸板按行）；全屏 TUI 下转发真实滚动事件给应用处理 |
| `左键拖拽` + `右键` | 选中 → 右键复制 |
| `右键` | 弹出菜单：复制选中文本 / 粘贴 / 清空输入（清空等效按住退格清除全部内容） |
| `页签右键` | 弹出菜单：打开目录 / 在 VS Code 打开 |
| `Ctrl+C` | 有选区时复制；无选区时发 SIGINT 给终端程序 |
| `Ctrl+V` / `Ctrl+Shift+V` | 粘贴（多行自动处理换行；剪贴板中是文件时粘贴文件相对路径） |
| `拖放文件到终端` | 把文件相对路径（相对会话目录）粘贴到输入行 |
| `Tab` / `Shift+Tab` / `方向键` / `Esc` | 原样转发给终端程序（斜杠命令补全、命令历史、取消输入）；终端聚焦时键盘焦点锁在终端，不做 egui 焦点遍历 |
| `×`（页签右侧） | 关闭会话 |

页签运行状态（🔄 / ✅ / 空 / ❌）走一层抽象接口 `src/runstate.rs`：**默认保底**是原来的输出启发式（最近 3s 有输出即 🔄，仅凭终端内容判定，适用普通 shell 命令），**pi / opencode 页签则读 agent 自己的权威状态源**：

- **pi / oh-my-pi**（`RunState` 抽象的第一个实现）：追尾会话 JSONL `~/.pi/agent/sessions/<项目slug>/<会话>.jsonl` 的最后一条 message 记录。`assistant+stopReason=toolUse`（工具在跑）或 `toolResult`（还在接着生成）→ **Busy**；`assistant+stop` 或 `user` → **Idle**。模型思考/长工具期间终端一个字节都不出，旧口径必然误判成「完成」并弹通知。**归属靠官方字段而非目录名反推**：目录 slug（照本机目录名反推的）只当索引，候选文件按 mtime 从新到旧逐个读**头一条 `{"type":"session","cwd":…}`** 校验 `cwd` 等于页签目录（路径比较统一斜杠、去尾斜杠、Windows 下大小写不敏感），故不会认到别的项目的会话去。
- **opencode**：经它自带的 `opencode db "SQL" --format json` 读库里的 `session`/`part` 表（不必自己解 SQLite）：同目录（`session.directory`）取最后一条 part 最新的那条会话，最后一条 part 是 `step-finish`+`reason=stop` → **Idle**，其余 → **Busy**。一次查询冷启约 1s，故走**全局缓存 + 锁内串行**（多个 opencode 页签最多一条查询在跑）；`opencode.exe` 的查找与「检查更新 / 一键安装」**同款**（同级目录 → 设置页配的工具路径 → PATH），不会出现「装更新时找得到、读状态时找不到」的分裂。
- 读不到 / 工具没装 / 命令认不出 → 一律 **Unknown，原样回退输出启发式**，不丢状态。判定合并口径见 `tab_icon_with`：`Busy` 直接 🔄（压过输出窗口），`Idle` 压掉输出窗口那条 🔄（pi 停在输入框时动画一直在刷，不压会常亮 🔄），但 ✅/空 仍按「≥3s 无输出」算——刚提交 prompt 的瞬间就是 Idle，提前判完成会弹假通知。
- 状态由每个会话自己的后台线程（1.2s 一轮）写进 `Session::run_state`（`Arc<AtomicU8>`），UI 只读原子量，不加锁等待；会话退出（`exited` 置位）线程自动收尾。手工验证入口：`cargo test --bin tui-project-manager -- --ignored --nocapture live_probe`（对着本机真实会话文件/数据库各读一遍，默认不跑）。

## 配置

配置自动保存到 exe 同级的 `config/config.json`，首次运行会自动生成。

```json
"tui_commands": ["nvim", "lazygit", "cmd"],
    "tui_command": "nvim",
    "history_lines": 1000
  }
}
```

`settings.history_lines`：终端回看历史行数上限（100..=5000，默认 1000）。每行 ≈ 32B/格，120 列时 1000 行 ≈ 3.8MB/页签，按需调大（如 2000 ≈ 7.7MB）。

`settings.tui_commands`：启动命令列表。**添加 / 改名 / 加载时都先按等价键判重**（去首尾空白与引号 → 取命令本身 → 取路径末段文件名 → 去 `.exe` → 小写），所以 `nvim`、`NVIM`、`nvim.exe`、`D:\Tools\nvim.EXE` 视为同一条，不会重复入列；输入框边输边标黄提示「已存在」，点添加时只在状态栏提示、不入列。配置里已有的重复项在加载时自动去重（保留首条），选中的 `tui_command` 归一到列表里真实存在的那条。

`settings.tool_paths` / `settings.tool_search_path`：「检查更新」里 pi / opencode 的查找位置（本机路径，不跨机器共用）与是否回退扫 PATH。启动时自动补齐「本软件同级目录下的 pi / opencode」两项（路径由 `current_exe()` 推得，不硬编码），设置页可增删。

**opencode 供应商配置**：设置页第三个页签，写 `~/.config/opencode/opencode.json`（有 `opencode.jsonc` 时优先那个）。只 patch `provider` 子树里本程序管理的字段（`name` / `npm` / `options.baseURL` / `options.apiKey` / `models.<id>.name`），`$schema`、顶层 `model`、`disabled_providers`、模型项的 `limit` 等一律原样保留。文件带注释（JSONC）导致解析失败时**停用编辑并报错**，绝不覆写原文件。`baseURL` 用的是 opencode 自己的键名，与 pi 页签的 `baseUrl` 不同。

字形图集（每页 1024² RGBA ≈ 4MB 内存 + 4MB 显存）由全进程共享：同一字体链与物理字号下所有页签共用一份位图与纹理，开第 2 个页签不再重复占用与重复光栅化，关掉最后一个页签即整体释放。字体、epi 字体图集本就是进程级的，多页签的额外开销只剩各页签自己的回看历史与帧缓冲。

渲染后端自动选择：AMD/NVIDIA 用 OpenGL（glow，内存省 ~200MB），Intel iGPU 用 DX12（wgpu，绕老 GL/Vulkan 驱动闪退）；`TPM_RENDERER=glow|wgpu` 可强制。


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
