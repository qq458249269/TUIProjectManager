# 项目开发注意事项

## 终端滚动渲染（关键架构约束）

### snapshot 机制
- `Session.snapshot` 是 `AtomicPtr<TermSnapshot>`，reader 线程处理 PTY 输出后原子交换
- `Session.snapshot_scratch` 是 `Vec<(Point, Cell)>`，供 `render_cells` 复用，避免每帧 clone
- `Session.parse_gen` 是 `AtomicU64`，reader 线程每次处理输出后 `fetch_add(1)`

### refresh_snapshot 必须从终端 grid 读 display_offset
**禁止**从旧 snapshot 指针读 offset：`(*cur_ptr).offset` 是旧 snapshot 的值，滚动时 snapshot 未更新，`offset_changed` 永远 false → 纯滚动时 `refresh_snapshot` 直接 return → 渲染冻结。

**正确做法**：`term.read().grid().display_offset()` 从终端 grid 直接读当前 offset。

### render_cells 的 skip_render_loop
- `cached_render_shapes` 存上帧完整 Shape 列表，`gen_changed=false` 且缓存存在时跳过逐格渲染
- 任何 offset/gen 变化后必须 `sess.cached_render_shapes = None`，否则跳过渲染用旧缓存
- 滚动后 `cached_render_shapes` 必须清除

### snapshot_scratch 生命周期
- `render_cells` 开头：`gen_changed=true` 时从 `snap.cells` clone；`false` 时 `std::mem::take(&mut sess.snapshot_scratch)`
- `render_cells` 结尾：`sess.snapshot_scratch = snapshot_cells` 归还
- `refresh_snapshot` 更新时：先存 `snapshot_scratch`，再 `clone()` 给 snapshot

## 跟随系统主题

**禁止**依赖 `ctx.system_theme()` 检测系统深浅：egui 依赖 `WM_SETTINGCHANGE` 消息，窗口未激活/消息丢失时返回 `None`，导致跟随系统失效。

**正确做法**：直接读 Windows 注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`（0=深色，1=浅色），见 `query_windows_dark_mode()`。

## 帧率控制（关键：空闲时必须停帧，交互时必须恢复）

### 核心原则
**空闲时停止重绘**：无变化时不调 `request_repaint_after` → egui 停止唤醒 → GPU ~0%。
**所有视觉变化必须恢复帧率**：任何导致画面变化的操作都要确保下一帧被调度，否则用户看到冻结画面。

### 帧率调度逻辑（`logic()` 方法）
```rust
let redraw_count = self.redraw_rx.try_iter().count();
let pointer_down = ctx.input(|i| i.pointer.any_down());
if is_foreground_term {
    if redraw_count > 0 || pointer_down {
        // 有终端输出 或 鼠标按住（选区拖动）→ 调度下一帧
        ctx.request_repaint_after(Duration::from_millis(1000 / fps));
    }
    // 否则不调度：SessionListener 在下次 PTY 输出时唤醒
} else {
    // 首页/设置页：2s 基线轮询
    ctx.request_repaint_after(Duration::from_secs(2));
}
```

### 各操作的帧率恢复方式
| 操作 | 帧率恢复方式 |
|------|-------------|
| PTY 有输出（打字回显、命令输出） | `SessionListener.send_event()` 调 `ctx.request_repaint()` |
| 鼠标按住拖动选区 | `logic()` 检测 `ctx.input pointer.any_down()` 调度帧率 |
| 滚动终端 | `show_terminal` 滚动后调 `ui.ctx().request_repaint()` |
| 主题切换 | `logic()` 检测 `cur_dark != last_theme_dark` 后调 `ctx.request_repaint()` |
| 会话退出 | `logic()` 检测 `update_exited()` 后调 `ctx.request_repaint()` |
| 更新检查结果 | `logic()` 检测 `update_rx.try_recv()` 后调 `ctx.request_repaint()` |
| 键盘输入 | 字节发 PTY → reader 线程处理输出 → SessionListener 唤醒 |
| 窗口 resize | egui 自身触发新帧 |
| 首页/设置页操作 | 无基线轮询，egui 输入处理自动唤醒 |

### show_terminal 中必须调 request_repaint 的场景
`show_terminal` 修改了视觉状态但不经过 PTY 通路时，必须调 `ui.ctx().request_repaint()`：
- 选区开始（`drag_started_by`）
- 选区拖动更新（`dragged_by`）
- 选区清除（纯点击，无拖动位移）
- 滚动（`scroll_display` 后）
- 鼠标按住或有活跃选区时（兜底刷新）

**根因**：空闲停帧后，只有 PTY 输出（SessionListener）和 `logic()` 中的检测逻辑会唤醒渲染。选区操作在 `show_terminal` 内直接修改 `term.selection`，不走 PTY 通路，SessionListener 不会触发。如果不调 `request_repaint()`，当帧不刷新 → 用户看到选区不出现，直到做其他操作（键盘/滚动）才突然显示。

**关键约束**：`logic()` 在 `ui()` 之前执行，`ctx.input()` 读到的指针状态可能不反映当前帧的实际交互。鼠标操作的帧率恢复必须在 `show_terminal` 内部（`ui` 上下文可用时）检测并触发，不能依赖 `logic()` 中的 `ctx.input()`。

### 选区渲染必须从终端实时读取 sel_range
**禁止**用快照的 `snap.sel_range` 做选区高亮：快照只有 reader 线程刷新，用户拖动选区不会更新快照 → 渲染用旧选区 → 看不到新选区。

**正确做法**：渲染时从终端实时读取选区范围（`sess.term.try_read()` + `selection.to_range()`），并检测选区变化后清 `cached_render_shapes` 强制重绘：
```rust
let sel_range = sess.term.try_read().ok().and_then(|t| {
    t.selection.as_ref().and_then(|s| s.to_range(&t))
});
if sel_range != snap.sel_range {
    sess.cached_render_shapes = None; // 选区变化 → 强制重绘
}
```

## 清空输入（ClearInput）

**禁止**用 `\x1b[F\x15`（End + Ctrl+U）：Ctrl+U 只清当前显示行，多行输入（`\\n` 分隔的逻辑行）时前面的行残留。

**正确做法**：`\x03` + 100次`\x15`（Ctrl+C + 多次 Ctrl+U）
- `\x03` Ctrl+C：取消整个输入（bash/zsh 多行模式下取消全部行）
- `\x15` Ctrl+U：清行刷新提示符，重复 100 次确保多行残留内容全部清除

## 选区点击清除必须区分纯点击与拖动结束
**禁止**在 `resp.clicked()` 时无条件清除选区：用户按下→拖动→释放时，释放帧 `clicked()=true` 会把刚创建的选区清掉。

**禁止**用 `pointer.press_origin()` 判定纯点击：该方法在释放帧返回 `None`（egui 事后清除状态），导致 `is_true_click` 永远 false → 选区永不清除。

**正确做法**：自己从 raw events 捕获按下坐标（`click_press_pos`），释放时比较位移：<4px 为纯点击才清除选区：
```rust
// 按下时记录位置
if primary_pressed {
    sess.click_press_pos = ui.input(|i| i.pointer.latest_pos());
}
// 释放时判断纯点击
let is_true_click = primary_released && sess.click_press_pos.map_or(false, |p0| {
    latest_pos.map_or(false, |p1| p1.distance(p0) < 4.0)
});
if resp.clicked() && is_true_click {
    t.selection = None;
}
```

### 禁止事项
- **禁止**在 `logic()` 中无条件调 `request_repaint_after`（导致空闲时 GPU 满载）
- **禁止**添加任何不检查活动状态的定时重绘
- **禁止**在 `show_terminal` 中为 PTY 输出相关的事件调 `request_repaint`（应由 SessionListener 负责）

### 首页/设置页
- 不调 `request_repaint_after`（无基线轮询）
- egui 输入处理自动唤醒渲染循环（用户交互时）
- 更新检查结果在用户交互时自然被消费

### 后台页签
- 不消费 redraw 信号，不触发 repaint
- `show_terminal` 不被调用（只渲染当前页签）
- 后台会话输出照常解析（管道不能停读），但不唤醒 UI
