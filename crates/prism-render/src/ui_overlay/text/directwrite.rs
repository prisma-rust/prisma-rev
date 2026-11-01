//! 文本光栅后端 B：Windows 系统 DirectWrite（`ui-directwrite`）。
//!
//! 目的：Windows 发布包不再链接 `fontdue` + `ttf-parser`（约 90 KB），改用系统
//! `dwrite.dll`。字形位图来自 `IDWriteBitmapRenderTarget`（DirectWrite 的 GDI interop
//! 位图目标）：把**单个字形**画进一张 32bpp BGRA 内存位图，扫描出墨迹包围盒后转成
//! 图集用的 8-bit 覆盖度。
//!
//! 为什么一个字形一个字形画，而不是整行一次性画：`ui_overlay::gpu` 的图集/四边形
//! 展开是**逐字形**的（对齐、advance 推进、缓存键都是 `(char, font_size)`）。整行位图
//! 需要改图集缓存键与对齐逻辑，收益只是省几次 GDI 调用——不值得为它改动与后端无关
//! 的那一层。逐字形也让两个后端（fontdue / DirectWrite）的布局完全可比。
//!
//! 渲染方式：
//! * 先把位图刷白（`PatBlt(WHITENESS)`），再用黑色 + **灰度**抗锯齿画字形
//!   （`CreateCustomRenderingParams(.., cleartype_level = 0.0, ..)`，ClearType level 0
//!   就是单通道灰度 AA），覆盖度 = `255 - R`。这样不依赖位图 alpha 通道的填充约定。
//! * `pixels_per_dip = 1.0`：UI 坐标本来就是像素，缩放交给窗口。
//!
//! **限制（与 fontdue 后端一致）**：不做复杂文本整形（阿拉伯/印度语系连写）、
//! 可变字体、Emoji 彩色字形、kerning/ligature——这些仍然只有 fontdue 的
//! `rasterize(char)` 级兜底（同样不整形），或根本不支持。

use std::mem::{size_of, ManuallyDrop};

use anyhow::{Context as _, Result};
use windows::core::{BOOL, HSTRING};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFontCollection,
    IDWriteFontFace, IDWriteFontFamily, IDWriteGdiInterop, IDWriteRenderingParams,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_OFFSET,
    DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PIXEL_GEOMETRY_RGB,
    DWRITE_RENDERING_MODE_NATURAL,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, PatBlt, SelectObject,
    SetBkMode, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    SRCCOPY, TRANSPARENT, WHITENESS,
};

use super::coverage::crop_bgra_coverage;
use super::{GlyphBitmap, LineMetrics, TextRasterizer};

/// 字体族候选，顺序与 fontdue 的 Windows 候选（msyh.ttc → simhei.ttf → arial.ttf）对齐，
/// 这样两个后端画出来的字体一致。全部缺失时回退 [`FALLBACK_FAMILY`]。
const FAMILY_CANDIDATES: &[&str] = &["Microsoft YaHei", "Microsoft YaHei UI", "SimHei", "Arial"];

/// 兜底字体族（Windows Vista 起随系统提供，含拉丁但**不含中文**）。
const FALLBACK_FAMILY: &str = "Segoe UI";

/// 临时位图四周留白（px），避免负 bearing / 掉墨被裁掉。
const PAD: u32 = 2;

pub(crate) struct DirectWriteRasterizer {
    face: IDWriteFontFace,
    /// 字体设计度量（em 单位），换算像素时除以 `designUnitsPerEm`。
    metrics: DWRITE_FONT_METRICS,
    interop: IDWriteGdiInterop,
    /// 灰度 AA 的渲染参数（ClearType level = 0）。
    params: IDWriteRenderingParams,
    /// 单个字形用的临时位图目标；换字号时 `Resize`，不重建。
    target: Option<IDWriteBitmapRenderTarget>,
    /// `target` 当前边长（px）；0 = 还没建。
    box_px: u32,
    /// 读回面，边长跟着 `box_px` 走。
    readback: Option<ReadbackSurface>,
}

impl DirectWriteRasterizer {
    pub(crate) fn new() -> Option<Self> {
        match Self::try_new() {
            Ok(this) => Some(this),
            Err(err) => {
                log::warn!("ui overlay: DirectWrite 后端初始化失败（{err:#}）—— 文字命令将被跳过");
                None
            }
        }
    }

    fn try_new() -> Result<Self> {
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
            .context("DWriteCreateFactory")?;

        let mut collection: Option<IDWriteFontCollection> = None;
        unsafe { factory.GetSystemFontCollection(&mut collection, false) }
            .context("GetSystemFontCollection")?;
        let collection = collection.context("系统字体集合为空")?;

        let (family_name, family) = pick_family(&collection)?;
        let font = unsafe {
            family.GetFirstMatchingFont(
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
            )
        }
        .with_context(|| format!("{family_name}: GetFirstMatchingFont"))?;
        let face = unsafe { font.CreateFontFace() }
            .with_context(|| format!("{family_name}: CreateFontFace"))?;

        let mut metrics = DWRITE_FONT_METRICS::default();
        unsafe { face.GetMetrics(&mut metrics) };
        if metrics.designUnitsPerEm == 0 {
            anyhow::bail!("{family_name}: DWRITE_FONT_METRICS.designUnitsPerEm = 0");
        }

        let interop = unsafe { factory.GetGdiInterop() }.context("GetGdiInterop")?;
        // ClearType level 0 = 灰度抗锯齿（单通道覆盖度，正好是 R8 图集要的东西）。
        let params = match unsafe {
            factory.CreateCustomRenderingParams(
                1.8,
                0.5,
                0.0,
                DWRITE_PIXEL_GEOMETRY_RGB,
                DWRITE_RENDERING_MODE_NATURAL,
            )
        } {
            Ok(params) => params,
            Err(err) => {
                // 退到系统默认参数：可能是 ClearType（R/B 通道有彩边），
                // 覆盖度取 R 通道会略有色偏 —— 先警告，别静默变糊。
                log::warn!(
                    "ui overlay: 创建灰度 AA 渲染参数失败（{err}），退回系统默认渲染参数\
                     —— 文本可能有彩边"
                );
                unsafe { factory.CreateRenderingParams() }.context("CreateRenderingParams")?
            }
        };

        log::info!("ui overlay: DirectWrite 后端就绪，字体族 {family_name}");
        Ok(Self {
            face,
            metrics,
            interop,
            params,
            target: None,
            box_px: 0,
            readback: None,
        })
    }

    /// em 单位 → 像素。
    fn scale(&self, font_size: f32) -> f32 {
        font_size / f32::from(self.metrics.designUnitsPerEm)
    }

    /// 取/建边长为 `box_px` 的临时位图目标（之后用 `self.target` 取用）。
    fn ensure_target(&mut self, box_px: u32) -> Result<()> {
        if self.box_px != box_px {
            match &self.target {
                Some(target) => unsafe { target.Resize(box_px, box_px) }
                    .with_context(|| format!("IDWriteBitmapRenderTarget::Resize({box_px})"))?,
                None => {
                    let target =
                        unsafe { self.interop.CreateBitmapRenderTarget(None, box_px, box_px) }
                            .context("CreateBitmapRenderTarget")?;
                    // UI 坐标是像素：1 dip = 1 px，系统缩放由窗口负责。
                    unsafe { target.SetPixelsPerDip(1.0) }.context("SetPixelsPerDip")?;
                    self.target = Some(target);
                }
            }
            self.box_px = box_px;
        }
        Ok(())
    }

    /// 取/建读回面（与临时位图同尺寸）。
    fn ensure_readback(&mut self, box_px: u32) -> Result<()> {
        if self.readback.as_ref().is_some_and(|rb| rb.box_px == box_px) {
            return Ok(());
        }
        // 先释放旧的：它可能正被上一次读回持有着。
        self.readback = None;
        self.readback = Some(ReadbackSurface::new(box_px)?);
        Ok(())
    }

    fn try_rasterize(&mut self, ch: char, font_size: f32) -> Result<GlyphBitmap> {
        let scale = self.scale(font_size);

        // 1. 字符 → 字形索引 + 设计度量。
        let mut glyph_index = 0u16;
        let codepoint = ch as u32;
        unsafe {
            self.face
                .GetGlyphIndices(&codepoint as *const u32, 1, &mut glyph_index as *mut u16)
        }
        .with_context(|| format!("GetGlyphIndices({ch:?})"))?;
        let mut design = DWRITE_GLYPH_METRICS::default();
        unsafe {
            self.face.GetDesignGlyphMetrics(
                &glyph_index as *const u16,
                1,
                &mut design as *mut DWRITE_GLYPH_METRICS,
                false,
            )
        }
        .with_context(|| format!("GetDesignGlyphMetrics({ch:?})"))?;
        let advance = design.advanceWidth as f32 * scale;

        // 2. 临时位图：2em 见方足够放任何单字形的墨迹（含负 bearing 与下伸部）。
        let em_px = font_size.ceil().max(1.0) as u32;
        let box_px = em_px * 2 + PAD * 2;
        let ascent_px = self.metrics.ascent as f32 * scale;
        // 整数原点：原点不落半像素，裁出来的 xmin/ymin 才是整像素。
        // 2em 见方的框里把笔位置放在左侧 60%：负 bearing / 下伸部 / 右侧溢出都有余量。
        let origin_x = PAD as f32 + (em_px as f32 * 0.6).round();
        let origin_y = (PAD as f32 + ascent_px.ceil()).min((box_px - 1) as f32);

        self.ensure_target(box_px)?;
        let target = self
            .target
            .as_ref()
            .context("bitmap render target 不可用")?;
        let hdc = unsafe { target.GetMemoryDC() };

        // 3. 刷白 + 透明背景模式 + 黑色字形 → 覆盖度 = 255 - R。
        unsafe {
            // 刷白失败会留下上一个字形的墨迹，被裁进新字形里 —— 必须当错误处理。
            PatBlt(hdc, 0, 0, box_px as i32, box_px as i32, WHITENESS)
                .ok()
                .context("PatBlt(WHITENESS) 清空临时位图")?;
            SetBkMode(hdc, TRANSPARENT);
        }

        let indices = [glyph_index];
        let advances = [advance];
        let offsets = [DWRITE_GLYPH_OFFSET::default()];
        let mut run = DWRITE_GLYPH_RUN {
            // `ManuallyDrop`：bindings 的字段约定，clone 出来的引用要在调用后手动 Release。
            fontFace: ManuallyDrop::new(Some(self.face.clone())),
            fontEmSize: font_size,
            glyphCount: 1,
            glyphIndices: indices.as_ptr(),
            glyphAdvances: advances.as_ptr(),
            glyphOffsets: offsets.as_ptr(),
            isSideways: BOOL(0),
            bidiLevel: 0,
        };
        let mut black_box = RECT::default();
        let drawn = unsafe {
            target.DrawGlyphRun(
                origin_x,
                origin_y,
                DWRITE_MEASURING_MODE_NATURAL,
                &run,
                &self.params,
                COLORREF(0),
                Some(&mut black_box),
            )
        };
        // 与上面的 clone 配对，避免每次光栅化漏一个 AddRef。
        unsafe { drop(ManuallyDrop::take(&mut run.fontFace)) };
        drawn.with_context(|| format!("DrawGlyphRun({ch:?})"))?;

        // 4. 读回 BGRA（BitBlt 到我们自己的 top-down DIB，见 ReadbackSurface）。
        self.ensure_readback(box_px)?;
        let readback = self.readback.as_ref().context("读回面不可用")?;
        readback.capture(hdc)?;
        let (glyph, clipped) =
            crop_bgra_coverage(readback.pixels(), box_px, origin_x, origin_y, advance);
        if clipped {
            // 2em 见方的临时位图对正常字体不该裁到墨迹；裁到说明字形异常大。
            log::warn!("ui overlay: DirectWrite 字形 {ch:?} @ {font_size}px 可能被临时位图裁切");
        }
        Ok(glyph)
    }
}

impl TextRasterizer for DirectWriteRasterizer {
    fn line_metrics(&self, _font: Option<&str>, font_size: f32) -> Option<LineMetrics> {
        // DirectWrite 后端没有工程字体注册表（`register_font` 用 trait 默认实现
        // 返回 false），所有文本都走系统字体（#583）。
        let scale = self.scale(font_size);
        let ascent = self.metrics.ascent as f32 * scale;
        let descent = self.metrics.descent as f32 * scale;
        let line_gap = self.metrics.lineGap as f32 * scale;
        Some(LineMetrics {
            ascent,
            descent,
            line_height: ascent + descent + line_gap,
        })
    }

    fn rasterize(&mut self, _font: Option<&str>, ch: char, font_size: f32) -> Option<GlyphBitmap> {
        match self.try_rasterize(ch, font_size) {
            Ok(glyph) => Some(glyph),
            Err(err) => {
                // 每个 (char, size) 只会走到这里一次（结果会被图集缓存），不刷屏。
                log::warn!("ui overlay: DirectWrite 光栅化 {ch:?} @ {font_size}px 失败：{err:#}");
                Some(GlyphBitmap {
                    width: 0,
                    height: 0,
                    advance: 0.0,
                    xmin: 0,
                    ymin: 0,
                    pixels: Vec::new(),
                })
            }
        }
    }
}

/// 位图读回面：一块**我们自己建的 top-down 32bpp DIB section**。
///
/// 为什么不直接读 `IDWriteBitmapRenderTarget` 的 memory DC 位图：
/// * `GetDIBits` 的文档要求「位图不能正选中在 DC 里」，而这个位图一直被 DWrite 选着；
/// * `BITMAP.bmBits` 的指针方向取决于 DIB 原始行序，读错就是上下颠倒。
///
/// `BitBlt` 在两个 DC 之间是**逻辑坐标到逻辑坐标**的拷贝，方向由我们自己的
/// 负高度 DIB 头决定，所以既不受 DWrite 内部行序影响，也不碰 GDI 的使用限制。
struct ReadbackSurface {
    dc: HDC,
    bitmap: HBITMAP,
    /// 新建 DC 时默认选中的 1x1 位图，销毁前要换回去。
    previous: HGDIOBJ,
    bits: *mut u8,
    box_px: u32,
}

impl ReadbackSurface {
    fn new(box_px: u32) -> Result<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: box_px as i32,
                // 负高度 = top-down：`bits` 的第 0 行就是屏幕上那一行。
                biHeight: -(box_px as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                anyhow::bail!("CreateCompatibleDC 失败");
            }
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(bitmap) => bitmap,
                Err(err) => {
                    let _ = DeleteDC(dc);
                    return Err(err).context("CreateDIBSection");
                }
            };
            if bits.is_null() {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                anyhow::bail!("CreateDIBSection 没有给出像素指针");
            }
            let previous = SelectObject(dc, bitmap.into());
            Ok(Self {
                dc,
                bitmap,
                previous,
                bits: bits.cast(),
                box_px,
            })
        }
    }

    /// 把 `source`（DWrite 的 memory DC）整块拷过来。
    fn capture(&self, source: HDC) -> Result<()> {
        let size = self.box_px as i32;
        unsafe { BitBlt(self.dc, 0, 0, size, size, Some(source), 0, 0, SRCCOPY) }
            .context("BitBlt 读回字形位图")
    }

    fn pixels(&self) -> &[u8] {
        let len = self.box_px as usize * self.box_px as usize * 4;
        // 安全性：`bits` 来自 CreateDIBSection，长度即 box_px² × 4 字节 BGRA。
        unsafe { std::slice::from_raw_parts(self.bits, len) }
    }
}

impl Drop for ReadbackSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// 按候选顺序找字体族，最后回退 [`FALLBACK_FAMILY`]。
fn pick_family(collection: &IDWriteFontCollection) -> Result<(String, IDWriteFontFamily)> {
    for name in FAMILY_CANDIDATES
        .iter()
        .copied()
        .chain(std::iter::once(FALLBACK_FAMILY))
    {
        let wide = HSTRING::from(name);
        let mut index = 0u32;
        let mut exists = BOOL(0);
        unsafe { collection.FindFamilyName(&wide, &mut index, &mut exists) }
            .with_context(|| format!("FindFamilyName({name})"))?;
        if exists.as_bool() {
            let family = unsafe { collection.GetFontFamily(index) }
                .with_context(|| format!("GetFontFamily({name})"))?;
            return Ok((name.to_string(), family));
        }
    }
    anyhow::bail!(
        "系统字体集合里没有候选字体族（{}）",
        FAMILY_CANDIDATES
            .iter()
            .chain(std::iter::once(&FALLBACK_FAMILY))
            .copied()
            .collect::<Vec<_>>()
            .join(" / ")
    )
}
