# 异常退出记录：kitty 键盘模式 push 洪水导致 reader 线程 panic

## 现象

`crash.log`（exe 同级，全局 panic 钩子落盘）：

```
panicked at alacritty_terminal-0.26.0\src\term\mod.rs:1296:44:
removal index (is 0) should be < len (is 0)
```

连锁症状：reader 线程死 → PTY 无人读 → 快照停更 → 画面永久冻结 → 子进程写管道报错
（os error 232）→ UI 标「终端被异常终止」。

## 根因

alacritty_terminal 0.26.0 `src/term/mod.rs:1296`：`push_keyboard_mode` 判断栈深超过
`KEYBOARD_MODE_STACK_MAX_DEPTH`（= `TITLE_STACK_MAX_DEPTH` = 4096）时，错误地对
`title_stack` 做 `remove(0)`（应为 `keyboard_mode_stack`）。title 栈通常为空 →
越界 panic。

触发：全屏 TUI（实测 pi 重启约 43 次/秒，每次一个 push）反复推
`ESC [ > flags u`，约 95 秒栈满必触。

## 方案演进

1. **9e74b7b（防御，已上线无异常退出）**：reader 在解析前 strip 转发的 kitty push，
   `KITTY_PUSH_BUDGET = 64` 限栈深；pop（`ESC [ < n u`）按 n 退还预算。效果等价但属
   症状缓解。
2. **当前（根治）**：`vendor/alacritty_terminal-0.26.0` 拷贝上游 0.26.0，第 1296 行
   改为 `self.keyboard_mode_stack.remove(0)`；`Cargo.toml` `[patch.crates-io]` 指向
   vendor 版。strip 预算（`KITTY_PUSH_BUDGET` 及全部相关代码/测试）随即删除：
   上游修复后无意义，留着只是维护负担。

## 验证

- 回归测试 `unfiltered_kitty_push_flood_no_longer_panics`：不经 strip 直接灌 5000 次
  push，vendor 版下不得 panic（防 vendor patch 被意外还原）。
- `cargo test` 全量通过（136 + 33）。
