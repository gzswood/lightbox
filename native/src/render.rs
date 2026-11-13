//! 软件渲染：32 位预乘 BGRA 画布 + 圆角矩形 + 位图合成 + 文本。
//!
//! 分层窗口（`WS_EX_LAYERED`）的像素是**预乘**的 BGBA：颜色通道已经乘过 alpha。
//! 这里所有绘制都在内存里完成，最后一次性 `UpdateLayeredWindow` 上去，
//! 这样圆角和抗锯齿都是逐像素精确的，不会出现 Win10 那种「1 像素台阶」。

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    GetTextExtentPoint32W, ReleaseDC, SelectObject, SetBkMode, SetTextAlign, SetTextColor,
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, ANTIALIASED_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE,
    DT_TOP, FF_DONTCARE, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, TA_LEFT, TA_TOP, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    UpdateLayeredWindow, ULW_ALPHA,
};

use std::sync::{OnceLock, RwLock};

use crate::config::{self, FONT_SIZE_BASE, FONT_SIZE_MAX, FONT_SIZE_MIN};
use crate::log;

/// 一张位图：BGRA，**非预乘**（alpha 0..255）
#[derive(Clone, Debug)]
pub struct IconBmp {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>,
}

impl IconBmp {
    pub fn empty(w: i32, h: i32) -> IconBmp {
        IconBmp {
            w,
            h,
            px: vec![0u8; (w * h * 4) as usize],
        }
    }
}

/// 文本的抗锯齿覆盖层：每个像素 0..255 表示「这里有多少字」
#[derive(Clone)]
pub struct TextBitmap {
    pub w: i32,
    pub h: i32,
    pub coverage: Vec<u8>,
}

/// 画布：预乘 BGBA
pub struct Canvas {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>,
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Canvas {
        Canvas {
            w,
            h,
            px: vec![0u8; (w.max(0) * h.max(0) * 4) as usize],
        }
    }

    /// 圆角矩形填充。`alpha` 是 0..255 的整体不透明度。
    pub fn fill_round_rect(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: f32,
        color: (u8, u8, u8),
        alpha: u8,
    ) {
        if w <= 0 || h <= 0 || alpha == 0 {
            return;
        }
        let hw = w as f32 * 0.5;
        let hh = h as f32 * 0.5;
        let cx = x as f32 + hw;
        let cy = y as f32 + hh;
        let r = radius.clamp(0.0, hw.min(hh));
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        for py in y0..y1 {
            for px in x0..x1 {
                let d = sd_round_rect(
                    px as f32 + 0.5,
                    py as f32 + 0.5,
                    cx,
                    cy,
                    hw,
                    hh,
                    r,
                );
                // 1 像素宽的抗锯齿过渡带
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let a = (alpha as f32 * cov).round().clamp(0.0, 255.0) as u8;
                self.blend_premul(px, py, color, a);
            }
        }
    }

    /// 圆角矩形描边（1px，带抗锯齿）。用于分层窗口的「防撞色」外框。
    pub fn stroke_round_rect(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: f32,
        color: (u8, u8, u8),
        alpha: u8,
    ) {
        if w <= 0 || h <= 0 || alpha == 0 {
            return;
        }
        let hw = w as f32 * 0.5;
        let hh = h as f32 * 0.5;
        let cx = x as f32 + hw;
        let cy = y as f32 + hh;
        let r = radius.clamp(0.0, hw.min(hh));
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        for py in y0..y1 {
            for px in x0..x1 {
                let d = sd_round_rect(
                    px as f32 + 0.5,
                    py as f32 + 0.5,
                    cx,
                    cy,
                    hw,
                    hh,
                    r,
                );
                // 1px 描边：边界两侧各半像素，再留 1 像素抗锯齿过渡
                let cov = (1.0 - d.abs()).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let a = (alpha as f32 * cov).round().clamp(0.0, 255.0) as u8;
                self.blend_premul(px, py, color, a);
            }
        }
    }

    /// 把一个颜色按覆盖度 alpha 混进来（dst 是预乘的）
    fn blend_premul(&mut self, x: i32, y: i32, color: (u8, u8, u8), a: u8) {
        let o = ((y * self.w + x) * 4) as usize;
        let (r, g, b) = color;
        let sr = (r as u32 * a as u32 / 255) as u8;
        let sg = (g as u32 * a as u32 / 255) as u8;
        let sb = (b as u32 * a as u32 / 255) as u8;
        let da = self.px[o + 3] as u32;
        let inv = 255 - a as u32;
        // dst 是预乘的，src 也先预乘，再做 over
        let out_a = a as u32 + da * inv / 255;
        if out_a == 0 {
            self.px[o] = 0;
            self.px[o + 1] = 0;
            self.px[o + 2] = 0;
            self.px[o + 3] = 0;
            return;
        }
        // 预乘的 over 就是 src + dst*(1-a)，**不能**再除 out_a。
        // 除 out_a 等于把结果「反预乘」回直通色 —— 整块不透明的地方看不出来
        // （out_a=255 时两种算法一样），但抗锯齿边缘（out_a<255）会把 RGB
        // 放大成满色，分层窗口按 AC_SRC_ALPHA 合成后，圆角透明处就多出
        // 一圈发白的描边。长条圆角、面板圆角都是这么来的。
        let mix = |sc: u8, dc: u8| -> u8 {
            ((sc as u32 * 255 + dc as u32 * inv) / 255).min(255) as u8
        };
        self.px[o] = mix(sb, self.px[o]);
        self.px[o + 1] = mix(sg, self.px[o + 1]);
        self.px[o + 2] = mix(sr, self.px[o + 2]);
        self.px[o + 3] = out_a.min(255) as u8;
    }

    /// 画一张位图（非预乘 BGRA），source-over 合成
    pub fn blend_icon(&mut self, x: i32, y: i32, img: &IconBmp) {
        for sy in 0..img.h {
            let dy = y + sy;
            if dy < 0 || dy >= self.h {
                continue;
            }
            for sx in 0..img.w {
                let dx = x + sx;
                if dx < 0 || dx >= self.w {
                    continue;
                }
                let i = (sy * img.w + sx) as usize * 4;
                let a = img.px[i + 3];
                if a == 0 {
                    continue;
                }
                let (b, g, r) = (img.px[i], img.px[i + 1], img.px[i + 2]);
                self.blend_premul(dx, dy, (r, g, b), a);
            }
        }
    }

    /// 画一张位图（非预乘 BGRA），用圆角矩形裁边，source-over 合成
    ///
    /// 毛玻璃的底图必须按圆角裁 —— 不裁的话四个角上会露出一块方形快照，
    /// 圆角窗口就废了。
    pub fn blend_bitmap_round(&mut self, x: i32, y: i32, img: &IconBmp, radius: f32) {
        let hw = img.w as f32 * 0.5;
        let hh = img.h as f32 * 0.5;
        let cx = x as f32 + hw;
        let cy = y as f32 + hh;
        let r = radius.clamp(0.0, hw.min(hh));
        for sy in 0..img.h {
            let dy = y + sy;
            if dy < 0 || dy >= self.h {
                continue;
            }
            for sx in 0..img.w {
                let dx = x + sx;
                if dx < 0 || dx >= self.w {
                    continue;
                }
                let i = (sy * img.w + sx) as usize * 4;
                let a = img.px[i + 3];
                if a == 0 {
                    continue;
                }
                let d = sd_round_rect(dx as f32 + 0.5, dy as f32 + 0.5, cx, cy, hw, hh, r);
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let (b, g, r_) = (img.px[i], img.px[i + 1], img.px[i + 2]);
                let a = (a as f32 * cov).round().clamp(0.0, 255.0) as u8;
                self.blend_premul(dx, dy, (r_, g, b), a);
            }
        }
    }

    /// 画文字覆盖层（coverage 是 0..255 的灰度）
    pub fn blend_text(&mut self, x: i32, y: i32, txt: &TextBitmap, color: (u8, u8, u8)) {        for sy in 0..txt.h {
            let dy = y + sy;
            if dy < 0 || dy >= self.h {
                continue;
            }
            for sx in 0..txt.w {
                let dx = x + sx;
                if dx < 0 || dx >= self.w {
                    continue;
                }
                let c = txt.coverage[(sy * txt.w + sx) as usize];
                if c == 0 {
                    continue;
                }
                self.blend_premul(dx, dy, color, c);
            }
        }
    }
}

/// 有向距离场：点到圆角矩形边界的距离（内部为负）
pub fn sd_round_rect(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - (hw - r);
    let qy = (py - cy).abs() - (hh - r);
    let ax = qx.max(0.0);
    let ay = qy.max(0.0);
    (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - r
}

fn create_font_w(px: i32, weight: i32, family: &str) -> HFONT {
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
            &windows::core::HSTRING::from(family),
        )
    }
}

/// 长条胶囊里的名字沿用旧网页版（参考软件）的字体：
/// `font-family: "Microsoft YaHei"; font-weight: 600`
const BAR_FONT_FAMILY: &str = "Microsoft YaHei";
const BAR_FONT_WEIGHT: i32 = 600;

/// 分层窗口文字的全局字体设置。设置页「外观 → 弹窗字体」应用后调 `set_font()`，
/// 启动时从 config.json 载入。这里存一份全局值，是为了不让每个绘制点都去
/// 取配置锁（文字渲染本来就在主线程热路径上）。
#[derive(Clone)]
pub struct FontCfg {
    pub family: String,
    /// 字号倍率 = 用户选的基准字号 / 12
    pub scale: f32,
}

static FONT_CFG: OnceLock<RwLock<FontCfg>> = OnceLock::new();

fn font_cfg() -> &'static RwLock<FontCfg> {
    FONT_CFG.get_or_init(|| {
        RwLock::new(FontCfg {
            family: config::default_panel_font(),
            scale: 1.0,
        })
    })
}

/// 应用新的弹窗字体 / 字号。**必须清缓存**，否则会继续复用旧字体渲染出来的位图。
pub fn set_font(family: &str, size_px: f32) {
    let name = family.trim();
    let family = if name.is_empty() {
        config::default_panel_font()
    } else {
        name.to_string()
    };
    let scale = (size_px.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX) / FONT_SIZE_BASE).max(0.5);
    *font_cfg().write().unwrap() = FontCfg { family, scale };
    clear_text_cache();
}

/// 丢掉所有已渲染的文字位图（换字体 / 换字号时用）
pub fn clear_text_cache() {
    if let Ok(mut guard) = TEXT_CACHE.lock() {
        guard.take();
    }
}

fn scaled_px(px: i32, cfg: &FontCfg) -> i32 {
    ((px as f32 * cfg.scale).round() as i32).max(1)
}

/// 量一段文字的宽高（像素）—— 按全局弹窗字体换算后的实际字号
pub fn measure_text(text: &str, px: i32, bold: bool) -> (i32, i32) {
    let cfg = font_cfg().read().unwrap().clone();
    measure_text_w(
        text,
        scaled_px(px, &cfg),
        if bold { 700 } else { 400 },
        &cfg.family,
    )
}

fn measure_text_w(text: &str, px: i32, weight: i32, family: &str) -> (i32, i32) {
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let font = create_font_w(px, weight, family);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let mut sz = SIZE::default();
        let w = crate::util::wide(text);
        let _ = GetTextExtentPoint32W(dc, &w, &mut sz);
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = ReleaseDC(None, screen);
        (sz.cx, sz.cy)
    }
}

/// 把一段文字渲染成抗锯齿覆盖层。
///
/// 用 GDI 画到一张 32 位 DIB 上（背景纯黑），GDI 的灰度抗锯齿让每个像素的
/// 亮度就是覆盖率；再用这个覆盖率按我们自己的颜色合成 —— 这样文字边缘
/// 能和圆角底板真正融合，而不是「一块矩形贴上去」。
pub fn render_text(text: &str, px: i32, bold: bool) -> Option<TextBitmap> {
    let cfg = font_cfg().read().unwrap().clone();
    render_text_w(
        text,
        scaled_px(px, &cfg),
        if bold { 700 } else { 400 },
        &cfg.family,
    )
}

fn render_text_w(text: &str, px: i32, weight: i32, family: &str) -> Option<TextBitmap> {
    if text.is_empty() {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let font = create_font_w(px, weight, family);
        let old_font = SelectObject(dc, HGDIOBJ(font.0));

        let mut sz = SIZE::default();
        let wchars = crate::util::wide(text);
        let _ = GetTextExtentPoint32W(dc, &wchars, &mut sz);
        let mut w = sz.cx;
        let mut h = sz.cy;
        if w <= 0 || h <= 0 {
            SelectObject(dc, old_font);
            let _ = DeleteObject(HGDIOBJ(font.0));
            let _ = ReleaseDC(None, screen);
            return None;
        }
        // 留一点余量，避免字形被裁
        w += px / 2;
        h += px / 2;

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 顶向下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = match CreateDIBSection(
            Some(dc),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        ) {
            Ok(b) => b,
            Err(_) => {
                SelectObject(dc, old_font);
                let _ = DeleteObject(HGDIOBJ(font.0));
                let _ = ReleaseDC(None, screen);
                return None;
            }
        };
        let old_bmp = SelectObject(dc, HGDIOBJ(hbmp.0));
        std::ptr::write_bytes(bits as *mut u8, 0, (w * h * 4) as usize);

        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(0x00FF_FFFF));
        let _ = SetTextAlign(dc, TA_LEFT | TA_TOP);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        let mut buf: Vec<u16> = wchars[..wchars.len() - 1].to_vec();
        buf.push(0);
        let _ = DrawTextW(
            dc,
            &mut buf,
            &mut rect,
            DT_LEFT | DT_TOP | DT_NOPREFIX | DT_SINGLELINE,
        );

        let raw = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
        let mut coverage = vec![0u8; (w * h) as usize];
        for i in 0..(w * h) as usize {
            coverage[i] = raw[i * 4]; // GDI 灰度抗锯齿：亮度 == 覆盖率
        }

        SelectObject(dc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = ReleaseDC(None, screen);

        Some(TextBitmap { w, h, coverage })
    }
}

/// 一段文字：位图 + 文字本身的宽高
#[derive(Clone)]
pub struct Text {
    pub bmp: TextBitmap,
    /// 文字实际占的宽（不含抗锯齿余量）
    pub w: i32,
    /// 文字实际占的高
    pub h: i32,
}

static TEXT_CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, Text>>> =
    std::sync::Mutex::new(None);

/// 文字缓存：key 命中就直接复用位图，否则调 `make()` 渲染一份。
/// 内容会变（比如 hex 输入框），缓存不能无限长。
fn cached_text(key: String, make: impl FnOnce() -> Option<Text>) -> Option<Text> {
    {
        let guard = TEXT_CACHE.lock().unwrap();
        if let Some(map) = guard.as_ref() {
            if let Some(hit) = map.get(&key) {
                return Some(hit.clone());
            }
        }
    }
    let out = make()?;
    let mut guard = TEXT_CACHE.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    if map.len() > 256 {
        map.clear();
    }
    map.insert(key, out.clone());
    Some(out)
}

/// 渲染一段分层窗口文字（弹窗面板 / 改名框 / 提示文字）。
/// 字体名和字号倍率来自全局设置，缓存 key 里带上，换字体后不会串味。
pub fn text(text: &str, px: i32, bold: bool) -> Option<Text> {
    let cfg = font_cfg().read().unwrap().clone();
    let key = format!("ui|{}|{:.4}|{px}|{bold}|{text}", cfg.family, cfg.scale);
    cached_text(key, || {
        let bmp = render_text(text, px, bold)?;
        let (w, h) = measure_text(text, px, bold);
        Some(Text { bmp, w, h })
    })
}

/// 长条胶囊里的名字（参考软件的微软雅黑 600）。和普通 UI 文字分开缓存。
/// 长条尺寸固定，不参与「弹窗字体」设置，所以这里用固定字体。
pub fn bar_text(text: &str, px: i32) -> Option<Text> {
    let key = format!("bar|{px}|{text}");
    {
        let guard = TEXT_CACHE.lock().unwrap();
        if let Some(map) = guard.as_ref() {
            if let Some(hit) = map.get(&key) {
                return Some(hit.clone());
            }
        }
    }
    let bmp = render_text_w(text, px, BAR_FONT_WEIGHT, BAR_FONT_FAMILY)?;
    let (w, h) = measure_text_w(text, px, BAR_FONT_WEIGHT, BAR_FONT_FAMILY);
    let out = Text { bmp, w, h };
    let mut guard = TEXT_CACHE.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    if map.len() > 256 {
        map.clear();
    }
    map.insert(key, out.clone());
    Some(out)
}

/// 把画布推到分层窗口上（预乘 BGRA + ULW_ALPHA）
pub fn push_layered(hwnd: HWND, canvas: &Canvas) {
    unsafe {
        let hdc_screen = GetDC(None);
        let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: canvas.w,
                biHeight: -canvas.h, // 顶向下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };
        let hbmp = match CreateDIBSection(
            Some(hdc_mem),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        ) {
            Ok(b) => b,
            Err(e) => {
                log::info(&format!("创建 DIB 失败：{e}"));
                let _ = ReleaseDC(None, hdc_screen);
                return;
            }
        };
        let old = SelectObject(hdc_mem, HGDIOBJ(hbmp.0));
        std::ptr::copy_nonoverlapping(
            canvas.px.as_ptr() as *const core::ffi::c_void,
            bits,
            canvas.px.len(),
        );

        let size = SIZE {
            cx: canvas.w,
            cy: canvas.h,
        };
        let pt = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: 0,  // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };
        // pptdst 传 None：否则 UpdateLayeredWindow 会把窗口搬到屏幕 (0,0)
        if let Err(e) = UpdateLayeredWindow(
            hwnd,
            Some(hdc_screen),
            None,
            Some(&size),
            Some(hdc_mem),
            Some(&pt),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            log::info(&format!("UpdateLayeredWindow 失败：{e}"));
        }
        SelectObject(hdc_mem, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
    }
}
