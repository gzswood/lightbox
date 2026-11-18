//! 软件渲染：32 位预乘 BGRA 画布 + 圆角矩形 + 位图合成 + 文本。
//!
//! 分层窗口（`WS_EX_LAYERED`）的像素是**预乘**的 BGBA：颜色通道已经乘过 alpha。
//! 这里所有绘制都在内存里完成，最后一次性 `UpdateLayeredWindow` 上去，
//! 这样圆角和抗锯齿都是逐像素精确的，不会出现 Win10 那种「1 像素台阶」。

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    GetCharWidth32W, GetTextExtentPoint32W, ReleaseDC, SelectObject, SetBkColor, SetBkMode, SetTextAlign,
    SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, ANTIALIASED_QUALITY,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_LEFT, DT_NOPREFIX,
    DT_SINGLELINE, DT_TOP, FF_DONTCARE, FONT_QUALITY, HFONT, HGDIOBJ, OPAQUE, OUT_DEFAULT_PRECIS,
    TA_LEFT, TA_TOP,
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

    /// 贴一张「已经是最终颜色」的文字位图（ClearType 直出）。
    ///
    /// 两条规矩：
    /// 1. 只写**被字压到**的像素（和底色一模一样的就是空白处），否则选中高亮、
    ///    分隔线、圆角这些底下的东西会被文字位图整块盖掉；
    /// 2. 只往**不透明**的底子上写 —— 半透明面板、毛玻璃、圆角边缘保持原样。
    pub fn blit_text_on(&mut self, x: i32, y: i32, t: &TextOn) {
        let (bb, bg, br) = (t.bg.2, t.bg.1, t.bg.0);
        for sy in 0..t.h {
            let dy = y + sy;
            if dy < 0 || dy >= self.h {
                continue;
            }
            for sx in 0..t.w {
                let dx = x + sx;
                if dx < 0 || dx >= self.w {
                    continue;
                }
                let s = ((sy * t.w + sx) * 4) as usize;
                let (b, g, r) = (t.bgra[s], t.bgra[s + 1], t.bgra[s + 2]);
                if b == bb && g == bg && r == br {
                    continue; // 空白处：底下的东西照旧
                }
                let o = ((dy * self.w + dx) * 4) as usize;
                if self.px[o + 3] != 255 {
                    continue;
                }
                self.px[o] = b;
                self.px[o + 1] = g;
                self.px[o + 2] = r;
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

fn create_font_w(px: i32, weight: i32, family: &str, quality: FONT_QUALITY) -> HFONT {
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
            quality,
            (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
            &windows::core::HSTRING::from(family),
        )
    }
}

/// 覆盖率 → 视觉覆盖率的 gamma。
///
/// GDI 的 `ANTIALIASED_QUALITY` 给出来的是**线性覆盖率**，DirectWrite（旧版
/// WebView2 里 Chromium 用的那套）还会再做一次 gamma 校正。同一个字号、同一个
/// 字重，线性混合出来的字看着明显更细更虚 —— 弹窗里 13px 标题、12px 图标名
/// 尤其明显（同样是 Regular，旧版看着干净，新版像隔了一层）。
/// 趁生成覆盖率位图时补上这一段；位图有缓存，不增加每帧开销。
const TEXT_GAMMA: f32 = 0.82;

fn gamma_lut() -> &'static [u8; 256] {
    static LUT: OnceLock<[u8; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u8; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let f = (i as f32 / 255.0).powf(TEXT_GAMMA) * 255.0;
            *v = f.round().clamp(0.0, 255.0) as u8;
        }
        t
    })
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
        let font = create_font_w(px, weight, family, ANTIALIASED_QUALITY);
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

/// 字宽缓存：字体配置 → 单个字符的进位宽度
static CHAR_W: std::sync::Mutex<
    Option<std::collections::HashMap<String, std::collections::HashMap<char, i32>>>,
> = std::sync::Mutex::new(None);

/// 一段文字**真实进位**的宽度（按字累加 `GetCharWidth32W`）。
///
/// 不能拿 `GetTextExtentPoint32W`（`measure_text`）来判「一行放不放得下」：
/// 它对西文会多算一成（16px 的「HiBitUninstaller」量出 125px，逐字相加只有 117px），
/// 名字会提前折行、或者明明放得下却被省略号砍掉。
pub fn measure_exact(text: &str, px: i32, bold: bool) -> i32 {
    char_widths(text, px, bold).iter().sum()
}

/// 逐个字符的进位宽度（折行要按字累加，逐个问一次锁太贵，这里一次给全）。
///
/// 字宽跟字号 / 字族 / 字重绑死，所以按字符缓存 —— 图标名一屏几十个，
/// 每帧都建一次 DC 重新量一遍太浪费。
pub fn char_widths(text: &str, px: i32, bold: bool) -> Vec<i32> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let cfg = font_cfg().read().unwrap().clone();
    let px = scaled_px(px, &cfg);
    let weight = if bold { 700 } else { 400 };
    let key = format!("{}|{:.4}|{px}|{weight}", cfg.family, cfg.scale);
    // 快路径：每个字都量过了，直接取
    {
        let guard = CHAR_W.lock().unwrap();
        if let Some(outer) = guard.as_ref() {
            if let Some(inner) = outer.get(&key) {
                if chars.iter().all(|c| inner.contains_key(c)) {
                    return chars.iter().map(|c| inner[c]).collect();
                }
            }
        }
    }
    let widths: Vec<(char, i32)> = unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let font = create_font_w(px, weight, &cfg.family, CLEARTYPE_QUALITY);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let out: Vec<(char, i32)> = chars
            .iter()
            .map(|&ch| {
                let mut w = 0i32;
                let ok = GetCharWidth32W(dc, ch as u32, ch as u32, &mut w);
                (ch, if ok.as_bool() { w } else { px / 2 })
            })
            .collect();
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = ReleaseDC(None, screen);
        out
    };
    let mut guard = CHAR_W.lock().unwrap();
    let outer = guard.get_or_insert_with(std::collections::HashMap::new);
    if outer.len() > 32 {
        outer.clear();
    }
    let inner = outer.entry(key).or_default();
    for (c, w) in &widths {
        inner.insert(*c, *w);
    }
    widths.iter().map(|(_, w)| *w).collect()
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

/// 文字渲染核心。
///
/// `quality` 决定光栅化方式，`bg` / `fg` 只有 `CLEARTYPE_QUALITY` 才用得上
/// （ClearType 是亚像素抗锯齿，必须知道「底是什么颜色」才能算出最终像素）。
/// 返回**原始 BGRA**，由调用方决定怎么用：
/// - `ANTIALIASED_QUALITY` + 黑底白字 → 取 R 通道当灰度覆盖率（alpha 合成）
/// - `CLEARTYPE_QUALITY` + 真实底色/字色 → 像素本身就是最终颜色（直出）
fn render_text_impl(
    text: &str,
    px: i32,
    weight: i32,
    family: &str,
    quality: FONT_QUALITY,
    bg: (u8, u8, u8),
    fg: (u8, u8, u8),
) -> Option<(i32, i32, Vec<u8>)> {
    if text.is_empty() {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let font = create_font_w(px, weight, family, quality);
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
        // 底色铺满整个位图：ClearType 要的「已知不透明背景」，灰度 AA 下黑底也不吃亏
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        for i in 0..(w * h) as usize {
            dst[i * 4] = bg.2;
            dst[i * 4 + 1] = bg.1;
            dst[i * 4 + 2] = bg.0;
            dst[i * 4 + 3] = 255;
        }

        let cref = |c: (u8, u8, u8)| COLORREF((c.0 as u32) | ((c.1 as u32) << 8) | ((c.2 as u32) << 16));
        let _ = SetBkColor(dc, cref(bg));
        let _ = SetBkMode(dc, OPAQUE);
        let _ = SetTextColor(dc, cref(fg));
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

        let raw = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();

        SelectObject(dc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = ReleaseDC(None, screen);

        Some((w, h, raw))
    }
}

/// 灰度覆盖率位图（alpha 合成用）：黑底白字 + `ANTIALIASED_QUALITY`
fn render_text_w(text: &str, px: i32, weight: i32, family: &str) -> Option<TextBitmap> {
    let (w, h, raw) = render_text_impl(
        text,
        px,
        weight,
        family,
        ANTIALIASED_QUALITY,
        (0, 0, 0),
        (255, 255, 255),
    )?;
    let lut = gamma_lut();
    let mut coverage = vec![0u8; (w * h) as usize];
    for i in 0..(w * h) as usize {
        // GDI 灰度抗锯齿：亮度 == 覆盖率。再补一次 gamma 校正，见 `TEXT_GAMMA`。
        coverage[i] = lut[raw[i * 4] as usize];
    }
    Some(TextBitmap { w, h, coverage })
}

/// 一张**已经是最终颜色**的文字位图（不透明，ClearType 直出用）
#[derive(Clone)]
pub struct TextOn {
    pub w: i32,
    pub h: i32,
    pub bgra: Vec<u8>,
    /// 渲染时用的底色 —— 贴图时凭它跳过「没被字压到」的像素，
    /// 否则选中高亮、分隔线这些底下的东西会被整块盖掉
    pub bg: (u8, u8, u8),
}

static TEXT_ON_CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, TextOn>>> =
    std::sync::Mutex::new(None);

/// ClearType 直出：在**已知底色**上渲染，拿到的像素就是屏幕上该显示的颜色。
///
/// 这是旧版（WebView2 / DirectWrite）看起来清楚的真正原因 —— 亚像素抗锯齿保留了
/// 笔画落在「半像素」时的位置信息。灰度覆盖率把这些信息求平均掉了，12px 中文
/// 就糊成一团灰边。
///
/// 只有**完全不透明**的面板能用：半透明 / 毛玻璃时底色不可知，调用方要退回
/// `text()` + `blend_text()` 那条路。
pub fn text_on(
    text: &str,
    px: i32,
    bold: bool,
    bg: (u8, u8, u8),
    fg: (u8, u8, u8),
) -> Option<TextOn> {
    let cfg = font_cfg().read().unwrap().clone();
    text_on_w(
        text,
        scaled_px(px, &cfg), // 和 render_text 一样吃「设置里的字号倍率」
        if bold { 700 } else { 400 },
        &cfg.family,
        bg,
        fg,
    )
}

/// 同上，但字体族 / 字重自己指定（长条的名字用的是固定字体，不走「弹窗字体」设置）
pub fn text_on_w(
    text: &str,
    px: i32,
    weight: i32,
    family: &str,
    bg: (u8, u8, u8),
    fg: (u8, u8, u8),
) -> Option<TextOn> {
    if text.is_empty() {
        return None;
    }
    let key = format!(
        "on|{family}|{px}|{weight}|{:02X}{:02X}{:02X}|{:02X}{:02X}{:02X}|{text}",
        bg.0, bg.1, bg.2, fg.0, fg.1, fg.2
    );
    {
        let guard = TEXT_ON_CACHE.lock().unwrap();
        if let Some(map) = guard.as_ref() {
            if let Some(hit) = map.get(&key) {
                return Some(hit.clone());
            }
        }
    }
    let (w, h, bgra) = render_text_impl(
        text,
        px,
        weight,
        family,
        CLEARTYPE_QUALITY,
        bg,
        fg,
    )?;
    let out = TextOn { w, h, bgra, bg };
    let mut guard = TEXT_ON_CACHE.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    if map.len() > 256 {
        map.clear();
    }
    map.insert(key, out.clone());
    Some(out)
}

/// 长条分段名字的直出版：字体固定（不吃「弹窗字体」设置），和 `bar_text` 对齐
pub fn bar_text_on(text: &str, px: i32, bg: (u8, u8, u8), fg: (u8, u8, u8)) -> Option<TextOn> {
    text_on_w(text, px, BAR_FONT_WEIGHT, BAR_FONT_FAMILY, bg, fg)
}

/// 这一帧的「文字直出」上下文：完全不透明时才能直出
///
/// 面板 / 长条每帧画之前调一次。放在全局是因为绘制函数分布得太散，
/// 逐个加参数会把签名改得满城风雨。
static TEXT_MODE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 设置本帧底色；`opaque` = 该块区域完全不透明（画布 alpha 是 255）
pub fn set_text_mode(bg: (u8, u8, u8), opaque: bool) {
    let mut v = ((bg.0 as u32) << 16) | ((bg.1 as u32) << 8) | bg.2 as u32;
    if opaque {
        v |= 1 << 24;
    }
    TEXT_MODE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// 取本帧底色 + 是否可直出
pub fn text_mode() -> ((u8, u8, u8), bool) {
    let v = TEXT_MODE.load(std::sync::atomic::Ordering::Relaxed);
    (
        (((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8),
        v & (1 << 24) != 0,
    )
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

#[cfg(test)]
mod fontprobe {
    use super::*;

    /// coverage 做 gamma 提亮（模拟 DirectWrite / Chromium 的灰度抗锯齿对比度）
    fn boost(c: u8, g: f32) -> u8 {
        if g == 1.0 {
            return c;
        }
        let v = (c as f32 / 255.0).powf(g) * 255.0;
        v.round().clamp(0.0, 255.0) as u8
    }

    fn write_bmp(path: &str, w: usize, h: usize, bgra: &[u8]) {
        let mut out = Vec::new();
        let size = (w * h * 4) as u32;
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(54 + size).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(w as i32).to_le_bytes());
        out.extend_from_slice(&(-(h as i32)).to_le_bytes()); // 顶向下
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(bgra);
        std::fs::write(path, out).unwrap();
    }

    #[test]
    #[ignore]
    fn dump_variants() {
        let bg = (90u8, 96u8, 107u8);
        let fg = (255u8, 255u8, 255u8);
        let cases: &[(i32, i32, f32)] = &[
            (13, 400, 1.0),
            (13, 700, 1.0),
            (17, 400, 1.0),
            (17, 700, 1.0),
        ];
        let zoom = 4usize;
        let row_h = 26usize;
        let text = "系统工具 14";
        let w = 140usize;
        let h = row_h * cases.len();
        let mut buf = vec![0u8; w * h * 4];
        for i in 0..(w * h) {
            buf[i * 4] = bg.2;
            buf[i * 4 + 1] = bg.1;
            buf[i * 4 + 2] = bg.0;
            buf[i * 4 + 3] = 255;
        }
        for (r, &(px, weight, gamma)) in cases.iter().enumerate() {
            let t = render_text_w(text, px, weight, "微软雅黑").expect("render");
            let ox = 2;
            let oy = r * row_h + 2;
            for sy in 0..t.h as usize {
                for sx in 0..t.w as usize {
                    let c = boost(t.coverage[sy * t.w as usize + sx], gamma);
                    if c == 0 {
                        continue;
                    }
                    let (x, y) = (ox + sx, oy + sy);
                    if x >= w || y >= h {
                        continue;
                    }
                    let o = (y * w + x) * 4;
                    let a = c as u32;
                    let inv = 255 - a;
                    let mix = |s: u8, d: u8| -> u8 {
                        ((s as u32 * a + d as u32 * inv) / 255) as u8
                    };
                    buf[o] = mix(fg.2, bg.2);
                    buf[o + 1] = mix(fg.1, bg.1);
                    buf[o + 2] = mix(fg.0, bg.0);
                }
            }
        }
        // 4× 最近邻放大，方便肉眼看笔画
        let (zw, zh) = (w * zoom, h * zoom);
        let mut big = vec![0u8; zw * zh * 4];
        for y in 0..zh {
            for x in 0..zw {
                let s = ((y / zoom) * w + (x / zoom)) * 4;
                let d = (y * zw + x) * 4;
                big[d..d + 4].copy_from_slice(&buf[s..s + 4]);
            }
        }
        let path = std::env::temp_dir().join("lb-font-variants.bmp");
        write_bmp(path.to_str().unwrap(), zw, zh, &big);
        eprintln!("wrote {}", path.display());
    }

    /// 直出（ClearType）与灰度覆盖率两条路的字形必须落在同一位置 ——
    /// 差 1px 实际界面里就是「字偏了」或「字被裁了」
    #[test]
    fn 直出位图与灰度位图对齐() {
        let bg = (231u8, 233u8, 237u8);
        let fg = (28u8, 32u8, 40u8);
        let s = "系统工具 Nextcloud-";
        let Some(g) = text(s, 12, false) else {
            return; // 机器上没有这个字体就当跳过
        };
        let Some(t) = text_on(s, 12, false, bg, fg) else {
            return;
        };
        let bounds = |cov: &[u8], w: i32, h: i32| -> Option<(i32, i32, i32, i32)> {
            let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, -1i32, -1i32);
            for y in 0..h {
                for x in 0..w {
                    if cov[(y * w + x) as usize] > 8 {
                        x0 = x0.min(x);
                        y0 = y0.min(y);
                        x1 = x1.max(x);
                        y1 = y1.max(y);
                    }
                }
            }
            (x1 >= 0).then_some((x0, y0, x1, y1))
        };
        let gi = bounds(&g.bmp.coverage, g.bmp.w, g.bmp.h).expect("灰度位图有墨迹");
        let mut mask = vec![0u8; (t.w * t.h) as usize];
        for i in 0..(t.w * t.h) as usize {
            let (b, gg, r) = (t.bgra[i * 4], t.bgra[i * 4 + 1], t.bgra[i * 4 + 2]);
            mask[i] = if (b, gg, r) != (bg.2, bg.1, bg.0) {
                255
            } else {
                0
            };
        }
        let ti = bounds(&mask, t.w, t.h).expect("直出位图有墨迹");
        assert!(
            (gi.0 - ti.0).abs() <= 1,
            "左边差 {}px（灰度 {} / 直出 {}）",
            gi.0 - ti.0,
            gi.0,
            ti.0
        );
        assert!(
            (gi.1 - ti.1).abs() <= 1,
            "上边差 {}px（灰度 {} / 直出 {}）",
            gi.1 - ti.1,
            gi.1,
            ti.1
        );
    }

    /// 按指定光栅化质量渲染一张覆盖率图（黑底白字）
    ///
    /// `avg` = true 时取 RGB 三通道平均（ClearType 的亚像素覆盖率），
    /// 否则取 R 通道（灰度抗锯齿下三通道相同）。
    fn render_cov(
        text: &str,
        px: i32,
        weight: i32,
        q: FONT_QUALITY,
        avg: bool,
    ) -> (i32, i32, Vec<u8>) {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let font = CreateFontW(
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
                q,
                (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
                &windows::core::HSTRING::from("微软雅黑"),
            );
            let old_font = SelectObject(dc, HGDIOBJ(font.0));
            let wchars = crate::util::wide(text);
            let mut sz = SIZE::default();
            let _ = GetTextExtentPoint32W(dc, &wchars, &mut sz);
            let w = sz.cx + px;
            let h = sz.cy + px;
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [Default::default()],
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbmp =
                CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old_bmp = SelectObject(dc, HGDIOBJ(hbmp.0));
            std::ptr::write_bytes(bits as *mut u8, 0, (w * h * 4) as usize);
            let _ = SetTextColor(dc, COLORREF(0x00FF_FFFF));
            // ClearType 需要不透明底色：黑底白字是它的最佳工况
            let _ = SetBkColor(dc, COLORREF(0x0000_0000));
            let _ = SetBkMode(dc, OPAQUE);
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
            let mut cov = vec![0u8; (w * h) as usize];
            for i in 0..(w * h) as usize {
                let (b, g, r) = (raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2]);
                cov[i] = if avg {
                    ((r as u32 + g as u32 + b as u32) / 3) as u8
                } else {
                    r
                };
            }
            SelectObject(dc, old_bmp);
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            SelectObject(dc, old_font);
            let _ = DeleteObject(HGDIOBJ(font.0));
            let _ = ReleaseDC(None, screen);
            (w, h, cov)
        }
    }

    /// 深底 / 浅底各画一遍，比较灰度抗锯齿与 ClearType 的清晰度
    #[test]
    #[ignore]
    fn dump_quality() {
        // (标签, 质量, 是否取三通道平均, gamma)
        let variants: &[(&str, FONT_QUALITY, bool, f32)] = &[
            ("AA g0.82 当前", ANTIALIASED_QUALITY, false, 0.82),
            ("AA g1.0", ANTIALIASED_QUALITY, false, 1.0),
            ("CT avg g1.0", CLEARTYPE_QUALITY, true, 1.0),
            ("CT avg g0.82", CLEARTYPE_QUALITY, true, 0.82),
            ("CT avg g1.2", CLEARTYPE_QUALITY, true, 1.2),
        ];
        // (底色, 字色)
        let bgs: &[((u8, u8, u8), (u8, u8, u8))] = &[
            ((90, 96, 107), (255, 255, 255)),
            ((231, 233, 237), (28, 32, 40)),
        ];
        let text = "系统工具 Nextcloud-";
        let label_px = 12;
        let zoom = 4usize;
        let row_h = 24usize;
        let w = 200usize;
        let h = row_h * variants.len() * bgs.len();

        let mut buf = vec![0u8; w * h * 4];
        for (bi, &(bg, fg)) in bgs.iter().enumerate() {
            for (vi, &(_tag, q, avg, gamma)) in variants.iter().enumerate() {
                let (tw, th, cov) = render_cov(text, label_px, 400, q, avg);
                let oy = (bi * variants.len() + vi) * row_h + 2;
                for sy in 0..th as usize {
                    for sx in 0..tw as usize {
                        let c = boost(cov[sy * tw as usize + sx], gamma);
                        if c == 0 {
                            continue;
                        }
                        let (x, y) = (2 + sx, oy + sy);
                        if x >= w || y >= h {
                            continue;
                        }
                        let o = (y * w + x) * 4;
                        let a = c as u32;
                        let inv = 255 - a;
                        let mix = |s: u8, d: u8| -> u8 {
                            ((s as u32 * a + d as u32 * inv) / 255) as u8
                        };
                        buf[o] = mix(fg.2, bg.2);
                        buf[o + 1] = mix(fg.1, bg.1);
                        buf[o + 2] = mix(fg.0, bg.0);
                        buf[o + 3] = 255;
                    }
                }
            }
        }
        let (zw, zh) = (w * zoom, h * zoom);
        let mut big = vec![0u8; zw * zh * 4];
        for y in 0..zh {
            for x in 0..zw {
                let s = ((y / zoom) * w + (x / zoom)) * 4;
                let d = (y * zw + x) * 4;
                big[d..d + 4].copy_from_slice(&buf[s..s + 4]);
            }
        }
        let path = std::env::temp_dir().join("lb-quality.bmp");
        write_bmp(path.to_str().unwrap(), zw, zh, &big);
        eprintln!("wrote {}", path.display());
    }

    /// 放大 8× 的两行对比图（AA vs ClearType），深底 / 浅底各一张，并打印锐利度指标
    #[test]
    #[ignore]
    fn dump_quality2() {
        let variants: &[(&str, FONT_QUALITY, bool, f32)] = &[
            ("AA  g0.82 当前", ANTIALIASED_QUALITY, false, 0.82),
            ("AA  g1.00", ANTIALIASED_QUALITY, false, 1.0),
            ("CT  g1.00", CLEARTYPE_QUALITY, true, 1.0),
            ("CT  g1.30", CLEARTYPE_QUALITY, true, 1.3),
        ];
        let bgs: &[((u8, u8, u8), (u8, u8, u8))] = &[
            ((90, 96, 107), (255, 255, 255)),
            ((231, 233, 237), (28, 32, 40)),
        ];
        let text = "系统工具 Nextcloud-";
        let font_px = 12;
        let zoom = 8usize;
        let gap = 6usize;

        let mut imgs = Vec::new();
        for &(tag, q, avg, g) in variants {
            let (w, h, cov) = render_cov(text, font_px, 400, q, avg);
            // 锐利度：水平相邻像素灰度差之和 / 面积（越大越锐）
            let mut grad = 0u64;
            for y in 0..h as usize {
                for x in 1..w as usize {
                    let a = cov[y * w as usize + x] as i32;
                    let b = cov[y * w as usize + x - 1] as i32;
                    grad += (a - b).unsigned_abs() as u64;
                }
            }
            let solid = cov.iter().filter(|&&c| c >= 200).count();
            eprintln!(
                "{tag}: 实心像素 {solid} | 边缘梯度 {}",
                grad / (w as u64 * h as u64)
            );
            imgs.push((w as usize, h as usize, cov, g));
        }
        let iw = imgs.iter().map(|x| x.0).max().unwrap();
        let ih = imgs.iter().map(|x| x.1).max().unwrap();
        let w = iw + 8;
        let h = (ih + gap) * imgs.len();

        for (bi, &(bg, fg)) in bgs.iter().enumerate() {
            let mut buf = vec![0u8; w * h * 4];
            for i in 0..(w * h) {
                buf[i * 4] = bg.2;
                buf[i * 4 + 1] = bg.1;
                buf[i * 4 + 2] = bg.0;
                buf[i * 4 + 3] = 255;
            }
            for (vi, (tw, th, cov, g)) in imgs.iter().enumerate() {
                let oy = vi * (ih + gap) + 2;
                for sy in 0..*th {
                    for sx in 0..*tw {
                        let c = boost(cov[sy * tw + sx], *g);
                        if c == 0 {
                            continue;
                        }
                        let (x, y) = (2 + sx, oy + sy);
                        if x >= w || y >= h {
                            continue;
                        }
                        let o = y * w + x;
                        let a = c as u32;
                        let inv = 255 - a;
                        let mix = |s: u8, d: u8| -> u8 {
                            ((s as u32 * a + d as u32 * inv) / 255) as u8
                        };
                        buf[o * 4] = mix(fg.2, bg.2);
                        buf[o * 4 + 1] = mix(fg.1, bg.1);
                        buf[o * 4 + 2] = mix(fg.0, bg.0);
                    }
                }
            }
            let (zw, zh) = (w * zoom, h * zoom);
            let mut big = vec![0u8; zw * zh * 4];
            for y in 0..zh {
                for x in 0..zw {
                    let s = ((y / zoom) * w + (x / zoom)) * 4;
                    let d = (y * zw + x) * 4;
                    big[d..d + 4].copy_from_slice(&buf[s..s + 4]);
                }
            }
            let name = if bi == 0 { "lb-q-dark.bmp" } else { "lb-q-light.bmp" };
            let path = std::env::temp_dir().join(name);
            write_bmp(path.to_str().unwrap(), zw, zh, &big);
            eprintln!("wrote {}", path.display());
        }
    }

    /// 返回原始 BGRA 像素（不折叠），供下面几种覆盖率提取方式对比
    fn render_raw(text: &str, px: i32, weight: i32, q: FONT_QUALITY) -> (i32, i32, Vec<u8>) {
        render_raw_c(text, px, weight, q, (0, 0, 0), (255, 255, 255))
    }

    /// 指定底色 / 字色渲染，返回不透明 BGRA（ClearType 直出用这个）
    fn render_raw_c(
        text: &str,
        px: i32,
        weight: i32,
        q: FONT_QUALITY,
        bg: (u8, u8, u8),
        fg: (u8, u8, u8),
    ) -> (i32, i32, Vec<u8>) {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let font = CreateFontW(
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
                q,
                (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
                &windows::core::HSTRING::from("微软雅黑"),
            );
            let old_font = SelectObject(dc, HGDIOBJ(font.0));
            let wchars = crate::util::wide(text);
            let mut sz = SIZE::default();
            let _ = GetTextExtentPoint32W(dc, &wchars, &mut sz);
            let w = sz.cx + px;
            let h = sz.cy + px;
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [Default::default()],
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbmp =
                CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old_bmp = SelectObject(dc, HGDIOBJ(hbmp.0));
            // 底色铺满：ClearType 要的"已知不透明背景"
            let px_buf = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
            for i in 0..(w * h) as usize {
                px_buf[i * 4] = bg.2;
                px_buf[i * 4 + 1] = bg.1;
                px_buf[i * 4 + 2] = bg.0;
                px_buf[i * 4 + 3] = 255;
            }
            let _ = SetTextColor(dc, COLORREF((fg.0 as u32) | ((fg.1 as u32) << 8) | ((fg.2 as u32) << 16)));
            let _ = SetBkColor(dc, COLORREF((bg.0 as u32) | ((bg.1 as u32) << 8) | ((bg.2 as u32) << 16)));
            let _ = SetBkMode(dc, OPAQUE);
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
            let raw = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
            SelectObject(dc, old_bmp);
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            SelectObject(dc, old_font);
            let _ = DeleteObject(HGDIOBJ(font.0));
            let _ = ReleaseDC(None, screen);
            (w, h, raw)
        }
    }

    fn fold(raw: &[u8], n: usize, mode: u8) -> Vec<u8> {
        let mut cov = vec![0u8; n];
        for i in 0..n {
            let (b, g, r) = (raw[i * 4] as u32, raw[i * 4 + 1] as u32, raw[i * 4 + 2] as u32);
            cov[i] = match mode {
                0 => r as u8,                                  // 灰度 AA：单通道
                1 => ((r + g + b) / 3) as u8,                   // 三通道平均
                2 => r.max(g).max(b) as u8,                      // 取最强子像素
                _ => (255 - (255 - r) * (255 - g) * (255 - b) / 65025) as u8, // 并集
            };
        }
        cov
    }

    /// 五种折叠 / 光栅化方式的最终观感对比（深、浅底色各一张）
    #[test]
    #[ignore]
    fn dump_quality3() {
        let text = "系统工具 Nextcloud-";
        let font_px = 12;
        let zoom = 8usize;
        let gap = 6usize;
        let bgs: &[((u8, u8, u8), (u8, u8, u8))] = &[
            ((90, 96, 107), (255, 255, 255)),
            ((231, 233, 237), (28, 32, 40)),
        ];

        let (aw, ah, araw) = render_raw(text, font_px, 400, ANTIALIASED_QUALITY);
        let (cw, ch, craw) = render_raw(text, font_px, 400, CLEARTYPE_QUALITY);
        let n_a = (aw * ah) as usize;
        let n_c = (cw * ch) as usize;

        // (标签, 宽, 高, 覆盖率, gamma)
        let rows: Vec<(&str, i32, i32, Vec<u8>, f32)> = vec![
            ("AA  单通道 g0.82 当前", aw, ah, fold(&araw, n_a, 0), 0.82),
            ("AA  单通道 g1.30", aw, ah, fold(&araw, n_a, 0), 1.3),
            ("CT  平均 g1.00", cw, ch, fold(&craw, n_c, 1), 1.0),
            ("CT  取最大 g1.00", cw, ch, fold(&craw, n_c, 2), 1.0),
            ("CT  并集 g1.00", cw, ch, fold(&craw, n_c, 3), 1.0),
        ];
        for (tag, w, h, cov, _g) in &rows {
            let mut grad = 0u64;
            for y in 0..*h as usize {
                for x in 1..*w as usize {
                    let a = cov[y * *w as usize + x] as i32;
                    let b = cov[y * *w as usize + x - 1] as i32;
                    grad += (a - b).unsigned_abs() as u64;
                }
            }
            let solid = cov.iter().filter(|&&c| c >= 200).count();
            eprintln!(
                "{tag}: 实心 {solid} | 梯度 {} | 均灰 {}",
                grad / (*w as u64 * *h as u64),
                cov.iter().map(|&c| c as u32).sum::<u32>() / (*w as u32 * *h as u32)
            );
        }

        let iw = rows.iter().map(|r| r.1 as usize).max().unwrap();
        let ih = rows.iter().map(|r| r.2 as usize).max().unwrap();
        let w = iw + 8;
        let h = (ih + gap) * rows.len();
        for (bi, &(bg, fg)) in bgs.iter().enumerate() {
            let mut buf = vec![0u8; w * h * 4];
            for i in 0..(w * h) {
                buf[i * 4] = bg.2;
                buf[i * 4 + 1] = bg.1;
                buf[i * 4 + 2] = bg.0;
                buf[i * 4 + 3] = 255;
            }
            for (vi, (_, tw, th, cov, g)) in rows.iter().enumerate() {
                let oy = vi * (ih + gap) + 2;
                for sy in 0..*th as usize {
                    for sx in 0..*tw as usize {
                        let c = boost(cov[sy * *tw as usize + sx], *g);
                        if c == 0 {
                            continue;
                        }
                        let (x, y) = (2 + sx, oy + sy);
                        if x >= w || y >= h {
                            continue;
                        }
                        let o = y * w + x;
                        let a = c as u32;
                        let inv = 255 - a;
                        let mix = |s: u8, d: u8| -> u8 {
                            ((s as u32 * a + d as u32 * inv) / 255) as u8
                        };
                        buf[o * 4] = mix(fg.2, bg.2);
                        buf[o * 4 + 1] = mix(fg.1, bg.1);
                        buf[o * 4 + 2] = mix(fg.0, bg.0);
                    }
                }
            }
            let (zw, zh) = (w * zoom, h * zoom);
            let mut big = vec![0u8; zw * zh * 4];
            for y in 0..zh {
                for x in 0..zw {
                    let s = ((y / zoom) * w + (x / zoom)) * 4;
                    let d = (y * zw + x) * 4;
                    big[d..d + 4].copy_from_slice(&buf[s..s + 4]);
                }
            }
            let name = if bi == 0 { "lb-q3-dark.bmp" } else { "lb-q3-light.bmp" };
            let path = std::env::temp_dir().join(name);
            write_bmp(path.to_str().unwrap(), zw, zh, &big);
            eprintln!("wrote {}", path.display());
        }
    }

    /// 当前做法（灰度 AA + alpha 合成）vs ClearType 直出（在已知底色上给最终像素）
    #[test]
    #[ignore]
    fn dump_quality4() {
        let text = "系统工具 Nextcloud-";
        let font_px = 12;
        let zoom = 8usize;
        let gap = 6usize;
        let bgs: &[((u8, u8, u8), (u8, u8, u8))] = &[
            ((90, 96, 107), (255, 255, 255)),
            ((231, 233, 237), (28, 32, 40)),
        ];
        let (aw, ah, araw) = render_raw(text, font_px, 400, ANTIALIASED_QUALITY);
        let acov = fold(&araw, (aw * ah) as usize, 0);

        for (bi, &(bg, fg)) in bgs.iter().enumerate() {
            let (cw, ch, craw) = render_raw_c(text, font_px, 400, CLEARTYPE_QUALITY, bg, fg);
            let w = aw.max(cw) as usize + 8;
            let h = (ah.max(ch) as usize + gap) * 2;
            let mut buf = vec![0u8; w * h * 4];
            for i in 0..(w * h) {
                buf[i * 4] = bg.2;
                buf[i * 4 + 1] = bg.1;
                buf[i * 4 + 2] = bg.0;
                buf[i * 4 + 3] = 255;
            }
            // 行 1：灰度 AA 覆盖率 + alpha 合成（现行做法）
            for sy in 0..ah as usize {
                for sx in 0..aw as usize {
                    let c = boost(acov[sy * aw as usize + sx], 0.82);
                    if c == 0 {
                        continue;
                    }
                    let (x, y) = (2 + sx, 2 + sy);
                    let o = (y * w + x) * 4;
                    let a = c as u32;
                    let inv = 255 - a;
                    let mix =
                        |s: u8, d: u8| -> u8 { ((s as u32 * a + d as u32 * inv) / 255) as u8 };
                    buf[o] = mix(fg.2, bg.2);
                    buf[o + 1] = mix(fg.1, bg.1);
                    buf[o + 2] = mix(fg.0, bg.0);
                }
            }
            // 行 2：ClearType 直出（GDI 已经给了最终像素，直接覆盖）
            let oy = ah as usize + gap + 2;
            for sy in 0..ch as usize {
                for sx in 0..cw as usize {
                    let s = (sy * cw as usize + sx) * 4;
                    let (x, y) = (2 + sx, oy + sy);
                    if x >= w || y >= h {
                        continue;
                    }
                    let o = (y * w + x) * 4;
                    buf[o] = craw[s];
                    buf[o + 1] = craw[s + 1];
                    buf[o + 2] = craw[s + 2];
                }
            }
            let (zw, zh) = (w * zoom, h * zoom);
            let mut big = vec![0u8; zw * zh * 4];
            for y in 0..zh {
                for x in 0..zw {
                    let s = ((y / zoom) * w + (x / zoom)) * 4;
                    let d = (y * zw + x) * 4;
                    big[d..d + 4].copy_from_slice(&buf[s..s + 4]);
                }
            }
            let name = if bi == 0 { "lb-q4-dark.bmp" } else { "lb-q4-light.bmp" };
            let path = std::env::temp_dir().join(name);
            write_bmp(path.to_str().unwrap(), zw, zh, &big);
            eprintln!("wrote {}", path.display());
        }
    }
}
