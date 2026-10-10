# TUI 项目管理器

一个 Windows GUI 项目启动器：在一个窗口里管理你的项目列表，并在内嵌的终端页签中于各项目目录运行 TUI 程序（nvim / lazygit / htop / cmd / bash 等）。

![egui](https://img.shields.io/badge/UI-egui%2Feframe-orange)
![Rust](https://img.shields.io/badge/Rust-1.97-blue)
![Windows](https://img.shields.io/badge/Platform-Windows%20x64-lightgrey)

## 功能

- **项目列表**：左侧选择项目 → 点击「启动」在内嵌终端页签中运行配置的 TUI 命令。
- **添加项目**：支持名称 + 路径（原生文件夹选择窗口「浏览…」），名称留空时自动用路径最后一段。
- **项目管理**：重命名 / 改路径 / 删除，配置自动保存到 exe 同级的 `config/config.json`。
- **多 TUI 命令**：在「设置」中可配置多个命令（nvim / lazygit / cmd …），点选其中一个作为启动命令；pi / opencode 自动安装装好后也会自动加进这个列表，设置页里还有「↺ 自动补齐」把「工具更新路径」扫到的 pi / opencode 一并加进来。
- **内嵌终端**：多页签并排，退出时记录打开中的页签目录，下次启动自动恢复。
- **深浅主题**：右下角一键切换，暗色 TUI 输出自动映射为浅色主题可读配色；原生标题栏固定黑色（DWM），不随深浅切换。
- **复制粘贴**：右键弹菜单（复制 / 粘贴 / 清空输入），不再右键直接粘贴以免误触；Ctrl+C 只做复制选区（`0x03` 永不写进 PTY，所以无选区时也不会发 SIGINT）；多行粘贴支持括号粘贴（应用按字面插入）或转 `\r`（shell 逐行执行）。
- **拖文件/粘贴文件到终端**：从资源管理器拖文件进终端，或复制文件后 `Ctrl+V` / 右键「粘贴」，把文件的**相对路径**（相对会话目录）粘贴到输入行；目录外的文件保留绝对路径，含空格的路径自动加引号。（纯文件剪贴板时 egui 根本不产粘贴事件——事件流里既没有 `Event::Paste` 也没有 `Event::Key`——所以文件粘贴是靠**物理 V 键按住 + Ctrl**（`GetAsyncKeyState(VK_V)`）认手势的：只有这一个组合会粘路径，切页签的 `Ctrl+Tab` 等其它 Ctrl 组合一律不认。）
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
| `⚙ 设置`（页签栏里，常驻在「🏠 首页」右边，与首页同款同尺寸、不可关不可拖） | 配置 TUI 启动命令（可添加多个、选择一个）、查看配置文件路径；内容超出一屏时出滚动条，配置被本程序别处改动（如自动安装后新增启动命令）时本页热更新 |
| 右下角「⋯ 更多」 | 折叠菜单（状态栏右侧固定簇只剩这一个按钮）：`🔄 检查更新`（同时检查 pi / opencode）/ `📂 打开用户目录` / `📂 打开软件目录` / 深浅色（深色 → 浅色 → 跟随系统 轮转）。**点菜单里的项菜单不关**，点菜单外面（或 Esc、再点「⋯ 更多」）才隐藏。设置改从页签栏的常驻 `⚙ 设置` 页签进，菜单里不再放设置项 |
| 设置页「工具更新路径」 | pi / opencode 的查找位置（exe 完整路径或目录，✅/⬜ 标存在与否，可添加/移除/打开）+ 切换是否扫 PATH；启动时自动补齐本软件同级目录下的默认项 |
| 设置页「供应商配置」 | 三个页签：pi（`~/.pi/agent/models.json`）/ oh-my-pi（`~/.omp/agent/models.yml`）/ opencode（`~/.config/opencode/opencode.json`），可增删供应商与模型、改 baseUrl/apiKey |

### 终端页签

| 操作 | 功能 |
| --- | --- |
| `滚轮` | 翻看滚动缓冲（历史输出），一格 = 一次翻页（触摸板按行）；全屏 TUI 下转发真实滚动事件给应用处理 |
| `左键拖拽` + `右键` | 选中 → 右键复制 |
| `右键` | 弹出菜单：复制选中文本 / 粘贴 / 清空输入（清空等效按住退格清除全部内容） |
| `页签右键` | 弹出菜单：打开目录 / 在 VS Code 打开 |
| `Ctrl+C` | 只复制选区（无选区时静默无动作）；**不会**向终端发 SIGINT（`0x03` 永不写进 PTY） |
| `Ctrl+V` / `Ctrl+Shift+V` | 粘贴（多行自动处理换行；剪贴板中是文件时粘贴文件相对路径，手势靠物理 V 键认，只认这一个组合） |
| `拖放文件到终端` | 把文件相对路径（相对会话目录）粘贴到输入行 |
| `Tab` / `Shift+Tab` / `方向键` / `Esc` | 原样转发给终端程序（斜杠命令补全、命令历史、取消输入）；终端聚焦时键盘焦点锁在终端，不做 egui 焦点遍历 |
| 其余 Ctrl 组合（含 `Ctrl+Z` / `Ctrl+Shift+Z`） | 一律发标准控制码原样转发（字节流表达不出 Shift，故 Ctrl+Shift+Z 与 Ctrl+Z 同为 `^Z`）；设置页不再有「Ctrl+Shift+Z 发送」翻译项 |
| `×`（页签右侧） | 关闭会话（`🏠 首页` 与 `⚙ 设置` 是常驻页面页签，没有 ×，不可关） |
| 滚轮（指针落在页签栏那一行） | 只横向拨**会话页签区**：页签过多时滚动查看，**不画滚动条**；`🏠 首页` / `⚙ 设置` 常驻左侧不动。拨轮期间不再自动把当前页签拽回视野（切页或改窗口宽度才重新跟随） |
| `◀` / `▶`（页签区两端） | 页签放不下时在视口两端**各留一个按钮位**（占布局位，不叠在页签上，页签正文不会被遮住），点一下翻大半屏；滚到头的那一端变灰且点不动，槽位仍保留（否则页签会在指针底下跳一下） |

页签运行状态（🔄 / ✅ / 空 / ❌）**仅凭终端输出启发式**判定（`update_done_states` + `tab_icon`）：最近 3s 有输出即 🔄（按键/粘贴等人工输入不算输出），内容静默但**未满 8s** 仍报 🔄（`DONE_QUIET_MS`），静默 ≥8s 且有内容才亮 ✅，零输出 / 静止等输入不误判 🔄。（早期版本曾为 pi / opencode 接过 agent 自己的权威状态源，后因那些状态源本身不稳/耗时而整体移除，见 `63f1756`。）

**为什么 🔄 与 ✅ 是两个门槛（3s / 8s）**：`OUTPUT_END_MS=3s` 只管「内容有多新鲜」，不足以判“任务结束”——agent 回合**中途**的静默（按下回车到首个 token 到达、跑一条不出字的命令、工具执行期 TUI 只在有变化时重绘）可以轻松超过 3s，旧判据此时就亮 ✅，等于谎报“完成”，用户看到的是「状态说完成了，实际还在执行中」。所以判完成另设一道 `DONE_QUIET_MS=8s` 的静默门槛：3~8s 这段不确定窗口继续报 🔄（也不落成空图标，那更像丢了状态），8s 后才判 ✅。代价是**真跑完时 ✅ 会晚 5s 亮**——用户看着的是刷新一眼就过去的图标，这个方向的迟滞远比谎报完成可接受。1Hz 秒表/时钟的刷新不算内容（它们只动 1~3 格），不受影响，仍照常判完成。

判据里的“输出”只看**可见格字符**的变化（动画分类器只用于字节分类，不参与状态判定）。变化分两档，各有时间戳：**成规模变化**（≥4 格）算“有输出”，进 3s 内容窗口；**小变化**（1~3 格，进度条/秒表/spinner）只算“画面在动”。由于图标是**每秒才重算一次**的快照（`STATE_CHECK_MS`），单靠“刚刚动过”的 250ms 瞬时窗口会被采样漏掉（动画周期不是 1s 整数倍时，大部分采样点落在窗外 → 页签在跑却显示空），所以另有“**连续动**”通道：两次画面变化间隔 <700ms 记一次连续动，持续保 1s（`MOTION_GAP_MS` / `MOTION_HOLD_MS`），同时它也是让状态重算提前的事件通道。同一次重绘被 `read()` 切成相邻几块时按 250ms 合并窗折叠，不会把“一次秒表跳格”数成“连续动”。

由此带来一条固有边界：**「任务完成」的系统通知默认启用**，靠「终端静默 N 秒 + 画面不在高频动」启发式判断（`src/app.rs` 的 `TOAST_QUIET_MS`）。原因是这条通知的判据只有「终端静默 N 秒 + 画面不在高频动」，而 agent 回合**中途**的静默（等首个 token、跑一条不出字的命令、工具执行期 TUI 只在有变化时重绘）与「回合真的跑完了」在信息上**不可区分**——判据原理上就不可靠，调参只能把误报率压低、压不到零。仍保留多条过滤：启动宽限、实质输出字节门槛、未查看、同页签 10s 节流、当前页签前台豁免。

另一条固有边界：**每秒跳一格（1Hz）的画面刷新与秒表/时钟在屏幕上不可区分**。“连续动”通道以 700ms 为上限正是为了把它们留在外（否则任何带时钟的 TUI 都会 🔄 常驻、✅ 永不亮）。代价是：一个只按 1Hz 刷新的后台程序仍可能在跑完前显示 ✅——纯屏幕启发式判不了，要判只能靠进程存活/CPU 采样。

8s 门槛同样不是万能的：**静默超过 8s 的回合**（长推理、网络挂起、卡在某个不重绘的 TUI 画面上）仍会被判完成，且 8s 之后屏幕若无变化就更无从分辨。这一条只能靠调 `DONE_QUIET_MS` 在「谎报完成」与「✅ 迟 5s 亮」之间取舍，压不到零。

页签底色分深浅两套（`ClientApp::tab_bg`）：**浅色**用 egui 的选中色（`from_gray(176)`），深浅一致；**深色**不用浅灰（浅灰块在 32,32,32 的面板底上刺眼），改成深蓝填充 `rgb(41,66,104)`（配白字约 10:1 对比度）并在页签底部加一条 2px 亮蓝下边线 `rgb(116,170,255)`（`ClientApp::tab_accent`，两端各留 8px，不顶到页签角、不压标题文字）——底色 + 下边线双重提示才够醒目（旧版只有 `rgb(42,44,52)` 与面板底色差十级灰，深色下肉眼分不出选中页签）。悬停仍用中性灰 `rgb(58,60,70)`，刻意不偏蓝，好让选中态的蓝明显是另一个色族。

- **「运行结束」通知始终弹**：它的判据是子进程真的退出（`try_wait`），权威而非启发式。
- **页签 ✅ 图标始终照旧**：它是给眼睛看的、刷新一眼就过去，误判的代价与系统通知不是一个量级。

「已查看」统一定义成**是否已交互**（`has_been_viewed`：当前页签激活、终端内点击/输入/滚轮等真实交互置位；新实质输出复位）。只有这一种情况免除 ✅。系统通知另有 `watched()`（当前页签**且**应用在前台）管"正盯着不打扰"：应用失焦时当前页签不再豁免 → 失焦期间跑完会亮 ✅、并按规则弹通知；回到窗口只恢复免打扰，✅ 要等一次真实交互才清。

## 配置

配置自动保存到 exe 同级的 `config/config.json`，首次运行会自动生成。

```json
"tui_commands": ["nvim", "lazygit", "cmd"],
    "tui_command": "nvim",
    "history_lines": 500
  }
}
```

`settings.history_lines`：终端回看历史行数上限（100..=5000，默认 500）。每行 ≈ 32B/格，120 列时 500 行 ≈ 1.9MB/页签，按需调大（如 2000 ≈ 7.7MB）。

`settings.tui_commands`：启动命令列表。**添加 / 改名 / 加载时都先按等价键判重**（去首尾空白与引号 → 取命令本身 → 取路径末段文件名 → 去 `.exe` → 小写），所以 `nvim`、`NVIM`、`nvim.exe`、`D:\Tools\nvim.EXE` 视为同一条，不会重复入列；输入框边输边标黄提示「已存在」，点添加时只在状态栏提示、不入列。配置里已有的重复项在加载时自动去重（保留首条），选中的 `tui_command` 归一到列表里真实存在的那条。

`settings.tool_paths` / `settings.tool_search_path`：「检查更新」里 pi / opencode 的查找位置（本机路径，不跨机器共用）与是否回退扫 PATH。启动时自动补齐「本软件同级目录下的 pi / opencode」两项（路径由 `current_exe()` 推得，不硬编码），设置页可增删。这份查找顺序（本软件目录 → 配的路径 → PATH）也被启动命令区的「↺ 自动补齐」复用：扫到的 pi / opencode 绝对路径按等价键幂并进 `tui_commands`，已有可用命令不动、失效旧路径就地替换、列表没变则不落盘；它只动列表，不改用户选中的 `tui_command`。

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

## 故障排查

### 画面卡死（内容不动、进程还在、CPU 占用正常）

先看**可执行文件同级目录**的 `crash.log`（`main.rs` 全局 panic 钩子写的）。

本项目已发生过的两类根因：

1. **reader 线程被 VT 解析 panic 带走**（历史高频）
   症状：画面停在最后一帧，子进程还活着但不响应，进程不退出。
   原因：`parser.advance()` 解析 PTY 输出时 panic（`alacritty_terminal` 内部），
   reader 线程直接死 → PTY 再无人读 → 子进程写满管道后阻塞 →
   快照停更 → 画面永远冻结。
   现状：`session.rs` 的读循环用 `guarded()`（`catch_unwind`）包住解析，
   panic 时打日志 + 置 `exited` + break（退出会话而不是静默冻屏）；
   锁中毒一律 `into_inner()` 照旧取用（`wlock` / `rlock`）。

2. **kitty 键盘模式 push 无界堆积**（2026-10-06 实测复现）
   症状：跑 1~2 分钟后必冻屏；`crash.log` 报
   `alacritty_terminal-0.26.0/src/term/mod.rs:1296:44:
   removal index (is 0) should be < len (is 0)`。
   原因：`Term::push_keyboard_mode`（处理 `ESC [ > flags u`）栈满 4096 时
   **误对 `title_stack` 做 `remove(0)`** —— 上游把变量写错了，空栈即 panic。
   触发：应用反复重新初始化终端（实测 pi 重启 43 次/秒，每次一个 push），
   95 秒必堆到 4096。
   现状：`strip_orphan_csi_u_bytes_into(.., &mut kitty_push_budget)` 给每会话
   64 条 push 预算，超出直接丢弃（本工具本就不支持 kitty 协议，
   应答 `ESC[?u` = flags 0，栈内容不影响对端）。
   回归测试：`session::tests::kitty_push_budget_prevents_terminal_panic`
   （喂 5000 条 push 不 panic）+ `kitty_push_within_budget_is_preserved`
   （预算内不误伤）+ `kitty_push_drops_after_budget`。

复现/取证开关（排障用，默认全关）：

- `TUIPM_LOG_WRITES=1`：把写入 PTY 的每个字节打到 stderr（查杂散输入）
- `PI_TUI_WRITE_LOG=<文件>`：让 pi 自己落盘 stdout（配合上面的日志对时序）
- 复现后先核对 `crash.log` 的行号：行号变了 = 换了解析路径，别只看现象

### 终端整体失灵（无横幅、无提示符、按键/拖选/Ctrl+C·V·Tab 全无反应）

**已修（2026-10-06，两处根因，同一次事故）**

1. **reader 复用缓冲只追加不清空** —— 这才是「杂字 / 输入全废」的真正元凶。
   `strip_orphan_csi_u_bytes_into(out, ..)` 的 `out` 是 reader 每块复用的同一个
   `Vec`，但函数**从不 `clear()`**：每次 `read()` 都把「全部历史字节 + 新块」
   重新丢给 VT parser 和 `reply_to_queries`。后果是每读一块就重复应答一遍
   历史里所有 DSR/DA，子进程 stdin 收到无穷多条重复应答（实测 4 秒回显 80 KB
   垃圾），画面被吃满、输入全废。
   **规则：任何 `*_into(out, ..)` 复用的缓冲，函数入口必须 `out.clear()`。**
   回归测试：`session::tests::strip_orphan_into_overwrites_reused_buffer`
   （同一缓冲连喂两次，结果只含第二次）+ `kitty_push_drops_after_budget`
   （它的旧断言曾把「追加」写成期望值，等于给 bug 背书，改动时勿再写反）。

2. **DSR / 主 DA 被误停答** —— cmd.exe 发出 `\x1b[6n` + `\x1b[c` 后
   **ConPTY 不会替它应答**（本机实测：捆绑 conpty 1.25 与系统内置一致，
   子进程就停在初始化等应答，既不打横幅也不出提示符）。
   现在 `reply_to_queries` 应答：DSR（`\x1b[6n` / `\x1b[?6n`）、
   主 DA（`\x1b[c` → `\x1b[?62;1;2;6;9;15;22c`）、DECRQM、kitty `ESC[?u`、
   主键增强 `ESC[?2;1;0S`、XTWINOPS、OSC 10/11/4；XTVERSION 仍不答
   （没有对端消费它，泄漏为键盘输入纯是噪声）。
   回归测试：`session::tests::reply_to_queries_answers_dsr_and_da`。

> **不要再引用旧结论「ConPTY 会替子进程应答 DA/DSR，宿主一律不应答」——
> 该结论已实测证伪，害得整个终端不启动。**
> 「TUI 输入框出现 CCCC」从来不是 DSR 本身的问题，而是上面第 1 条的重复应答
> （`CCCC` ×N = N 条重复应答）。怀疑任何「ConPTY 会自动应答 X」的说法时，
> 先裸 ConPTY + cmd.exe 实测，不要靠推断。

### 终端被异常终止（子进程自己崩掉：writer_died.log / os error 232）

**已修（2026-10-06）**：不是子进程的问题，是**我们的 reader 线程被上游 panic 带走**，
子进程只是随后写管道失败被误记成“终端异常终止”。

完整链条（crash.log + writer_died.log 实况）：

```
pi/opencode 重初始化狂发 kitty push（CSI > flags u，实测 43 次/秒）
→ alacritty_terminal 0.26 上游笔误 term/mod.rs:1296：
  push_keyboard_mode 栈满 4096 时误对**空的 title_stack** 做 remove(0)
→ panic: removal index (is 0) should be < len (is 0)
→ reader 线程死 → PTY 无人读 + 快照停更（画面永久冻结、进程还活着）
→ 子进程写 stdout 管道报 os error 232 → writer_died.log
```

三处加固：

1. **kitty push 预算 64 封顶**（`KITTY_PUSH_BUDGET`，在
   `strip_orphan_csi_u_bytes_into` 里丢）：栈深恒 < 4096，panic 不可达。
   pop（`ESC[<n u`）按 n **退还预算**——单向递减会让会话活过 64 次 push 后
   kitty 协商永久失效（Enter 键行为退变）。
2. **读循环整体 `catch_unwind`**：解析以外的 panic（快照/命令/尺寸）也不再静默
   带走线程，一律收敛成「会话异常结束」+ 记日志 + 页签变可重开态。
3. **写入侧管道断 → 立即结束会话**：不再“看着活着、敲字没反应”。

回归测试：`unfiltered_kitty_push_flood_still_panics_upstream`（证明上游 bug
仍在、预算封顶是唯一防线，**别删**）、`kitty_push_budget_prevents_terminal_panic`、
`kitty_pop_refunds_push_budget`。

> ponytail：根治要 vendor 一份 alacritty 0.26 打上游笔误补丁（46MB）。
> 预算丢弃对本场景等价（只读栈顶 flags），等哪天必须保住 4096 级真实栈深再加。

### 全局异常日志

`main.rs::log_crash` 是**唯一落盘点**：`exe 同级 crash.log`（带 kind / 线程名 /
pid / 出错位置 / 回溯，4MB 滚动为 crash.log.1）。panic hook、reader 解析 panic、
writer-died 全部经它写入。

> GUI 子系统**没有控制台**（`#![windows_subsystem]`），`eprintln!` 谁也看不见——
> 排查任何异常都必须写文件，别再只打 stderr。也别往 CWD 写日志
> （旧的 `writer_died.log` 就是这么丢的）。

## 技术栈

- UI: [egui / eframe](https://github.com/emilk/egui)（OpenGL/Glow 渲染）
- 终端: [alacritty_terminal](https://github.com/alacritty/alacritty)（解析终端输出）
- 伪终端: [portable-pty](https://github.com/wez/wezterm)（winpty/conpty）
- 原生对话框: [rfd](https://github.com/Polpua/rfd)
