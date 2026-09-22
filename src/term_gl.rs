//! 终端字形批渲染：字形预光栅化进一张纹理图集，格子直接拼 quad 提交，
//! 跳过逐格 Galley 排版与 epaint tessellation（终端区每帧上万三角形的
//! 主开销）。走 egui 自身纹理 + Mesh 管线，无自定义 shader/GL 调用：
//! 核显、软件渲染（WARP/llvmpipe）与普通 UI 元素同一条路，能跑本程序
//! 就能用。图集缺字形（emoji 等）的格子自动回落原 galley 路径，逐格混合。
//!
//! 静止帧优化：每格算 FNV 哈希入表，与上一帧全等则直接重放缓存的 Mesh，
//! 跳过 quad 重建；内容一变即整帧重建（不做局部更新——重建本身已是微秒级）。
//!
//! 内存控制（实测驱动）：
//! - 分页图集：单页 1024² RGBA ≈ 4MB，满页开新页渐进，不做整体清空重灌
//!   （整体清空曾造成汉字输入 ~400ms 卡顿）。
//! - 常驻字体链：fontdue::from_bytes 是全字形 eager 解析，CJK ~30K 字形
//!   几何 ≈ +127MB/字体。首次建图集即整条链拉满（含汉字字符集，进程级
//!   缓存 Arc 共享，多页签零重复解析）；首个缺字形不再中途补链 → 渲染/输入
//!   永无卡顿峰值。代价：CJK 解析结果常驻内存（用户明确要求常驻加载）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui;
use egui::{Color32, ColorImage, Mesh, Pos2, Rect, TextureHandle, TextureOptions};

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
const FONT_CACHE_MAX: usize = 4;

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

/// 单个字形的图集记录。UV 指向图集内的位图矩形；
/// dx 相对格子左缘、dy 相对基线的位图左上角偏移（逻辑点，dy 恒 ≤ 0）。
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
        };
        atlas.paint_white_texel();
        atlas
    }

    fn paint_white_texel(&mut self) {
        self.rgba[0..4].copy_from_slice(&[255, 255, 255, 255]);
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
}

/// 一个待提交的字形/实心 quad。
pub struct CellQuad {
    pub rect: Rect,
    pub uv0: Pos2,
    pub uv1: Pos2,
    pub color: Color32,
}

/// 把 quad 列表组装成 egui Mesh（两个三角形一格）。
pub fn build_mesh(quads: &[CellQuad], texture_id: egui::TextureId) -> Mesh {
    let mut mesh = Mesh::with_texture(texture_id);
    mesh.vertices.reserve(quads.len() * 4);
    mesh.indices.reserve(quads.len() * 6);
    for q in quads {
        let vi = mesh.vertices.len() as u32;
        let r = q.rect;
        let v = |pos: Pos2, uv: Pos2| egui::epaint::Vertex { pos, uv, color: q.color };
        mesh.vertices.extend([
            v(r.left_top(), q.uv0),
            v(r.right_top(), Pos2::new(q.uv1.x, q.uv0.y)),
            v(r.right_bottom(), q.uv1),
            v(r.left_bottom(), Pos2::new(q.uv0.x, q.uv1.y)),
        ]);
        mesh.indices.extend_from_slice(&[vi, vi + 1, vi + 2, vi, vi + 2, vi + 3]);
    }
    mesh
}

/// FNV-1a 搅拌一步。
#[inline]
pub fn hash_mix(h: &mut u64, v: u64) {
    *h ^= v;
    *h = h.wrapping_mul(0x100_0000_01b3);
}

// @@PART2@@/// 每会话 GPU 批渲染状态。`None` = 尚未初始化或初始化失败，整格走 galley 回落。
/// 分页架构：`pages[i]` 一张独立图集 + 独立纹理，`glyph()` 返回 (页号, 槽位)，
/// quad 也要带页号收集到 `quads[pg]`。
pub struct TermGpu {
    pub pages: Vec<GlyphAtlas>,
    /// 图集重建所需的原始字体字节（Arc 共享，DPI 变化时重建用）。
    sources: Vec<FontSource>,
    font_size_pt: f32,
    params_ppp: f32,
    pub texs: Vec<Option<TextureHandle>>,
    tex_versions: Vec<u64>,
    /// 上一帧每格哈希（rows×cols，索引 vline*cols+col，未访问格保持 0）。
    prev_hash: Vec<u64>,
    /// 本帧哈希写入缓冲（跨帧复用分配）。
    pub hash_scratch: Vec<u64>,
    /// 本帧 quad 收集缓冲（跨帧复用分配），按页分桶。
    pub quads: Vec<Vec<CellQuad>>,
    /// 静止帧复用的已提交网格（页序号 → 纹理 id + Arc<Mesh>）。
    pub meshes: Vec<(egui::TextureId, std::sync::Arc<Mesh>)>,
}

impl TermGpu {
    /// 从 egui 已注册的 Monospace 家族提取字体数据初始化。字体链为空返回 None。
    pub fn new(ctx: &egui::Context, font_size_pt: f32, ppp: f32) -> Option<Self> {
        let sources = cached_font_data(ctx); // 进程级零拷贝，见 FONT_DATA_CACHE
        if sources.is_empty() {
            return None;
        }
        let atlas = GlyphAtlas::new(&sources, font_size_pt, ppp);
        Some(Self {
            pages: vec![atlas],
            sources,
            font_size_pt,
            params_ppp: ppp,
            texs: vec![None],
            tex_versions: vec![0],
            prev_hash: Vec::new(),
            hash_scratch: Vec::new(),
            quads: vec![Vec::new()],
            meshes: Vec::new(),
        })
    }

    /// 图集是否完全为空（尚未光栅化任何字形）。首帧懒预热据此判断。
    pub fn is_empty(&self) -> bool {
        self.pages.iter().all(|p| p.is_empty())
    }

    /// DPI 变化时重建图集（光栅化字号随物理像素变化）；其余情况原地复用。
    pub fn ensure_params(&mut self, font_size_pt: f32, ppp: f32) {
        if self.params_ppp == ppp && self.font_size_pt == font_size_pt {
            return;
        }
        let sources = self.sources.clone(); // 避开 &mut self 与 &self 的借用冲突
        self.pages = vec![GlyphAtlas::new(&sources, font_size_pt, ppp)];
        self.texs = vec![None];
        self.tex_versions = vec![0];
        self.quads = vec![Vec::new()];
        self.meshes.clear();
        self.prev_hash.clear();
        self.params_ppp = ppp;
        self.font_size_pt = font_size_pt;
    }

    /// 帧首准备：哈希缓冲对齐 rows×cols，清空各页 quad 桶。
    pub fn begin_frame(&mut self, rows: usize, cols: usize) {
        let needed = rows * cols;
        if self.hash_scratch.len() >= needed {
            // 已有足够容量 → fill(0) 重置，无重分配。
            self.hash_scratch[..needed].fill(0);
            self.hash_scratch.truncate(needed);
        } else {
            self.hash_scratch.clear();
            self.hash_scratch.resize(needed, 0);
        }
        for q in &mut self.quads {
            q.clear();
        }
    }

    /// 查询/光栅化字形，返回 (页号, 槽位)。缺字形且末页满 → 开新页重试
    /// （页码单调递增，旧页字形常驻，永不整体重灌）。
    pub fn glyph(&mut self, ch: char) -> (usize, GlyphSlot) {
        // 跨页查缓存：从最新页往前找（新字形普遍落在最新页）。
        for i in (0..self.pages.len()).rev() {
            if let Some(s) = self.pages[i].peek(ch) {
                return (i, s);
            }
        }
        let last = self.pages.len() - 1;
        let slot = self.pages[last].glyph(ch);
        if slot.w > 0.0 || !self.pages[last].is_full() {
            return (last, slot);
        }
        // 末页满且该字形无可见笔画：开新页（新页懒加载未触发时缺字形自动补链）。
        let sources = self.sources.clone();
        let mut atlas = GlyphAtlas::new(&sources, self.font_size_pt, self.params_ppp);
        let slot = atlas.glyph(ch);
        let idx = self.pages.len();
        self.pages.push(atlas);
        self.texs.push(None);
        self.tex_versions.push(0);
        self.quads.push(Vec::new());
        (idx, slot)
    }

    /// 往指定页收一个 quad（跨帧复用分配）。
    #[inline]
    pub fn push_quad(&mut self, pg: usize, q: CellQuad) {
        if let Some(bucket) = self.quads.get_mut(pg) {
            bucket.push(q);
        }
    }

    /// 帧尾判定 + 网格组装。返回本帧应提交的 (纹理, Mesh) 列表；静止帧返回
    /// 上一帧的同一批 Arc（调用方 clone 后照常 add——egui 每帧都要画，
    /// 省的是 CPU 侧 quad→mesh 组装）。
    pub fn end_frame(&mut self, ctx: &egui::Context) -> &[(egui::TextureId, std::sync::Arc<Mesh>)] {
        let changed = self.prev_hash != self.hash_scratch;
        if changed {
            self.prev_hash.clear();
            self.prev_hash.extend_from_slice(&self.hash_scratch);
            self.meshes.clear();
            for (pi, page) in self.pages.iter().enumerate() {
                if self.texs[pi].is_none() || self.tex_versions[pi] != page.version {
                    self.texs[pi] = Some(ctx.load_texture(
                        format!("term_glyph_atlas_{pi}"),
                        page.image(),
                        TextureOptions::LINEAR,
                    ));
                    self.tex_versions[pi] = page.version;
                }
                // 空桶页跳过：该页无新字形，旧 mesh 继续有效。
                if !self.quads[pi].is_empty() {
                    let tid = self.texs[pi].as_ref().unwrap().id();
                    self.meshes
                        .push((tid, std::sync::Arc::new(build_mesh(&self.quads[pi], tid))));
                }
            }
        }
        self.meshes.as_slice()
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

    /// 基线应在格子内部（内容行高垂直居中）。
    #[test]
    fn baseline_inside_cell() {
        let a = test_atlas(2.0); // 高 DPI
        let b = a.baseline_rel(20.0);
        assert!(b > 0.0 && b < 20.0, "baseline={b}");
        assert!(a.underline_rel(20.0) > b);
    }

    /// Mesh 组装：quad 数 × 4 顶点 × 6 索引，索引引用合法。
    #[test]
    fn build_mesh_layout() {
        let quads = [CellQuad {
            rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(10.0, 20.0)),
            uv0: Pos2::ZERO,
            uv1: Pos2::new(1.0, 1.0),
            color: Color32::WHITE,
        }];
        let m = build_mesh(&quads, egui::TextureId::Managed(0));
        assert_eq!(m.vertices.len(), 4);
        assert_eq!(m.indices.len(), 6);
        assert!(m.is_valid());
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
}