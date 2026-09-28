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
- **检查更新**：启动与新开页签时自动检查 GitHub Release 版本（默认走国内镜像代理源，ghfast / gh-proxy / moeyy 等镜像 CDN 轮流加速、延迟低，全挂才回退直连 GitHub）；右下角「⋯ 更多」折叠菜单（打开用户目录 / 打开软件目录 / 检查更新）可手动触发；提示只显示在窗口底部状态栏（下载链接），不弹窗。
- **pi / opencode 一键安装与升级**：检查更新时顺带检查 pi / opencode，有新版就在状态栏出「⬇ pi vX.Y.Z」按钮。**本机一个都没检测到时**（同级目录 / 设置路径 / PATH 都没有）不当作无事发生，而是出「⬇ 安装 pi」「⬇ 安装 opencode」按钮 + 状态栏提示，点一下即从镜像源下载、校验、解压并装进本软件同级目录：pi 装到 `<软件目录>\pi\`（压缩包含整棵程序树，不平铺以免弄脏软件目录），opencode 平铺为 `<软件目录>\opencode.exe`；版本号在点按钮那一刻现查，检查时网络不通也不影响安装入口。状态栏布局（先量宽度再画）：右侧固定簇（⋯ 更多 / 检查更新 / 深浅色）是 `right_to_left` **贴右边**画的，它不看左边占了多宽，放不下只会盖上去而不是换行——曾把「⬇ 装 pi」压在深浅色按钮下面点不着，所以先量出它、从行宽里扣掉；剩下的左段（消息 + 待办按钮）画在**横向滚动区**里，**挤不下就在下方出横向滚动条**（可拖、滚轮也能拨）——既不互相盖住，也没有按钮被收进弹出菜单里“消失”。左段的让位顺序：消息先拿宽度（封顶行宽 45%、省略号截断，悬停看全文、右键复制）→ 待办按钮（自更新下载 / 工具入口 / 装完重启）按实测宽度依次排开 → 消息剩下的宽度不足 40px 就整条不画（此时按钮已放不下，交给滚动条）。三种入口：① 有新版本 → 更新**该工具当前所在目录**那份；② 本机一个都没检测到 → 「⬇ 安装 pi」装到本软件同级目录；③ **本机在别处有（PATH / 别的配置路径）但本软件目录里没有** → 「⬇ 装 pi 到本软件目录」另装一份自锁（否则这种机器上一个入口都没有：检测到即视为已装，PATH 里那份永远是事实上的版本，换机器/改 PATH/删全局安装就断供；装好后本软件目录这份优先被找到，按钮自退）。装完自动把新 exe 写进「⚙ 设置 → TUI 启动命令」列表（列表里已有同义且能跑的命令不动；指向已不存在文件、或本机没装的裸名 `pi` 这类死命令就地换成新路径），选中哪条启动命令仍由用户自己点；设置页已开着时这条会**热更新**进去，不用关掉重开。
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
| `⚙ 设置` | 配置 TUI 启动命令（可添加多个、选择一个）、查看配置文件路径；内容超出一屏时出滚动条，配置被本程序别处改动（如自动安装后新增启动命令）时本页热更新 |
| 右下角「⋯ 更多」 | 折叠菜单：打开用户目录 / 打开软件目录 / 检查更新（同时检查 pi / opencode） |
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
| `×`（页签右侧） | 关闭会话 |

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
