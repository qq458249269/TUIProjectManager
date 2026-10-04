# 重构语言选型分析（高性能 / 低消耗 / 低内存 / 编译快 / 产物体积小）

## 0. 结论先行

**不换语言。** Rust 已是此项目五项指标上的局部最优解；换语言只能买到「重写 17k 行」的成本，
换不回任何一项硬指标的真实收益。
不在语言本身。

---

## 1. 现状基线（本仓库实测，非估算）

| 指标 | 实测值 |
| --- | --- |
| 语言 / 框架 | Rust 2024 + eframe/egui 0.36（glow + wgpu 双后端） |
| 自有代码 | 17,141 行（`src/`，不含 tests/build.rs） |
| 测试代码 | 1,832 行 |
| 传递依赖 | `Cargo.lock` 417 个 = 解析全集（含未激活可选依赖）；**实际编译 192 个** |
| Release 产物 | **11.8 MB**（已 `lto` + `strip` + `opt-level=2` + `codegen-units=1`） |
| Debug 产物 | 135.7 MB（dev 依赖已 `opt-level=3`，仅行号调试信息） |
| 热路径 | 终端逐格渲染、10fps 主循环、字形图集 1024² RGBA（≈4MB 内存 + 4MB 显存，全进程共享） |
| 单页签回看历史 | 1000 行 ≈ 3.8 MB（可配 100..=5000） |
| 常驻内存大头 | 字形图集 4MB + 每页签回看 3.8MB + wgpu/DX12 设备（Intel 路径）或 glow（AMD/NVIDIA） |

**判断：11.8 MB / 释放后约 20–40 MB RSS，在「带 GPU 渲染的桌面 GUI + ConPTY + VT 解析 + 字体栅格化」
这一类程序里已属轻量档**（同类的 Windows Terminal ~30–80 MB、Electron 150 MB+、WPF/.NET 80 MB+）。
语言层已无可压空间，剩下的体积全在依赖。

---

## 2. 判据（按项目实际负载排序）

1. **无 GC 停顿**：终端逐格渲染 + 10fps 帧循环，任何 GC 抖动 = 可见的输入延迟。
2. **可控内存**：字形图集 / 回看缓冲需要确定的峰值，`Vec`/arena 可预算，GC 堆不可。
3. **产物体积**：便携单 exe（项目明确要求「Release 双击即用，无外部依赖」）。
4. **编译反馈速度**：日常迭代的是 UI 与终端逻辑，编译须在数十秒内。
5. **Win32 原生能力直连**：ConPTY、`CF_HDROP` 剪贴板、原生文件夹选择框、ConPTY 首次运行解包逻辑。

---

## 3. 候选语言评分

图例：★ 越多越好；✕ 为该语言的硬伤（本项目场景下）。

| 语言 | 无 GC | 产物体积 | 编译速度 | 内存可控 | 原生能力直连 | 生态（VT 解析/字体/ConPTY） | 总评 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **Rust（现状）** | ★★★★★ | ★★★★☆ 11.8MB | ★★★ 依赖 417 个偏慢 | ★★★★★ | ★★★★★ 直接 `-sys` | ★★★★★ `alacritty_terminal`/`portable-pty`/`fontdue` 现成 | **保留** |
| C / C++ | ★★★★★ | ★★★★★ ~1–3MB | ★★★★ 单文件极快 | ★★★★★ | ★★★★★ Win32 原生 | ★★☆ 终端与字体全需自研（现成轮子皆绑定 GUI 库） | 见 §5 |
| Zig | ★★★★★ | ★★★★★ ~2–5MB | ★★★★★ 增量最快 | ★★★★★ | ★★★★ `@cImport` 绑 Win32 | ★☆☆☆ 无 | 见 §5 |
| Go | ✕ GC | ✕ ~15–25MB | ★★★★★ | ✕ 基线 RSS 数十 MB | ✕ ConPTY/DirectWrite 须 cgo 或纯 syscall | ★★★ `creack/pty` 仅 POSIX，ConPTY 无成熟件 | **淘汰** |
| C++/WinUI3、.NET/Avalonia | ✕ GC | ✕ 30–80MB + 运行时 | ★★★ | ✕ | ★★★★★ | ★★★★ 现成但重 | **淘汰**（GUI 栈比本项目现有栈重一个数量级） |
| C#/WinForms | ✕ GC | ✕ 需 .NET 运行时 | ★★★★ | ✕ | ★★★★ | ★★★ | **淘汰** |
| D | ★★★★★ | ★★★★ | ★★★ | ★★★★★ | ★★★★ | ★★☆ | 无收益（与 Rust 同代同价，无生态差） |
| Java/Kotlin | ✕ GC | ✕ JVM | ★★★★ | ✕ | ✕ | ★★☆ | **淘汰** |

**唯一未被硬伤淘汰的备选：C/C++ 与 Zig** —— 但它们只赢在「产物体积再省 8 MB 左右」，
代价是重写 VT 解析器、字形栅格化管线、ConPTY 会话管理、OpenGL/DX12 渲染后端、GUI 控件层，
以及**丢掉 Rust 的 `catch_unwind` 页签崩溃隔离**（C++ 需 SEH，C 需手工 longjmp + 状态一致性处理，
两者都比现有实现脆弱）。收益 8 MB，成本 ≈ 全量重写 + 长期失去内存安全。不成立。

---

## 4. 「417 个依赖能清吗」——实测答案

**先纠正基数**：`Cargo.lock` 的 417 是**解析全集**（含未激活的可选依赖、其它平台依赖），
**实际参与本项目编译的只有 192 个**：

```bash
cargo tree -e normal,build --prefix none --no-dedupe | sort -u | wc -l   # → 192
```

（`target/release/.fingerprint` 交叉验证一致。）「417 → 192」这 225 个的差额不是浪费，
而是 Cargo.lock 与语言无关的设计使然，改语言也一样躲不掉。

**直接依赖无一闲置**：12 个直接依赖在 `src/` 中全部有真实引用
（`serde_yaml` 4 处、`fontdue` 15 处、`rfd` 5 处、`raw-window-handle` 1 处……）。
能动的只有 feature 开关，实测收益如下（`cargo tree` 计数，改完即还原）：

| 动作 | 编译 crate 数 | 实测代价 / 阻塞点 |
| --- | --- | --- |
| 现状（glow + wgpu 双后端） | **192** | — |
| 只留 glow（`features = ["glow"]`） | **155（−37）** | **编译失败**：需改 `src/main.rs` 约 40 行（GPU 厂商探测 + DX12 回退 + `WgpuSetup` 分支，实测 6 个 E0433/E0599/E0609）。且 Intel 老 iGPU 用户失去 DX12 通路 = 功能回归 |
| 只留 wgpu | 188（−4） | 相对现状几乎无收益，且 AMD/NVIDIA 全家失去省 ~200MB 的 glow 通路 |
| 去掉 `serde_yaml` | 151（−4，叠加于 glow-only） | oh-my-pi 的 `models.yml` 供应商配置功能直接消失（功能损失，非纯瘦身） |
| 去掉 `image` / `png` / `moxcms` | −8 量级 | 不可行：`egui-winit` 把 `arboard` 的 `image-data` 写死为强制 feature，只能 vendor 改上游 |
| 去掉 `serial2` / `shell-words` | −2 | `portable-pty` 无条件依赖（Windows 上也编译），无 feature 可关，只能 vendor patch |

**结论：能清，但清不出什么。**唯一有量的是 wgpu 一族（−37 crate，rlib 粗估 `ash` 67MB +
`naga` 22MB + `wgpu_core` 14.5MB + `wgpu_hal` 11.4MB，是首编译时间的真正大头），
而它是用 Intel 老显卡的可运行性换来的 —— 不划算。其余全是锁死的上游 feature 或功能本身。

> 编译速度的瓶颈与语言无关：换 Go 能把 3 分钟降到 30 秒，
> 但那是拿运行期内存和产物体积去换，且丢 5 条既成功能（见 §3 Go 行）。若编译速度是首要痛点，
> 正确做法是拆分 crate + 拆分 `default-features`，不是换语言、也不是删依赖。

### 附：同一项体积/编译预算的真实分配（`.rlib` 粗估，未计 LTO 去重）

| 家族 | 成员 | 合计 rlib | 说明 |
| --- | --- | --- | --- |
| wgpu 族 | ash / naga / wgpu_core / wgpu_hal / wgpu-types / naga-types | ≈ 120 MB | 由 `eframe` 的 `wgpu` feature 带入，删之即整族消失 |
| windows 族 | windows / windows-sys / windows_* / windows_x86_64_* | ≈ 80 MB | 被 `arboard`/`rfd`/`wgpu` 共同依赖，删不掉 |
| 字体栈 | read-fonts / skrifa / harfrust / fontdue / ttf-parser | ≈ 40 MB | **四套并存**：`fontdue` 是本项目直接用，其余三套来自 `epaint`，无法裁剪（epaint 默认路径） |
| 无障碍 | accesskit / accesskit_winit | 小 | **不得删**（无障碍属不可简化项） |

---

## 5. 极端方案备忘（若指标排序与本文件相反）

若「产物体积必须 < 3 MB」成为第一优先，唯一可行路线是 **C + Win32 + Direct2D/DirectWrite（或裸 OpenGL）**：
- 可丢弃 `egui`/`eframe`/`wgpu`/`fontdue`/`alacritty_terminal`/`portable-pty`/`rfd`/`serde` 全链，自实现控件与 VT 状态机；
- 预计重写量 **15k–20k 行 C**，其中 VT 解析器 + 网格化着色器状态机约 3k 行（`alacritty_terminal` 的核心移植量）；
- 崩溃隔离退化为 SEH（GUI 线程）/ `longjmp`（PTY 读线程），需自行保证资源与状态一致；
- 收益：exe ~2–4 MB，RSS 再降 10–20 MB；
- 成立条件：**只有**在「体积优先于一切」且愿意承担全部维护成本时。
  在当前需求（GUI 工具、单机、内存充裕）下不成立。

---

## 6. 若仍要验证（spike，而非全量重写）

一次性、单文件验证脚本，约 200 行，逐条实测而非纸面比较：
1. 同一段 ConPTY 读写 + VT 解析循环，分别在 Rust 与候选语言下跑 10 分钟高输出压测，比较**峰值 RSS 与 p99 帧耗时**；
2. 空窗口 + 一个终端页签的**空闲 RSS** 对比（这是用户实际感知的「低消耗」）；
3. `cargo build --release` 冷/热耗时 vs 候选语言增量构建耗时；
4. 产物大小用 `ls -l` 对比。

**停止条件**：候选语言的空闲 RSS 未能低于现状 30%，或 p99 帧耗时更高 → 立即放弃，回到 §4 的依赖瘦身清单。