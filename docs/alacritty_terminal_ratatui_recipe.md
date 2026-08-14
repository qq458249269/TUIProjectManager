# Embedding `alacritty_terminal` 0.26.0 inside a ratatui TUI

Code-level integration recipe. Researched against the published 0.26.0 crate
(docs.rs) and the matching GitHub master source (`alacritty_terminal` was
version `0.26.1-dev` on master when verified, so treat master as an exact match
for 0.26.0).

## 1. Dependency setup

```toml
[dependencies]
alacritty_terminal = { version = "0.26.0", default-features = false }  # drops serde (default = ["serde"])
portable-pty = "0.9.0"
ratatui = "0.30.2"   # or whatever your existing version is
```

`alacritty_terminal` always depends on `log`, `parking_lot`, `polling`,
`vte` (0.15, features `std` + `ansi`), `regex-automata`, `unicode-width`,
plus `rustix`/`rustix-openpty` (unix) or `piper`/`miow`/`windows-sys` (windows).
The only lightening available is `default-features = false` (removes serde).

`alacritty_terminal` re-exports vte as `alacritty_terminal::vte`, so you get the
`vte::ansi::Processor` you need without adding a separate vte dependency.

## 2. Core type map (0.26.0, all verified)

| What you need | Exact path | Notes |
|---|---|---|
| `Term<T>` | `alacritty_terminal::Term` | generic over your `EventListener` |
| `Config` | `alacritty_terminal::term::Config` | fields: `scrolling_history`, `default_cursor_style`, `vi_mode_cursor_style`, `semantic_escape_chars`, `kitty_keyboard`, `osc52`; has `Default` |
| Dimensions | `alacritty_terminal::grid::Dimensions` | trait: `columns()`, `screen_lines()`, `total_lines()`; `Grid` and `Term` both implement it |
| Size helper | `alacritty_terminal::term::test::TermSize` | public struct `{ columns, screen_lines }`, implements `Dimensions`, has `TermSize::new(cols, rows)` |
| Grid | `alacritty_terminal::grid::Grid` | `display_iter()`, `iter_from(point)`, `display_offset()`, `Index<Line>`, `Index<Point>` |
| Iterator | `grid::GridIterator` | `Item = Indexed<&'a Cell>`; `.point()`, `.cell()` |
| `Indexed<T>` | `grid::Indexed` | `{ point: Point, cell: T }`, derefs to `T` |
| Cell | `term::cell::Cell` | `c: char`, `fg: Color`, `bg: Color`, `flags: Flags`, `extra: Option<Arc<CellExtra>>`; `zerowidth()` |
| Flags | `term::cell::Flags` | `WIDE_CHAR`, `WIDE_CHAR_SPACER`, `BOLD`, `DIM`, `ITALIC`, `UNDERLINE`, `INVERSE`, `HIDDEN`, `STRIKEOUT`, `WRAPLINE`, underline variants |
| Color | `alacritty_terminal::vte::ansi::Color` | `Named(NamedColor)`, `Spec(Rgb)`, `Indexed(u8)` |
| Palette | `term::color::Colors` | `Index<NamedColor>` -> `Option<Rgb>` (default palette, 0.26 keeps it in `term`) |
| Point | `index::Point` | `{ line: Line, column: Column }`; `Line(i32)`, `Column(usize)` |
| TermMode | `term::TermMode` | bitflags; `SHOW_CURSOR`, `ALT_SCREEN`, `MOUSE_REPORT_CLICK`, `SGR_MOUSE`, etc. |
| Events | `event::{Event, EventListener, VoidListener, WindowSize}` | see section 7 |
| Parser | `alacritty_terminal::vte::ansi::Processor` | vte 0.15, drives `Term` directly |
| EventLoop | `event_loop::{EventLoop, EventLoopSender, Notifier, Msg, State}` | needs crate's own `tty::EventedPty` (see section 6) |

## 3. Creating the Term

```rust
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::term::test::TermSize;

let config = Config::default();
let size = TermSize::new(columns, rows); // your first grid size, e.g. 80x24
let term = Term::new(config, &size, event_proxy); // event_proxy: your EventListener
```

Note: `Term::new` does **not** require `T: EventListener`; but
`Term::renderable_content()` and `Term::resize` need it for some paths — actually
`resize` works without the bound; `renderable_content()` requires
`T: EventListener`. Just always pass a real listener.

`Term::new` signature (0.26): `pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T>`.

There is **no `SizeInfo`** in 0.26 — dimensions are supplied purely via the
`Dimensions` trait, which is why `TermSize` exists. If you prefer, implement
`Dimensions` yourself on a `{ cols, rows }` struct.

## 4. Feeding PTY bytes into the Term

In 0.26.0 there is **no `Term::process_readable`** anymore. The term is driven
through the `vte::ansi::Handler` trait (`impl<T: EventListener> Handler for
Term<T>`), and the crate's own event loop drives it with a `vte::ansi::Processor`:

```rust
// (inside the crate's event loop — this is exactly what State::pty_read does)
state.parser.advance(&mut **terminal, &buf[..unprocessed]); // terminal: &mut Term<U>
```

If you don't use the crate's `EventLoop`, do this in your own PTY reader thread:

```rust
use alacritty_terminal::vte::ansi::Processor;
use std::sync::Arc;
use parking_lot::Mutex; // or alacritty_terminal::sync::FairMutex

let term: Arc<Mutex<Term<MyListener>>> = Arc::new(Mutex::new(term));
let term_thread = term.clone();
let reader = /* portable_pty master.try_clone_reader() */;

std::thread::spawn(move || {
    let mut parser = Processor::default();
    let mut buf = [0u8; 0x10_0000]; // READ_BUFFER_SIZE used by alacritty
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut term = term_thread.lock();
                parser.advance(&mut *term, &buf[..n]);
                // after unlock: request a UI redraw (e.g. send a message
                // through a crossbeam/crossterm channel)
            }
        }
    }
});
```

`Processor::advance` requires `H: Handler`; `Term<T>: Handler` holds whenever
`T: EventListener`, which is why your listener type must implement
`event::EventListener`.

## 5. Rendering the grid into a ratatui buffer

```rust
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::point_to_viewport; // display_offset, Point -> Option<Point<usize>>
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::text::Line;

let content = term.renderable_content(); // requires T: EventListener
let display_offset = content.display_offset;
let mut selection = content.selection;   // Option<SelectionRange>
let cursor = content.cursor;             // RenderableCursor { shape, point }

for indexed in content.display_iter {
    let point = indexed.point();          // grid coords: Line(i32), Column(usize)
    let cell = indexed.cell();            // &Cell

    // Convert grid coords to viewport coords.
    let Some(vp) = point_to_viewport(display_offset, point) else { continue };
    let (row, col) = (vp.line, vp.column);
    if row >= area.height || col >= area.width { continue; }

    // Skip wide-char spacer cells (the glyph is on the WIDE_CHAR cell).
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) { continue; }

    // Resolve fg/bg through the 0.26 colors palette.
    let fg = resolve_color(cell.fg, content.colors, true);
    let bg = resolve_color(cell.bg, content.colors, false);

    // Selection invert.
    let selected = selection.as_ref().map_or(false, |s| s.contains(point));

    let mut style = Style::default()
        .fg(if selected { bg } else { fg })
        .bg(if selected { fg } else { bg });
    if cell.flags.contains(Flags::BOLD) { style = style.add_modifier(Modifier::BOLD); }
    if cell.flags.contains(Flags::DIM) { style = style.add_modifier(Modifier::DIM); }
    if cell.flags.contains(Flags::ITALIC) { style = style.add_modifier(Modifier::ITALIC); }
    if cell.flags.contains(Flags::UNDERLINE) { style = style.add_modifier(Modifier::UNDERLINED); }
    if cell.flags.contains(Flags::INVERSE) { style = style.add_modifier(Modifier::REVERSED); }
    if cell.flags.contains(Flags::HIDDEN) { style = style.add_modifier(Modifier::HIDDEN); }
    if cell.flags.contains(Flags::STRIKEOUT) { style = style.add_modifier(Modifier::CROSSED_OUT); }

    let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
    let symbol = cell.c.to_string();
    buf.set_string(col, row, symbol, style); // will handle the width-2 glyph
    // Optionally clear the second column manually.
    if width == 2 && col + 1 < area.width {
        buf.get_mut(col + 1, row).set_symbol("").set_style(style);
    }
    // Zero-width combining chars:
    for zc in cell.zerowidth().into_iter().flatten() {
        // append to previous cell's symbol
    }
}

// Cursor
let shape = cursor.shape; // CursorShape: Block, Underline, Beam, HollowBlock, Hidden
if !term.mode().contains(TermMode::SHOW_CURSOR) { /* hidden */ }
let cur = term.grid().cursor.point; // or cursor.point
// convert and set reversed style at that cell
```

`resolve_color` (0.26 palette model):

```rust
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

fn resolve_color(color: Color, palette: &Colors, is_fg: bool) -> RColor {
    let rgb = match color {
        Color::Spec(rgb) => Some(rgb),
        Color::Indexed(i) => palette[i as usize],
        Color::Named(n) => palette[n],
    };
    match rgb {
        Some(Rgb { r, g, b }) => RColor::Rgb(r, g, b),
        None if is_fg => RColor::White, // or palette default
        None => RColor::Black,
    }
}
```

`Colors` has `Index<NamedColor>` and `Index<usize>`; entries are `Option<Rgb>`
(None = use a default). `NamedColor::Foreground`/`Background` are variants too.

## 6. Resizing, and the crate's own EventLoop vs. manual loop

**Crate `EventLoop`:** `event_loop::EventLoop::new(terminal: Arc<FairMutex<Term<U>>>, event_proxy: U, pty: T, drain_on_exit, ref_test)` where `T: tty::EventedPty + event::OnResize + Send + 'static`. `tty::EventedPty` is **not** implemented by portable-pty's `MasterPty` — it's the crate's own abstraction over `polling::Poller` (`register`/`reregister`/`deregister` + `next_child_event`). `Msg::Input`, `Msg::Resize(WindowSize)`, `Msg::Shutdown`; `EventLoopSender` to send them; `Notifier` implements `Notify`/`OnResize`. `WindowSize { num_lines, num_cols, cell_width, cell_height }` (all `u16`).

**Recommended for portable-pty:** skip the crate's EventLoop and run your own
reader thread (section 4) + a writer handle. This avoids implementing
`EventedPty`. portable-pty gives you blocking `Read` (via `master.try_clone_reader()`) and `Write` (via `master.take_writer()`); a plain blocking read loop is the simplest correct approach and is what the recipe above does.

**Resize path** (call both the pty and the term):

```rust
use portable_pty::PtySize;

fn resize(term: &mut Term<L>, master: &portable_pty::MasterPty, cols: u16, rows: u16) {
    master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).unwrap();
    term.resize(TermSize::new(cols as usize, rows as usize));
}
```

`Term::resize<S: Dimensions>(&mut self, size: S)` — confirmed 0.26 signature.

## 7. EventListener and the Event enum

```rust
use alacritty_terminal::event::{Event, EventListener};

#[derive(Clone)]
struct MyListener {
    tx: std::sync::mpsc::Sender<Event>,          // to your UI thread
    pty_writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>, // master.take_writer()
}

impl EventListener for MyListener {
    fn send_event(&self, event: Event) {
        match &event {
            Event::PtyWrite(text) => {           // term wants to write to PTY
                let _ = self.pty_writer.lock().write_all(text.as_bytes());
            }
            Event::Wakeup => { /* redraw request */ }
            Event::Title(title) => { /* set window title */ }
            Event::Bell => { /* flash */ }
            Event::ChildExit(_) => { /* child died */ }
            Event::ClipboardStore(_, _) => { /* OSC52 */ }
            _ => {}
        }
        let _ = self.tx.send(event); // if you want async handling
    }
}
```

`VoidListener` exists (ignores everything) — good for tests, but you must handle
`Event::PtyWrite` in production (cursor-position reports, DA, etc.).

`Event` variants (0.26): `MouseCursorDirty`, `Title(String)`, `ResetTitle`,
`ClipboardStore(ClipboardType, String)`, `ClipboardLoad(ClipboardType, Arc<dyn Fn(&str)->String>)`,
`ColorRequest(usize, Arc<dyn Fn(Rgb)->String>)`, `PtyWrite(String)`,
`TextAreaSizeRequest(Arc<dyn Fn(WindowSize)->String>)`, `CursorBlinkingChange`,
`Wakeup`, `Bell`, `Exit`, `ChildExit(ExitStatus)`.

## 8. Input: keys and mouse

alacritty_terminal does **not** encode keys; you send raw bytes to the pty
writer:

```rust
// key -> bytes: implement per your needs, e.g. Enter = b"\r", Ctrl+C = b"\x03",
// Esc = b"\x1b", arrows = b"\x1b[A" etc. (or use a helper like termwiz/crossterm keys).
master_writer.write_all(&bytes)?;
```

Mouse: alacritty_terminal tracks modes but you generate the SGR sequences:

```rust
let mode = *term.mode();
if mode.contains(TermMode::SGR_MOUSE) {
    // \x1b[<{btn};{col+1};{row+1}M  (press) / m (release)
    let msg = format!("\x1b[<{btn};{};{}M", col + 1, row + 1);
    writer.write_all(msg.as_bytes())?;
} else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
    // legacy \x1b[M.. encoding
}
```

## 9. `os-terminal` verdict

**Not relevant for a ratatui TUI.** `os-terminal` (0.7.x) is a `no_std`
terminal emulator for embedded systems / OS kernels. It renders pixels into a
framebuffer via `DrawTarget`/`Palette` and does its own bitmap font rendering
(noto-sans-mono-bitmap or swash/ab_glyph for truetype). It consumes `vte` 0.15
like alacritty, but its output model (pixel `Rgb` per framebuffer position) does
not map onto ratatui's cell/Buffer model, and it cannot run in a normal
userspace terminal window. Skip it.

## 10. Reference implementation

`Harzu/iced_term` (now `kemokempo/iced_term`) is a working iced widget embedding
`alacritty_terminal` — it uses the crate's own `EventLoop` + `tty::new`, wraps
the terminal in `Arc<FairMutex<Term<EventProxy>>>`, uses a `Notifier` to write
`Msg::Input`, handles `Event::PtyWrite`, does selection via `selection::Selection`
and `SelectionRange`, encodes SGR mouse reports, and renders by cloning the grid.
Good reference for the full feature surface (its dependency is iced, but the
alacritty-facing code ports directly to ratatui).

## Quick checklist

- [ ] `alacritty_terminal = "0.26"` (consider `default-features = false`)
- [ ] `portable-pty = "0.9"`
- [ ] Implement `EventListener` (must handle `Event::PtyWrite`)
- [ ] Create `Term` with `Term::new(config, &TermSize, listener)`
- [ ] Reader thread: `vte::ansi::Processor::advance(&mut *term, &buf)`
- [ ] Render from `term.renderable_content().display_iter`, skip `WIDE_CHAR_SPACER`
- [ ] `term.resize(TermSize::new(...))` + `master.resize(PtySize{..})` on layout change
- [ ] Write raw bytes to `master.take_writer()` for input
- [ ] Handle `TermMode::SGR_MOUSE`/`MOUSE_REPORT_CLICK` for mouse reporting