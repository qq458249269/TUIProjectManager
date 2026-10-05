//! 终端字形批渲染：字形预光栅化进一张纹理图集，格子直接拼 quad 提交，
//! 跳过逐格 Galley 排版与 epaint tessellation（终端区每帧上万三角形的
//! 主开销）。走 egui 自身纹理 + Mesh 管线，无自定义 shader/GL 调用：
//! 核显、软件渲染（WARP/llvmpipe）与普通 UI 元素同一条路，能跑本程序
//! 就能用。图集缺字形（emoji 等）的格子自动回落原 galley 路径，逐格混合。
//!
//! 静止帧优化：每格算 FNV 哈希入表，与上一帧全等则直接重放缓存的 Mesh，
//! 跳过 quad 重建；内容一变即整帧重建（不做局部更新——重建本身已是微秒级）。
//!
//! 多页签：字形位图与纹理**全进程共享**（同字体链 + 物理字号一份，见
//! SHARED_ATLASES），页签各留的只有与自身屏幕内容相关的帧缓冲。
//!
//! 内存控制（实测驱动）：
//! - 分页图集：单页 1024² RGBA ≈ 4MB，满页开新页渐进，不做整体清空重灌
//!   （整体清空曾造成汉字输入 ~400ms 卡顿）。
//! - 常驻字体链：fontdue::from_bytes 是全字形 eager 解析，CJK ~30K 字形
//!   几何 ≈ +127MB/字体。首次建图集即整条链拉满（含汉字字符集，进程级
//!   缓存 Arc 共享，多页签零重复解析）；首个缺字形不再中途补链 → 渲染/输入
//!   永无卡顿峰值。代价：CJK 解析结果常驻内存（用户明确要求常驻加载）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use alacritty_terminal::vte::ansi::{Color, Rgb};
use eframe::egui;
use egui::{Color32, ColorImage, Mesh, Pos2, Rect, TextureHandle, TextureOptions, Vec2};

/// 图集边长（物理像素）。1024×1024 RGBA ≈ 4MB 显存；CJK 字形约 28px 高，
/// 可容纳上千个不同字符，满页开新页（页号随 quad 记录）。
const ATLAS_SIZE: u32 = 1024;
/// 字形位图之间的间隔像素：LINEAR 采样时防止相邻字形边缘渗色。
const GLYPH_PAD: u32 = 1;

// ── 进程级 fontdue 解析缓存 ──
// fontdue::Font::from_bytes 解析 msyh.ttc（约 20MB）需要 ~400ms；
// 每个 Session 首帧都会重建 TermGpu，重启/新开页签都在 UI 线程重复解析
// → 重启卡顿。fontdue::Font 不可变，解析结果可跨会话共享（Arc 引用）。
// key = 原始字节序列（会话间恒定，进程内只解析一次）。
/// 原始字体源：(文件字节, ttc 子索引)。
type FontSource = (Arc<Vec<u8>>, u32);
type FontCacheEntry = (Vec<FontSource>, u32, Vec<Arc<fontdue::Font>>);
static FONT_CACHE: OnceLock<Mutex<Vec<FontCacheEntry>>> = OnceLock::new();
// 全链 fontdue 解析（CJK）≈130-170MB/条（字体轮廓 eager 解析），且按物理字号
// （px）各存一条：窗口拖动跨 DPI 显示器缩放时会叠出多条。上限 4 条 = 潜在
// 500MB+ 白吃；同进程 DPI 稳定时实际只有 1 条，降为 2 只牺牲极端场景下的
// 跨 px 复用，保住内存上限。
const FONT_CACHE_MAX: usize = 2;

/// 进程级字体字节缓存：egui 的 FontData 是 Cow::Owned，`definitions().clone()`
/// 会深拷贝整份字体，而 TermGpu::new 每次新页签都调一次 → 每页签白吃
/// ~20MB。首次从 egui 提取一次后存入 Arc，后续会话 Arc clone 零拷贝。
static FONT_DATA_CACHE: OnceLock<Mutex<Vec<FontSource>>> = OnceLock::new();

fn cached_font_data(ctx: &egui::Context) -> Vec<FontSource> {
    let cache = FONT_DATA_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = cache.lock().unwrap();
    if guard.is_empty() {
        let defs = ctx.fonts_mut(|f| f.definitions().clone()); // 仅首次深拷贝
        if let Some(mono) = defs.families.get(&egui::FontFamily::Monospace) {
            for name in mono {
                if let Some(d) = defs.font_data.get(name) {
                    guard.push((Arc::new(d.font.clone().into_owned()), d.index));
                }
            }
        }
    }
    guard.clone() // Arc clone，零拷贝
}

/// 解析 `sources[..take]` 中每个字体为 fontdue::Font（跳过失败项）。
fn parse_fonts(sources: &[FontSource], px: f32, full: bool) -> Vec<Arc<fontdue::Font>> {
    let take = if full {
        sources.len()
    } else {
        1.min(sources.len())
    };
    let mut fonts: Vec<Arc<fontdue::Font>> = Vec::new();
    for (data, index) in &sources[..take] {
        let settings = fontdue::FontSettings {
            collection_index: *index,
            scale: px,
            load_substitutions: false,
        };
        if let Ok(f) = fontdue::Font::from_bytes(data.as_slice(), settings) {
            fonts.push(Arc::new(f));
        }
    }
    fonts
}

/// 从进程级缓存取（或解析后缓存）fontdue 解析结果。
/// 命中条件：字体列表字节级全等 + 物理字号一致。
///
/// `full=false` 已无调用方（曾用于懒加载：先只解析链首，见 GlyphAtlas::new）；
/// 现恒以 `full=true` 全量解析整条链进缓存，条目就地升级为全量。
/// ponytail: 参数与升级分支可随懒加载机制一并移除。
fn cached_fonts(sources: &[FontSource], px: f32, full: bool) -> Vec<Arc<fontdue::Font>> {
    let cache = FONT_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = cache.lock().unwrap();
    if let Some(idx) = guard
        .iter()
        .position(|(sources2, px2, _)| *px2 == px as u32 && sources2 == sources)
    {
        let (_, _, fonts) = &guard[idx];
        if !full || fonts.len() >= sources.len() {
            return fonts.clone();
        }
        // full 但缓存只有链首（懒加载未完成）：全量解析并升级缓存条目。
        let full_fonts = parse_fonts(sources, px, true);
        guard[idx].2 = full_fonts.clone();
        return full_fonts;
    }
    guard.push((
        sources.to_vec(),
        px as u32,
        parse_fonts(sources, px, full),
    ));
    if guard.len() > FONT_CACHE_MAX {
        guard.remove(0);
    }
    guard.last().unwrap().2.clone()
}

// 纹理上传统计（TUIPM_TEX_DEBUG=1 启用）：验证「脏矩形上传」真的把
// 每帧 4MiB 整页重传降成了单字形级别的小区域。
static TEX_UPLOAD_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TEX_UPLOAD_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tex_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("TUIPM_TEX_DEBUG").is_ok())
}

fn count_upload(px: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let n = TEX_UPLOAD_N.fetch_add(1, Relaxed) + 1;
    TEX_UPLOAD_BYTES.fetch_add(px as u64, Relaxed);
    if tex_debug() && n.is_multiple_of(100) {
        eprintln!(
            "[tex] uploads={n} avg={:.1}KiB",
            TEX_UPLOAD_BYTES.load(Relaxed) as f64 / n as f64 / 1024.0,
        );
    }
}

/// 单个字形的图集记录。UV 指向图集内的位图矩形；
/// dx 相对格子左缘
#[derive(Clone, Copy)]
pub struct GlyphSlot {
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub dx: f32,
    pub dy: f32,
    /// 位图尺寸（逻辑点）。w == 0 表示空槽：无可见笔画或所有字体都缺该字形，
    /// 命中缓存但不出 quad（调用方据此回落 galley 路径）。
    pub w: f32,
    pub h: f32,
}

impl GlyphSlot {
    const EMPTY: Self = Self {
        u0: 0.0,
        v0: 0.0,
        u1: 0.0,
        v1: 0.0,
        dx: 0.0,
        dy: 0.0,
        w: 0.0,
        h: 0.0,
    };
}

/// 字形纹理图集：动态 shelf 打包，按需光栅化。
pub struct GlyphAtlas {
    fonts: Vec<Arc<fontdue::Font>>,
    /// 懒加载源（全链字节）：缺字形时补全字体链用。
    lazy_sources: Vec<FontSource>,
    /// 懒加载已完成（或字体链本就单条）。标记后不再重复尝试。
    lazy_done: bool,
    px: f32,
    ppp: f32,
    ascent_px: f32,
    descent_px: f32,
    w: u32,
    h: u32,
    /// RGBA 位图；(0,0) 保留为纯白素。
    rgba: Vec<u8>,
    cx: u32,
    cy: u32,
    rh: u32,
    /// 页满标记：放不下任何新字形。分页打开新页，不再整体清空重灌。
    full: bool,
/// char → 槽位（含负缓存：w==0 的空槽）。
    map: HashMap<char, GlyphSlot>,
    /// 内容版本：光栅化新字形后 +1，纹理已上传方据此决定是否重传。
    version: u64,
    /// 自上次上传以来被写脏的像素矩形（含 (0,0) 白素区）。None = 整页上传
    /// （新建页）。上传成功后由 page_texture 清空。
    dirty: Option<Rect>,
}

impl GlyphAtlas {
    /// `font_data`：(字体文件字节的 Arc，ttc 子索引)。解析失败的字体跳过。
    pub fn new(font_data: &[FontSource], font_size_pt: f32, ppp: f32) -> Self {
        let px = font_size_pt * ppp;
        // 常驻加载：建图集即整条链拉满（Hack + CJK/emoji），进程级 FONT_CACHE
        // Arc 复用，多页签不重复解析。首个缺字形不再中途补链 → 渲染/输入永无
        // 卡顿峰值。代价：CJK ~127MB 解析结果常驻内存（用户要求常驻）。
        let fonts = cached_fonts(font_data, px, true);
        let mut ascent_px = px * 0.8;
        let mut descent_px = -px * 0.2;
        if let Some(f) = fonts.first()
            && let Some(lm) = f.horizontal_line_metrics(px)
        {
            ascent_px = lm.ascent;
            descent_px = lm.descent;
        }
        let mut atlas = Self {
            fonts,
            lazy_sources: font_data.to_vec(),
            // 常驻后永远无需补链：懒加载触发路径（glyph() 内）与 swap_fonts
            // 成为死代码，保留待清理。
            lazy_done: true,
            px,
            ppp,
            ascent_px,
            descent_px,
            w: ATLAS_SIZE,
            h: ATLAS_SIZE,
            rgba: {
                let sz = (ATLAS_SIZE * ATLAS_SIZE * 4) as usize;
                // 安全：rgba 是 u8 数组，任意位模式都合法；
                // paint_white_texel 紧接着覆盖前 4 字节。
                vec![0u8; sz]
            },
            cx: 1, // (0,0) 保留为纯白素：实心 quad（下划线）取色用
            cy: 0,
            rh: 0,
            full: false,
            map: HashMap::with_capacity(256),
            version: 0,
            dirty: None, // 整页（None）首次上传
        };
        atlas.paint_white_texel();
        atlas
    }

    fn paint_white_texel(&mut self) {
        self.rgba[0..4].copy_from_slice(&[255, 255, 255, 255]);
        // 不标脏：dirty 初始为 None = 首次整页上传（含这枚白素）。
    }

    /// 记脏矩形（像素坐标取整，闭区间含端点 → 半开区间给上传用）。
    fn mark_dirty(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let r = Rect::from_min_size(
            Pos2::new(x as f32, y as f32),
            Vec2::new(w as f32, h as f32),
        );
        self.dirty = Some(match self.dirty {
            Some(cur) => cur.union(r),
            None => r,
        });
    }

    /// 取出脏矩形并清零（整页脏时返回 None = 整页上传）。
    pub fn take_dirty(&mut self) -> Option<Rect> {
        self.dirty.take()
    }

    /// 图集是否为空（尚未光栅化任何字形）。首帧懒预热据此判断。
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 是否已满（放不下任何新字形）。满页由调用方开新页接管。
    #[inline]
    pub fn is_full(&self) -> bool {
        self.full
    }

    /// 查询已缓存槽位（不触发光栅化）。调用方跨页查命中用。
    #[inline]
    pub fn peek(&self, ch: char) -> Option<GlyphSlot> {
        self.map.get(&ch).copied()
    }

    /// 替换字体链（懒加载补全）。字体没变多则忽略；负缓存无效化：
    /// 懒加载前缺失的字形可能已命中新字体，清空重试。
    fn swap_fonts(&mut self, fonts: Vec<Arc<fontdue::Font>>) {
        if fonts.len() <= self.fonts.len() {
            return;
        }
        self.fonts = fonts;
        self.map.retain(|_, s| s.w > 0.0);
    }

    /// 基线相对格子顶部的偏移（逻辑点）：内容行高在 cell_h 内垂直居中，
    /// 与 egui 强制 line_height 的排版结果一致。
    #[inline]
    pub fn baseline_rel(&self, cell_h: f32) -> f32 {
        let content_h = (self.ascent_px - self.descent_px) / self.ppp;
        (cell_h - content_h) / 2.0 + self.ascent_px / self.ppp
    }

    /// 下划线相对格子顶部的偏移（逻辑点）：基线下方 1px。
    #[inline]
    pub fn underline_rel(&self, cell_h: f32) -> f32 {
        self.baseline_rel(cell_h) + 1.0
    }

    /// 实心 quad 的 UV（纯白素中心），配合顶点色画下划线等待意矩形。
    #[inline]
    pub fn solid_uv() -> Pos2 {
        Pos2::new(0.5 / ATLAS_SIZE as f32, 0.5 / ATLAS_SIZE as f32)
    }

    /// shelf 打包：返回位图区域左上角。放不下（含换行后）标记页满返回 None。
    fn alloc(&mut self, gw: u32, gh: u32) -> Option<(u32, u32)> {
        if gw > self.w || gh > self.h {
            self.full = true;
            return None;
        }
        if self.cx + gw > self.w {
            self.cy += self.rh;
            self.cx = 1;
            self.rh = 0;
        }
        if self.cy + gh > self.h {
            self.full = true;
            return None;
        }
        let pos = (self.cx, self.cy);
        self.cx += gw;
        self.rh = self.rh.max(gh);
        Some(pos)
    }

    /// 查询字形，未缓存则光栅化入库。永不失败：缺字形/图集满记为空槽
    /// （负缓存，避免每帧重复尝试），调用方按 slot.w == 0 回落 galley。
    pub fn glyph(&mut self, ch: char) -> GlyphSlot {
        if let Some(&s) = self.map.get(&ch) {
            return s;
        }
        // 先在不可变借用内完成字体链查找与光栅化，再拿可变借用入库。
        let mut hit = None;
        for font in &self.fonts {
            if !font.has_glyph(ch) {
                continue;
            }
            hit = Some(font.rasterize(ch, self.px));
            break;
        }
        // 懒加载：链首等宽字体缺该字形且全链未补 → 一次性补全字体链重试。
        if hit.is_none() && !self.lazy_done && self.lazy_sources.len() > 1 {

            self.lazy_done = true;
            let px = self.px;
            let full = cached_fonts(&self.lazy_sources, px, true);
            self.swap_fonts(full);
            return self.glyph(ch);
        }
        let slot = match hit {
            // 有字形无笔画（零宽/空白）：空槽即可
            Some((m, _)) if m.width == 0 || m.height == 0 => GlyphSlot::EMPTY,
            Some((m, bmp)) => {
                let gw = m.width as u32 + GLYPH_PAD * 2;
                let gh = m.height as u32 + GLYPH_PAD * 2;
                let Some((ax, ay)) = self.alloc(gw, gh) else {
                    // 页满（超大字形或空间耗尽）：回落 galley，不入缓存，
                    // 调用方据此开新页。ponytail: 上限=单字形 ATLAS_SIZE 像素，
                    // 更大字形永远走 galley；需要时改分块图集。
                    return GlyphSlot::EMPTY;
                };
                let stride = self.w as usize * 4;
                for row in 0..m.height {
                    let src = &bmp[row * m.width..(row + 1) * m.width];
                    let dst = (ay as usize + GLYPH_PAD as usize + row) * stride
                        + (ax as usize + GLYPH_PAD as usize) * 4;
                    for (i, &c) in src.iter().enumerate() {
                        let o = dst + i * 4;
                        self.rgba[o] = c;
                        self.rgba[o + 1] = c;
                        self.rgba[o + 2] = c;
                        self.rgba[o + 3] = c;
                    }
                }
                self.mark_dirty(ax, ay, gw, gh);
                let wf = self.w as f32;
                let hf = self.h as f32;
                let x0 = ax as f32 + GLYPH_PAD as f32;
                let y0 = ay as f32 + GLYPH_PAD as f32;
                GlyphSlot {
                    u0: x0 / wf,
                    v0: y0 / hf,
                    u1: (x0 + m.width as f32) / wf,
                    v1: (y0 + m.height as f32) / hf,
                    dx: m.xmin as f32 / self.ppp,
                    dy: -(m.ymin + m.height as i32) as f32 / self.ppp,
                    w: m.width as f32 / self.ppp,
                    h: m.height as f32 / self.ppp,
                }
            }
            None => return GlyphSlot::EMPTY, // 所有字体都没有该字形（不入负缓存，
                                             // 避免运行时加载字体后永远缺字）
        };
        self.map.insert(ch, slot);
        self.version += 1;
        slot
    }

    /// 当前图集内容的 egui 纹理镜像。
    pub fn image(&self) -> ColorImage {
        ColorImage::from_rgba_premultiplied([self.w as usize, self.h as usize], &self.rgba)
    }

    /// 脏矩形子图的 egui 纹理镜像（半开区间像素矩形）。
    fn sub_image(&self, r: Rect) -> ColorImage {
        let x0 = r.min.x as u32;
        let y0 = r.min.y as u32;
        let w = ((r.max.x as u32) - x0).max(1);
        let h = ((r.max.y as u32) - y0).max(1);
        let stride = self.w as usize * 4;
        let mut pixels = Vec::with_capacity((w * h) as usize * 4);
        for y in y0..y0 + h {
            let start = y as usize * stride + x0 as usize * 4;
            pixels.extend_from_slice(&self.rgba[start..start + w as usize * 4]);
        }
        ColorImage::from_rgba_premultiplied([w as usize, h as usize], &pixels)
    }
}

/// 一个待提交的字形/实心 quad（尺寸约 36 字节，按值复制负载下线为取布局变量）。
#[derive(Clone, Copy)]
pub struct CellQuad {
    pub rect: Rect,
    pub uv0: Pos2,
    pub uv1: Pos2,
    pub color: Color32,
}

/// 把一行的 quad 接在 mesh 顶点尾部（索引由调用方统一生成）。
fn push_quad_vertices(mesh: &mut Mesh, quads: &[CellQuad]) {
    mesh.vertices.reserve(quads.len() * 4);
    for q in quads {
        let r = q.rect;
        let v = |pos: Pos2, uv: Pos2| egui::epaint::Vertex { pos, uv, color: q.color };
        mesh.vertices.extend([
            v(r.left_top(), q.uv0),
            v(r.right_top(), Pos2::new(q.uv1.x, q.uv0.y)),
            v(r.right_bottom(), q.uv1),
            v(r.left_bottom(), Pos2::new(q.uv0.x, q.uv1.y)),
        ]);
    }
}

/// 写入第 pg 页的逐行顶点区间（记 quad 序号，不是顶点号）。
fn store_ranges(row_vertex: &mut Vec<Vec<(u32, u32)>>, pg: usize, ranges: &[(u32, u32)]) {
    if row_vertex.len() <= pg {
        row_vertex.resize_with(pg + 1, Vec::new);
    }
    row_vertex[pg].clear();
    row_vertex[pg].extend_from_slice(ranges);
}

/// FNV-1a 搅拌一步。
#[inline]
pub fn hash_mix(h: &mut u64, v: u64) {
    *h ^= v;
    *h = h.wrapping_mul(0x100_0000_01b3);
}

/// 行指纹用的原始颜色 key：不解析、不做主题适配，直接把三种色型拼成一个
/// u64（高位区分色型，避免不同色型的数值相撞）。与 hash_mix 同处，供
/// session.rs 构建快照时顺手算行指纹（渲染端不再重扫全屏格）。
#[inline]
pub fn raw_color_key(c: Color) -> u64 {
    match c {
        Color::Spec(Rgb { r, g, b }) => {
            0x4000_0000_0000_0000 | ((r as u64) << 16) | ((g as u64) << 8) | b as u64
        }
        Color::Indexed(i) => 0x8000_0000_0000_0000 | i as u64,
        Color::Named(n) => 0xc000_0000_0000_0000 | n as u64,
    }
}

// ── 进程级共享图集（多页签复用）──
// 字形位图只取决于「字体链 + 物理字号」，与哪个页签在显示什么毫无关系：
// 每页签各建一份纯属重复——单页 1024² RGBA = 4MB 内存 + 4MB 显存，5 个页签
// 白吃 ~40MB，且每个新页签都要把 ASCII 重新光栅化一遍。改为按
// (字体链, 物理字号) 建一份全进程共享的图集页 + egui 纹理，所有页签复用：
// - 位图页（4MB/页）只存一份：内存 O(页数) 而非 O(页签数 × 页数)，
//   显存同理（纹理唯一，不再一页签一张同内容纹理）。
// - 重传走 `TextureHandle::set` 原地更新，TextureId 保持不变：其他页签
//   缓存的 mesh 无需重建，也不会出现「纹理被换掉、旧 id 悬垂」的问题。
// - 新页签直接命中已光栅化的字形（ASCII 预热也在共享图集里），零重复光栅化。
// 不能共享的只有每帧缓冲（见 TabBuffers）：静止帧 diff 哈希、quad 收集桶与
// 已提交 mesh 都与本页签屏幕内容一一对应。
// 生命周期：注册表只存 Weak，最后一个页签关掉后条目在下一次 acquire 时回收，
// 整组图集页与纹理随之释放（回到单页签前的占用，不留常驻大块）。
// 并发：渲染全在 UI 线程，逐格 `glyph()` 不能再加锁（每帧上万格），
// 故取帧时加一次锁并把访问权（TermFrame）借给整帧，逐格路径零锁开销。

/// 一页图集 + 该页全进程唯一的 egui 纹理。
struct AtlasPage {
    atlas: GlyphAtlas,
    /// 已上传纹理的位图版本（与 `atlas.version` 不等才需重传）。
    uploaded: u64,
    /// 该页纹理：所有页签共用同一个 TextureId（重传原地更新，id 不变）。
    tex: Option<TextureHandle>,
}

impl AtlasPage {
    fn new(atlas: GlyphAtlas) -> Self {
        Self { atlas, uploaded: 0, tex: None }
    }
}

/// 进程级共享图集：同（字体链, 物理字号）下所有页签共用同一组图集页。
struct SharedAtlas {
    /// 开新页用的字体源（Arc 共享，零拷贝；字体链全进程恒定，见 FONT_DATA_CACHE）。
    sources: Vec<FontSource>,
    /// 开新页时复用的物理字号参数。
    font_size_pt: f32,
    ppp: f32,
    pages: Vec<AtlasPage>,
}

impl SharedAtlas {
    fn new(sources: &[FontSource], font_size_pt: f32, ppp: f32) -> Self {
        Self {
            sources: sources.to_vec(),
            font_size_pt,
            ppp,
            pages: vec![AtlasPage::new(GlyphAtlas::new(sources, font_size_pt, ppp))],
        }
    }

    /// 是否所有页都还没光栅化过字形。首帧预热据此判断。
    fn is_empty(&self) -> bool {
        self.pages.iter().all(|p| p.atlas.is_empty())
    }

    /// 查询/光栅化字形，返回 (页号, 槽位)。缺字形且末页满 → 开新页重试
    /// （页码单调递增，旧页字形常驻，永不整体重灌）。其他页签已光栅化的
    /// 字形直接命中共享表，零重复光栅化。
    fn glyph(&mut self, ch: char) -> (usize, GlyphSlot) {
        // 跨页查缓存：从最新页往前找（新字形普遍落在最新页）。
        for i in (0..self.pages.len()).rev() {
            if let Some(s) = self.pages[i].atlas.peek(ch) {
                return (i, s);
            }
        }
        let last = self.pages.len() - 1;
        let slot = self.pages[last].atlas.glyph(ch);
        if slot.w > 0.0 || !self.pages[last].atlas.is_full() {
            return (last, slot);
        }
        // 末页满且该字形无可见笔画：开新页（新页懒加载未触发时缺字形自动补链）。
        let mut atlas = GlyphAtlas::new(&self.sources, self.font_size_pt, self.ppp);
        let slot = atlas.glyph(ch);
        let idx = self.pages.len();
        self.pages.push(AtlasPage::new(atlas));
        (idx, slot)
    }

    /// 基线相对格子顶部的偏移（逻辑点）。
    #[inline]
    fn baseline_rel(&self, pg: usize, cell_h: f32) -> f32 {
        self.pages[pg].atlas.baseline_rel(cell_h)
    }

    /// 下划线相对格子顶部的偏移（逻辑点）。
    #[inline]
    fn underline_rel(&self, pg: usize, cell_h: f32) -> f32 {
        self.pages[pg].atlas.underline_rel(cell_h)
    }

    /// 取该页纹理 id，位图有新字形时**原地**重传。
    /// 用 `TextureHandle::set` / `set_partial` 而非 `ctx.load_texture`：后者换新
    /// id 并释放旧纹理，其他页签缓存的 mesh 会指向已释放的纹理 id（渲染成空白/
    /// 回退纹理）。已有纹理的页只传脏矩形（`set_partial`），否则每出现一个新
    /// 字形就要重传整页 4MiB —— 流式中文输出时这是 UI 线程最大的单点开销。
    /// 新页（tex 为 None）必须整传：首次 sub-image 上传不分配纹理存储。
    fn page_texture(&mut self, pg: usize, ctx: &egui::Context) -> egui::TextureId {
        let page = &mut self.pages[pg];
        if let Some(t) = &page.tex
            && page.uploaded == page.atlas.version
        {
            return t.id();
        }
        // 无论整传还是脏区传，脏标记都在此消费（整传后无需再传）。
        let dirty = page.atlas.take_dirty();
        if page.tex.is_some()
            && let Some(r) = dirty
        {
            let x0 = r.min.x as usize;
            let y0 = r.min.y as usize;
            page.tex
                .as_mut()
                .unwrap()
                .set_partial([x0, y0], page.atlas.sub_image(r), TextureOptions::LINEAR);
            count_upload(r.width() as usize * r.height() as usize);
        } else {
            let img = page.atlas.image();
            count_upload(page.atlas.w as usize * page.atlas.h as usize);
            match &mut page.tex {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => {
                    page.tex = Some(ctx.load_texture(
                        format!("term_glyph_atlas_{pg}"),
                        img,
                        TextureOptions::LINEAR,
                    ));
                }
            }
        }
        page.uploaded = page.atlas.version;
        page.tex.as_ref().unwrap().id()
    }
}

/// 共享图集的键：物理字号（与 FONT_CACHE 同口径取整）+ 缩放因子比特。
/// 字体链不进键（字节全等比较要 memcmp 20MB），改在命中后校验：同参不同链
/// 极少见，宁可错开两份图集也不能错配字形。
#[derive(PartialEq, Eq, Hash)]
struct AtlasKey {
    px: u32,
    ppp_bits: u32,
}

/// 注册表条目：字体链（命中时校验）+ 图集弱引用。
struct AtlasEntry {
    sources: Vec<FontSource>,
    atlas: Weak<Mutex<SharedAtlas>>,
}

type AtlasRegistry = HashMap<AtlasKey, AtlasEntry>;
static SHARED_ATLASES: OnceLock<Mutex<AtlasRegistry>> = OnceLock::new();

/// 字体链判等：Arc 同源直接比指针（进程级缓存里就是同一份），
/// 否则退回字节全等（~20MB memcmp，只在建图集/开新页签时发生）。
fn same_chain(a: &[FontSource], b: &[FontSource]) -> bool {
    a.len() == b.len()
        && a
            .iter()
            .zip(b)
            .all(|(x, y)| x.1 == y.1 && (Arc::ptr_eq(&x.0, &y.0) || x.0 == y.0))
}

/// 取（或建）本进程该参数的共享图集。已有页签在用则复用同一份，
/// 没有页签再持有的条目顺手回收（连带释放它的位图页与纹理）。
fn shared_atlas(
    sources: &[FontSource],
    font_size_pt: f32,
    ppp: f32,
) -> Option<Arc<Mutex<SharedAtlas>>> {
    if sources.is_empty() {
        return None;
    }
    let key = AtlasKey { px: (font_size_pt * ppp) as u32, ppp_bits: ppp.to_bits() };
    let reg = SHARED_ATLASES.get_or_init(|| Mutex::new(HashMap::new()));
    // 中毒（页签渲染 panic 后 unwind 穿过加锁）也要能继续服务：取回内层即可。
    let mut reg = reg.lock().unwrap_or_else(|e| e.into_inner());
    // 回收已无人持有的条目：Weak 失效 → 该键的位图页/纹理可以整体释放。
    reg.retain(|_, e| e.atlas.strong_count() > 0);
    if let Some(entry) = reg.get(&key)
        && same_chain(&entry.sources, sources)
        && let Some(hit) = entry.atlas.upgrade()
    {
        return Some(hit);
    }
    let atlas = Arc::new(Mutex::new(SharedAtlas::new(sources, font_size_pt, ppp)));
    reg.insert(key, AtlasEntry { sources: sources.to_vec(), atlas: Arc::downgrade(&atlas) });
    Some(atlas)
}

/// 共享图集句柄（Arc clone，零拷贝）。帧期间借它取访问权：
/// 调用方须让句柄活到 TermFrame 之后（声明在帧之前即可自动满足）。
#[derive(Clone)]
pub struct AtlasHandle(Arc<Mutex<SharedAtlas>>);

/// 页签私有的帧缓冲：静止帧 diff 哈希、quad 收集桶、已提交网格。
/// 与共享图集无任何引用关系，故帧借用它不构成别名（见 TermFrame）。
#[derive(Default)]
struct TabBuffers {
    /// 本帧每行的内容指纹（UI 每行写一次；长度 = rows）。
    row_hash: Vec<u64>,
    /// 上一帧每行的内容指纹。
    prev_row_hash: Vec<u64>,
    /// 本帧每行是否变化（set_row_hash 时算好）。
    row_dirty: Vec<bool>,
    /// 本帧 quad 收集缓冲（跨帧复用分配）：行 → 图集页 → quad。
    row_quads: Vec<Vec<Vec<CellQuad>>>,
    /// 每个图集页里每行对应的顶点区间（起始顶点索引, quad 数），供增量重建。
    row_vertex: Vec<Vec<(u32, u32)>>,
    /// 已提交网格（按图集页索引；None = 该页本帧无 quad）。
    meshes: Vec<Option<Arc<Mesh>>>,
    /// 上一帧的帧级指纹（几何/主题/选区/图集世代，见 set_frame_sig）。
    frame_sig: u64,
}

/// 每页签的 GPU 批渲染句柄。`None` = 尚未初始化或初始化失败（整格走 galley 回落）。
/// 字形位图与纹理在页签间共享（见 SHARED_ATLASES），本结构只留页签私有帧缓冲。
pub struct TermGpu {
    /// 进程级共享图集（同字体链 + 物理字号的所有页签共用一份）。
    atlas: AtlasHandle,
    /// 字体参数世代：每次重建图集 +1。图集换代后旧的逐行顶点/指纹基线作废，
    /// 渲染侧把它排进帧参数指纹。
    epoch: u64,
    /// 换图集所需的原始字体字节（Arc 共享，DPI 变化时用）。
    sources: Vec<FontSource>,
    font_size_pt: f32,
    params_ppp: f32,
    buf: TabBuffers,
}

impl TermGpu {
    /// 从 egui 已注册的 Monospace 家族提取字体数据，挂到该参数的共享图集上。
    /// 字体链为空返回 None（整格 galley 回落）。
    pub fn new(ctx: &egui::Context, font_size_pt: f32, ppp: f32) -> Option<Self> {
        let sources = cached_font_data(ctx); // 进程级零拷贝，见 FONT_DATA_CACHE
let atlas = AtlasHandle(shared_atlas(&sources, font_size_pt, ppp)?);
        Some(Self {
            atlas,
            epoch: 1,
            sources,
            font_size_pt,
            params_ppp: ppp,
            buf: TabBuffers::default(),
        })
    }

    /// 帧入口用的共享图集句柄。
    pub fn atlas_handle(&self) -> AtlasHandle {
        self.atlas.clone()
    }

    /// DPI/字号变化时改挂对应参数的共享图集（多数页签此时也挂在同一份上，
    /// 天然互不重复）；其余情况原地复用。换图集后旧哈希基线与已提交网格
    /// 全部作废（字形位图与纹理 id 都可能已换）。
    pub fn ensure_params(&mut self, font_size_pt: f32, ppp: f32) {
        if self.params_ppp == ppp && self.font_size_pt == font_size_pt {
            return;
        }
self.atlas =
            AtlasHandle(shared_atlas(&self.sources, font_size_pt, ppp).expect("字体源非空"));
        self.font_size_pt = font_size_pt;
        self.params_ppp = ppp;
        // 字形位图与纹理 id 全换：旧的逐行指纹基线作废（否则所有行都可能被
        // 判为「没变」而复用到旧顶点）。
self.buf.prev_row_hash.clear();
        self.buf.meshes.clear();
        self.buf.row_vertex.clear();
        self.epoch += 1;
    }

    /// 字体图集世代号（换字体/DPI 时自增）。
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 挂帧级指纹：与上帧不同则作废逐行指纹基线（所有行当帧重算）。
    pub fn set_frame_sig(&mut self, sig: u64) {
        if self.buf.frame_sig != sig {
            self.buf.frame_sig = sig;
            self.buf.prev_row_hash.clear();
        }
    }

    /// 帧入口：逐行缓冲对齐 rows、清空各行 quad 桶，并取一帧的图集访问权。
    /// `handle` 须是本帧之前由 `atlas_handle()` 取出的同一份句柄（见其注释）。
    ///
    /// 先用 `set_frame_sig()` 挂上帧级指纹：行指纹只含格子**内容**，几何/主题/
    /// 选区/图集世代的变化只能由它作废旧指纹基线——否则「内容没变但格子位置
    /// 变了」（挪窗口、改选区）的行会命中干净行而复用旧顶点。
    pub fn begin_frame<'a>(
        &'a mut self,
        handle: &'a AtlasHandle,
        rows: usize,
    ) -> TermFrame<'a> {
        self.buf.row_hash.clear();
        self.buf.row_hash.resize(rows, 0);
        self.buf.row_dirty.clear();
        self.buf.row_dirty.resize(rows, true);
        self.buf.row_quads.clear();
        self.buf.row_quads.resize_with(rows, Vec::new);
        for row in &mut self.buf.row_quads {
            for q in row.iter_mut() {
                q.clear();
            }
        }
        // 每帧一次锁（单 UI 线程、零竞争），之后逐格零锁开销。
        let atlas = handle.0.lock().unwrap_or_else(|e| e.into_inner());
        TermFrame { atlas, buf: &mut self.buf }
    }
}

/// 一帧的图集访问权：共享图集（帧级独占 `&mut`）+ 本页签帧缓冲借用。
/// 帧内所有字形/纹理访问都经它，逐格路径无锁；`end_frame` 后随作用域释放。
pub struct TermFrame<'a> {
    /// 共享图集的唯一访问路径（帧内独占可变借用）。
    atlas: MutexGuard<'a, SharedAtlas>,
    /// 本页签帧缓冲：与共享图集无引用关系，&mut 它够不到图集状态。
    buf: &'a mut TabBuffers,
}

impl TermFrame<'_> {
    /// 图集是否完全为空（尚未光栅化任何字形）。首帧预热据此判断。
    pub fn is_empty(&self) -> bool {
        self.atlas.is_empty()
    }

/// 记录一行的内容指纹并当场判它是否变化（行级 dirty 的唯一判据）。
    /// UI 只渲染 dirty 行；`end_frame` 也只重建 dirty 行对应的顶点。
    #[inline]
    pub fn set_row_hash(&mut self, row: usize, h: u64) {
        self.buf.row_hash[row] = h;
        self.buf.row_dirty[row] = self.buf.prev_row_hash.get(row) != Some(&h);
    }

    /// 查询/光栅化字形，返回 (页号, 槽位)。空槽（w==0）由调用方回落 galley。
    #[inline]
    pub fn glyph(&mut self, ch: char) -> (usize, GlyphSlot) {
        self.atlas.glyph(ch)
    }

    /// 基线相对格子顶部的偏移（逻辑点）。
    #[inline]
    pub fn baseline_rel(&self, pg: usize, cell_h: f32) -> f32 {
        self.atlas.baseline_rel(pg, cell_h)
    }

    /// 下划线相对格子顶部的偏移（逻辑点）。
    #[inline]
    pub fn underline_rel(&self, pg: usize, cell_h: f32) -> f32 {
        self.atlas.underline_rel(pg, cell_h)
    }

/// 往指定行的指定图集页收一个 quad（跨帧复用分配）。
    ///
    /// 桶随页号自增：帧缓冲是页签私有的，与共享图集的页数没有同步机制，
    /// 而新页由 `glyph()` 在末页满时按需开出（页号可越过桶的现有长度）。
    /// 缺失即 `get_mut` 落空会把整格字形静默丢掉（表现为「终端整片白」）。
    #[inline]
    pub fn push_quad(&mut self, row: usize, pg: usize, q: CellQuad) {
        let buckets = &mut self.buf.row_quads[row];
        if pg >= buckets.len() {
            buckets.resize_with(pg + 1, Vec::new);
        }
        buckets[pg].push(q);
    }

    /// 帧尾：逐行增重建网格。返回本帧应提交的网格（按图集页索引，None =
    /// 该页无 quad）；全行未变时返回上一帧的同一批 Arc（调用方 clone 后照常
    /// add——egui 每帧都要画，省的是 CPU 侧 quad→mesh 组装）。
    /// 纹理缺失或位图有新字形时原地重传（共享纹理 id 不变，其他页签缓存的
    /// mesh 继续有效）。
    pub fn end_frame(&mut self, ctx: &egui::Context) -> &[Option<Arc<Mesh>>] {
        let rows = self.buf.row_hash.len();
        // 全部行未变：零组装，直接重放上一帧的同一批 Arc<Mesh>。
        if !self.buf.row_dirty.iter().any(|&d| d) {
            return &self.buf.meshes;
        }
        let old_meshes = std::mem::take(&mut self.buf.meshes);
        let old_vertex = std::mem::take(&mut self.buf.row_vertex);
        self.buf.prev_row_hash.clear();
        self.buf.prev_row_hash.extend_from_slice(&self.buf.row_hash);
        self.buf.meshes.resize_with(self.atlas.pages.len().max(self.buf.meshes.len()), || None);
        for pg in 0..self.atlas.pages.len() {
            let page_quads: Vec<&[CellQuad]> = (0..rows)
                .map(|r| {
                    self.buf.row_quads[r].get(pg).map(|q| q.as_slice()).unwrap_or(&[])
                })
                .collect();
            let old_mesh = old_meshes.get(pg).and_then(|m| m.as_ref());
            let old_ranges = old_vertex.get(pg);
            // dirty 行里只要有一行在本页产生 quad，才需要重建本页网格；否则
            // （变化全在本页之外，如纯背景色变化）旧网格整体有效，整页照搬。
            let touches = self
                .buf
                .row_dirty
                .iter()
                .enumerate()
                .any(|(r, &d)| d && !page_quads[r].is_empty());
            if old_mesh.is_some() && !touches {
                self.buf.meshes[pg] = old_mesh.cloned();
                if let Some(rs) = old_ranges {
                    store_ranges(&mut self.buf.row_vertex, pg, rs);
                }
                continue;
            }
            if !touches && old_mesh.is_none() {
                // 本页既无脏行 quad 也无旧网格（可能是别的页签刷出的新页）：
                // 不要为本页签用不到的纹理白分 4MiB 显存。
                self.buf.meshes[pg] = None;
                continue;
            }
            // 首帧（无旧网格）与增量重建走同一条路径：老网格缺席或该行区间
            // 缺失时该行只能重新生成——未变行没渲染 quad，此时无 quad 可生成，
            // 所以每帧都必须记下区间（row_vertex），否则整行字形会消失。
            let tid = self.atlas.page_texture(pg, ctx);
            let mut mesh = Mesh::with_texture(tid);
            let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(rows);
            let mut quad_total = 0usize;
            for (r, dirty) in self.buf.row_dirty.iter().copied().enumerate() {
                let reused = match (dirty, old_mesh, old_ranges.and_then(|rs| rs.get(r))) {
                    (false, Some(old), Some(&(s, n))) => Some((old, s, n)),
                    _ => None,
                };
                let (start, n) = match reused {
                    // 未变的行：整段顶点从旧网格 memcpy 过来（不重算字形位置）。
                    Some((old, s, n)) => {
                        let from = s as usize * 4;
                        // 区间记的是 quad 序号，顶点偏移要 ×4。
                        let start = mesh.vertices.len() as u32 / 4;
                        mesh.vertices
                            .extend_from_slice(&old.vertices[from..from + n as usize * 4]);
                        (start, n)
                    }
                    None => {
                        let start = mesh.vertices.len() as u32 / 4;
                        push_quad_vertices(&mut mesh, page_quads[r]);
                        (start, page_quads[r].len() as u32)
                    }
                };
                quad_total += n as usize;
                ranges.push((start, n));
            }
            // 顶点数不变 → 索引整体照抄。顶点按「行序 → 行内 quad 序」连续
            // 追加，索引是纯序号模式（0,1,2,0,2,3 …），与每行各自的区间无关：
            // 只要 quad 总数没变，逐行区间移位也不会让索引指错行。
if let Some(old) = old_mesh.filter(|o| o.vertices.len() == mesh.vertices.len()) {
                mesh.indices.clone_from(&old.indices);
            } else {
                mesh.indices.reserve(quad_total * 6);
                for &(start, n) in &ranges {
                    for i in 0..n {
                        let vi = (start + i) * 4;
                        mesh.indices.extend_from_slice(&[vi, vi + 1, vi + 2, vi, vi + 2, vi + 3]);
                    }
                }
            }
            store_ranges(&mut self.buf.row_vertex, pg, &ranges);
            self.buf.meshes[pg] = Some(Arc::new(mesh));
        }
        &self.buf.meshes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_atlas(ppp: f32) -> GlyphAtlas {
        GlyphAtlas::new(&[(Arc::new(epaint_default_fonts::HACK_REGULAR.to_vec()), 0)], 14.0, ppp)
    }

/// ASCII 字形应成功光栅化入库，且二次查询走缓存返回同一槽位。
    #[test]
    fn ascii_glyph_rasterizes_and_caches() {
        let mut a = test_atlas(1.0);
        let s1 = a.glyph('A');
        assert!(s1.w > 0.0 && s1.h > 0.0, "'A' 应有可见笔画");
        assert!(s1.dy <= 0.0 && s1.dy >= -40.0, "字形顶应在基线上方合理范围内 dy={}", s1.dy);
        assert!((0.0..=1.0).contains(&s1.u0) && (0.0..=1.0).contains(&s1.v1));
        let s2 = a.glyph('A');
        assert_eq!((s1.u0, s1.v0), (s2.u0, s2.v0), "二次查询应命中缓存");
        let v_before = a.version;
        a.glyph('B');
        assert_eq!(a.version, v_before + 1, "新增字形应推进版本号");
    }

/// 缺字形（emoji）应得空槽（负缓存），调用方回落 galley 路径。
    #[test]
    fn missing_glyph_returns_empty_slot() {
        let mut a = test_atlas(1.0);
        let s = a.glyph('\u{1F600}'); // 😀 Hack 无此字形
        assert_eq!(s.w, 0.0, "缺字形应为空槽");
    }

    /// 脏矩形上传：子图内容必须与整页镜像逐字节一致（否则字形显示为
    /// 错位的其它字形），且取走后清零、新字形重新标脏。
    #[test]
    fn dirty_rect_sub_image_matches_full() {
        let mut a = test_atlas(1.0);
        a.glyph('A');
        let r = a.take_dirty().expect("光栅化后应标脏");
        assert!(r.width() < 64.0 && r.height() < 64.0, "脏区应远小于整页: {r:?}");
        let sub = a.sub_image(r);
        let full = a.image();
        let (x0, y0, fw, sw, sh) = (r.min.x as usize, r.min.y as usize, full.width(), sub.width(), sub.height());
        for y in 0..sh {
            for x in 0..sw {
                assert_eq!(
                    sub.pixels[y * sw + x],
                    full.pixels[(y + y0) * fw + x + x0],
                    "({x},{y}) 子图与整页不一致",
                );
            }
        }
        assert!(a.take_dirty().is_none(), "取走脏区后应清零");
        a.glyph('B');
        assert!(a.take_dirty().is_some(), "新字形应重新标脏");
    }

    /// 基线应在格子内部（内容行高垂直居中）。
    #[test]
    fn baseline_inside_cell() {
        let a = test_atlas(2.0); // 高 DPI
        let b = a.baseline_rel(20.0);
        assert!(b > 0.0 && b < 20.0, "baseline={b}");
        assert!(a.underline_rel(20.0) > b);
    }

    /// Mesh 顶点组装：quad 数 × 4 顶点，索引在 end_frame 里统一生成。
    #[test]
    fn quad_vertices_layout() {
        let quads = [CellQuad {
            rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(10.0, 20.0)),
            uv0: Pos2::ZERO,
            uv1: Pos2::new(1.0, 1.0),
            color: Color32::WHITE,
        }];
        let mut m = Mesh::with_texture(egui::TextureId::Managed(0));
        push_quad_vertices(&mut m, &quads);
        assert_eq!(m.vertices.len(), 4);
        // 左上、右上、右下、左下，索引生成后 6 个。
        assert_eq!(m.vertices[0].pos, Pos2::new(0.0, 0.0));
        assert_eq!(m.vertices[1].pos, Pos2::new(10.0, 0.0));
        assert_eq!(m.vertices[2].pos, Pos2::new(10.0, 20.0));
        assert_eq!(m.vertices[3].pos, Pos2::new(0.0, 20.0));
    }

    /// 哈希搅拌确定性 + 对不同输入产生不同值（防碰撞冒烟）。
    #[test]
    fn hash_mix_deterministic() {
        let mut h1 = 0xcbf2_9ce4_8422_2325u64;
        hash_mix(&mut h1, 'a' as u64);
        let mut h2 = 0xcbf2_9ce4_8422_2325u64;
        hash_mix(&mut h2, 'a' as u64);
        assert_eq!(h1, h2);
        let mut h3 = h1;
        hash_mix(&mut h3, 'b' as u64);
        assert_ne!(h1, h3);
    }

    /// 缺字形行为：全链缺字形（如 😀 Hack/链路都不含）应得空槽（负缓存），
    /// 调用方回落 galley 路径；版本号不变（常驻加载后不得再有补链动作）。
    /// 懒加载机制已由常驻加载取代（GlyphAtlas::new 即全量解析），此测试守住
    /// 缺字形的空槽契约与「不重复触发」防抖。
    #[test]
    fn missing_glyph_empty_slot_no_retry() {
        let mut a = test_atlas(1.0);
        assert!(a.lazy_done, "常驻加载：构造即完成补链语义（lazy_done=true）");
        let s = a.glyph('\u{6C49}'); // 汉：Hack 无此字形
        assert_eq!(s.w, 0.0, "Hack 缺 CJK 字形应回落空槽");
        let v = a.version;
        assert_eq!(a.glyph('\u{8BD5}').w, 0.0); // 试：仍空槽
        assert_eq!(a.version, v, "补链后不得再重复触发（lazy_done 置位）");
    }

    /// 测试用字体源（HACK 单链）。
    fn hack_sources() -> Vec<FontSource> {
        vec![(Arc::new(epaint_default_fonts::HACK_REGULAR.to_vec()), 0)]
    }

    /// 多页签共享：同（字体链, 物理字号）必须拿到同一份图集，一个页签光栅化
    /// 的字形另一个页签直接命中——位图页（4MB/页）与纹理都不再按页签复制。
    /// ppp 不同则必须分开（字形位图随物理像素变化，混用会错位/发虚）。
    #[test]
    fn shared_atlas_reused_across_tabs() {
        let src = hack_sources();
        let a = shared_atlas(&src, 14.0, 1.0).unwrap();
        let b = shared_atlas(&src, 14.0, 1.0).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "同参页签应复用同一份共享图集");
        a.lock().unwrap().glyph('A');
        assert!(
            b.lock().unwrap().glyph('A').1.w > 0.0,
            "另一个页签应命中已光栅化的字形（零重复光栅化）"
        );
        let hi = shared_atlas(&src, 14.0, 2.0).unwrap();
        assert!(!Arc::ptr_eq(&a, &hi), "ppp 不同必须另建图集");
    }

    /// 生命周期：最后一个页签关掉后共享图集整体释放（不留 4MB/页 常驻），
    /// 再开页签时重建为空图集，而非复活旧页签的字形页。
    #[test]
    fn shared_atlas_released_after_last_tab_closes() {
        let src = hack_sources();
        let a = shared_atlas(&src, 15.0, 1.0).unwrap();
        a.lock().unwrap().glyph('B');
        let b = shared_atlas(&src, 15.0, 1.0).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        drop(a);
        drop(b);
        let c = shared_atlas(&src, 15.0, 1.0).unwrap();
        assert!(c.lock().unwrap().is_empty(), "全部页签关闭后应重建空图集");
    }

    /// 纹理重传必须原地更新：位图多一个字形后 TextureId 依旧不变，
    /// 否则其他页签缓存的 mesh 会指向已释放的纹理（渲染成空白/回退纹理）。
    /// 位图无变化时则完全不产生上传 delta。
    #[test]
    fn texture_id_stable_and_skips_redundant_upload() {
        let ctx = egui::Context::default();
        let mut sh = SharedAtlas::new(&hack_sources(), 14.0, 1.0);
        sh.glyph('A');
        let id = sh.page_texture(0, &ctx);
        // 吃掉首帧上传的 delta（未交给渲染器的 delta 不得丢弃，egui 会断言；
        // 里面还有 egui 自己那张 1×1 纯白默认纹理，故只查本图集那一项）。
        let mut first = ctx.tex_manager().write().take_delta();
        assert!(first.set.contains_key(&id), "首帧应上传图集纹理");
        first.clear();
        // 无新字形 → 不重传。
        assert_eq!(sh.page_texture(0, &ctx), id);
        let mut none = ctx.tex_manager().write().take_delta();
        assert!(
            none.is_empty(),
            "无新字形不应产生纹理上传"
        );
        none.clear();
        // 新字形 → 原地重传，id 不变。
        sh.glyph('B');
        assert_eq!(sh.page_texture(0, &ctx), id, "重传后 TextureId 必须保持不变");
        let mut delta = ctx.tex_manager().write().take_delta();
        assert_eq!(delta.set.len(), 1, "新字形应重传该页一次");
        assert!(delta.free.is_empty(), "共享纹理不得被释放（其他页签还在引用）");
        delta.clear();
    }

    /// 页签级契约：两个页签共用一份共享图集（位图/纹理/光栅化都不按页签翻倍），
    /// 但帧缓冲各自私有（静止帧哈希、quad 桶与已提交 mesh 都与本页签内容绑定，
    /// 共享出去会串页）。
    #[test]
    fn term_gpus_share_atlas_but_not_buffers() {
        let src = hack_sources();
        let mk = || TermGpu {
            atlas: AtlasHandle(shared_atlas(&src, 16.0, 1.0).unwrap()),
            epoch: 1,
            sources: src.clone(),
            font_size_pt: 16.0,
            params_ppp: 1.0,
            buf: TabBuffers::default(),
        };
        let (mut a, mut b) = (mk(), mk());
        assert!(Arc::ptr_eq(&a.atlas.0, &b.atlas.0), "两页签应共用同一份共享图集");
        let (ha, hb) = (a.atlas_handle(), b.atlas_handle());
        {
            let mut fa = a.begin_frame(&ha, 2);
            assert!(fa.glyph('W').1.w > 0.0, "'W' 应光栅化成功");
            fa.set_row_hash(1, 0xabcd);
        }
        // 帧缓冲私有：行指纹只写进了 A 的缓冲（B 未开帧，仍为空）。
        assert_eq!(a.buf.row_hash[1], 0xabcd);
        assert!(b.buf.row_hash.is_empty() && b.buf.prev_row_hash.is_empty());
        let mut fb = b.begin_frame(&hb, 1);
        assert!(!fb.is_empty(), "共享图集已含 'W'，B 页签无需再预热/重光栅化");
        assert!(fb.glyph('W').1.w > 0.0, "B 页签应命中 A 已光栅化的字形");
    }

    /// 回归锁：默认（空）帧缓冲下的首帧必须真的产出 mesh。
    /// 帧缓冲的 quad 桶与共享图集的页数不同步，而页号可能越过桶的现有长度
    /// （首帧桶为空、图集开新页）。桶缺失时若静默丢 quad，终端会整片白屏
    /// （只余直接画的网格/光标），但「字形槽位非空」等断言全部照过——
    /// 这里从 push_quad 一直走到 end_frame，锁住真出网格。
    #[test]
    fn empty_frame_buffers_still_build_mesh() {
        let ctx = egui::Context::default();
        let src = hack_sources();
        let mut g = TermGpu {
            atlas: AtlasHandle(shared_atlas(&src, 16.0, 1.0).unwrap()),
            epoch: 1,
            sources: src.clone(),
            font_size_pt: 16.0,
            params_ppp: 1.0,
            buf: TabBuffers::default(),
        };
        let h = g.atlas_handle();
        let mut f = g.begin_frame(&h, 1);
        let (pg, slot) = f.glyph('W');
        assert!(slot.w > 0.0, "槽位应有可见笔画");
        f.set_row_hash(0, 1);
        f.push_quad(
            0,
            pg,
            CellQuad {
                rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(slot.w, slot.h)),
                uv0: Pos2::new(slot.u0, slot.v0),
                uv1: Pos2::new(slot.u1, slot.v1),
                color: Color32::WHITE,
            },
        );
        let meshes = f.end_frame(&ctx).to_vec();
let mesh = meshes[0].as_ref().expect("首帧（桶为空）也必须提交一个字形网格");
        assert_eq!(mesh.vertices.len(), 4, "网格应含一个 quad 的 4 个顶点");
        ctx.tex_manager().write().take_delta().clear();
    }

    /// 行级增量网格必须与「全行重建」逐字段一致。
    /// 未变行的顶点是整段 memcpy 过来的，若区间/行序错位，画面会整行串位
    /// 且这种错误只在部分行变化时出现——满屏重建永远看不出来。
    #[test]
    fn row_incremental_mesh_matches_full_rebuild() {
        let ctx = egui::Context::default();
        let src = hack_sources();
        let mk = || TermGpu {
            atlas: AtlasHandle(shared_atlas(&src, 16.0, 1.0).unwrap()),
            epoch: 1,
            sources: src.clone(),
            font_size_pt: 16.0,
            params_ppp: 1.0,
            buf: TabBuffers::default(),
        };
        let mut g = mk();
        let h = g.atlas_handle();
        let emit = |f: &mut TermFrame, row: usize, hash: u64, ch: char, x: f32| {
            f.set_row_hash(row, hash);
            let (pg, slot) = f.glyph(ch);
            assert!(slot.w > 0.0, "字形应已入库");
            f.push_quad(
                row,
                pg,
                CellQuad {
                    rect: Rect::from_min_size(Pos2::new(x, 0.0), egui::vec2(slot.w, slot.h)),
                    uv0: Pos2::new(slot.u0, slot.v0),
                    uv1: Pos2::new(slot.u1, slot.v1),
                    color: Color32::WHITE,
                },
            );
        };
        let mesh_of = |f: &mut TermFrame| -> Arc<Mesh> {
            f.end_frame(&ctx)[0].as_ref().expect("应有网格").clone()
        };

        // 帧1：两行各一个字。
        let mut f = g.begin_frame(&h, 2);
        emit(&mut f, 0, 10, 'A', 0.0);
        emit(&mut f, 1, 11, 'B', 0.0);
        let m1 = mesh_of(&mut f);
        drop(f);

        // 帧2：只改第 1 行，第 0 行零 quad（命中整段复用）。
        let mut f = g.begin_frame(&h, 2);
        f.set_row_hash(0, 10);
        emit(&mut f, 1, 12, 'C', 0.0);
        let m2 = mesh_of(&mut f);
        drop(f);
        assert!(!Arc::ptr_eq(&m1, &m2), "有脏行时应重建网格");
        assert_eq!(m2.vertices.len(), 8);
        assert_eq!(&m2.vertices[..4], &m1.vertices[..4], "未变行的顶点应原样复用");

        // 帧3：只改第 0 行。
        let mut f = g.begin_frame(&h, 2);
        emit(&mut f, 0, 13, 'D', 0.0);
        f.set_row_hash(1, 12);
        let m3 = mesh_of(&mut f);
        drop(f);
        assert_eq!(&m3.vertices[4..], &m2.vertices[4..], "第 1 行未变，顶点应原样复用");

        // 帧4：同样内容但两行全标脏（走全量重建）→ 顶点/索引必须与帧3一致。
        let mut f = g.begin_frame(&h, 2);
        emit(&mut f, 0, 14, 'D', 0.0);
        emit(&mut f, 1, 15, 'C', 0.0);
        let m4 = mesh_of(&mut f);
        drop(f);
        assert_eq!(m3.vertices.len(), m4.vertices.len());
        assert!(m3.vertices == m4.vertices, "增量重建的顶点应与全量重建逐字段一致");
        assert!(m3.indices == m4.indices, "索引应一致");

        // 全静止帧：不重建，直接重放同一批 Arc。
        let mut f = g.begin_frame(&h, 2);
        f.set_row_hash(0, 14);
        f.set_row_hash(1, 15);
        let m5 = mesh_of(&mut f);
        drop(f);
assert!(Arc::ptr_eq(&m4, &m5), "静止帧应零组装");

// 中间行清空、末行补 quad：总 quad 数不变、逐行区间整体移位，增量结果
        // 必须与全量重建逐字段一致（含照抄的索引）。
        let mut g3 = mk();
        let h3 = g3.atlas_handle();
        let emit3 = |f: &mut TermFrame, row: usize, hash: u64, ch: char, x: f32| {
            f.set_row_hash(row, hash);
            let (pg, slot) = f.glyph(ch);
            f.push_quad(
                row,
                pg,
                CellQuad {
                    rect: Rect::from_min_size(Pos2::new(x, 0.0), egui::vec2(slot.w, slot.h)),
                    uv0: Pos2::new(slot.u0, slot.v0),
                    uv1: Pos2::new(slot.u1, slot.v1),
                    color: Color32::WHITE,
                },
            );
        };
        let mut f = g3.begin_frame(&h3, 3);
        emit3(&mut f, 0, 30, 'A', 0.0);
        emit3(&mut f, 1, 31, 'B', 0.0);
        emit3(&mut f, 2, 32, 'C', 0.0);
        let _base = mesh_of(&mut f); // 必须收帧，prev_row_hash/旧网格才有基准
        drop(f);
        let mut f = g3.begin_frame(&h3, 3);
        f.set_row_hash(0, 30); // 首行未变（整段复用）
        f.set_row_hash(1, 33); // 中间行清空
        emit3(&mut f, 2, 34, 'D', 0.0);
        emit3(&mut f, 2, 34, 'E', 10.0);
        let m6 = mesh_of(&mut f);
        drop(f);
        let mut g4 = mk(); // 全量重建基准
        let h4 = g4.atlas_handle();
        let emit4 = |f: &mut TermFrame, row: usize, hash: u64, ch: char, x: f32| {
            f.set_row_hash(row, hash);
            let (pg, slot) = f.glyph(ch);
            f.push_quad(
                row,
                pg,
                CellQuad {
                    rect: Rect::from_min_size(Pos2::new(x, 0.0), egui::vec2(slot.w, slot.h)),
                    uv0: Pos2::new(slot.u0, slot.v0),
                    uv1: Pos2::new(slot.u1, slot.v1),
                    color: Color32::WHITE,
                },
            );
        };
        let mut f = g4.begin_frame(&h4, 3);
        emit4(&mut f, 0, 30, 'A', 0.0);
        f.set_row_hash(1, 33);
        emit4(&mut f, 2, 34, 'D', 0.0);
        emit4(&mut f, 2, 34, 'E', 10.0);
        let m7 = mesh_of(&mut f);
        drop(f);
        assert_eq!(m6.vertices, m7.vertices, "顶点应与全量重建一致");
assert_eq!(m6.indices, m7.indices, "索引应与全量重建一致");
        ctx.tex_manager().write().take_delta().clear();
    }
}
