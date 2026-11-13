//! 普通窗口的绘制工具：GDI 画圆角矩形、开关、色块、文字。
//!
//! 为什么设置窗口不是分层窗口（而是普通窗口）：
//! 分层窗口**不能当子控件的父窗口** —— `UpdateLayeredWindow` 会把整个窗口
//! 的内容换成位图，挂在它下面的 EDIT 会被整个盖掉（实测：底板画出来了，
//! 输入框一个像素都看不见）。而盒子名要支持中文输入，就必须用真正的
//! EDIT（自己处理 IME 太麻烦）。所以设置窗口走普通窗口这条路：
//!  · 圆角：Win11 用 DWM 的 `DWMWA_WINDOW_CORNER_PREFERENCE`（系统级圆角，
//!    抗锯齿完美），Win10 用 `SetWindowRgn` 裁一个圆角区域；
//!  · 阴影：DWM 自带；
//!  · 文字：GDI 的 ClearType，比分层窗口里那套灰度抗锯齿更清晰。

use windows::Win32::Foundation::{COLORREF, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, GetStockObject, GetTextExtentPoint32W,
    GetTextMetricsW, MoveToEx, Polygon, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HGDIOBJ, NULL_BRUSH, OUT_DEFAULT_PRECIS, TEXTMETRICW,
    TRANSPARENT,
};
use windows::Win32::Graphics::Gdi::{CreatePen, PS_SOLID};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, GetDC, ReleaseDC, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, SRCCOPY,
};

/// 离屏位图（双缓冲）
///
/// 直接往窗口 DC 上画，Windows 是「先擦背景、再逐块画上去」的 ——
/// 鼠标一动就重画一遍，用户能看见中间态，那就是闪烁。
/// 改成先画在这张离屏位图上，画完一次性贴到窗口，就只有完整的一帧。
pub struct Buf {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
}

impl Buf {
    /// 建一张 w×h 的离屏位图（建不出来就返回 None，调用方退回直接画）
    pub fn new(w: i32, h: i32) -> Option<Buf> {
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
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
            let hbmp = match CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) => b,
                Err(_) => {
                    let _ = ReleaseDC(None, screen);
                    let _ = DeleteDC(mem);
                    return None;
                }
            };
            let old = SelectObject(mem, HGDIOBJ(hbmp.0));
            let _ = ReleaseDC(None, screen);
            Some(Buf { dc: mem, bmp: hbmp, old, w, h })
        }
    }

    /// 在这张离屏位图上画
    pub fn hdc(&self) -> HDC {
        self.dc
    }

    /// 一次性贴到目标 DC
    pub fn blit_to(&self, dst: HDC) {
        unsafe {
            let _ = BitBlt(dst, 0, 0, self.w, self.h, Some(self.dc), 0, 0, SRCCOPY);
        }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        unsafe {
            if !self.bmp.0.is_null() {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

/// 文字对齐方式
#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// RGB → COLORREF（注意是 0x00BBGGRR，和常见的 0xRRGGBB 正好相反）
pub fn colorref(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// 绘制上下文：包着 DC 和当前字体
pub struct Gfx {
    pub hdc: HDC,
    font: windows::Win32::Graphics::Gdi::HFONT,
    old: HGDIOBJ,
    px: i32,
    bold: bool,
    family: String,
}

impl Gfx {
    pub fn new(hdc: HDC) -> Gfx {
        let font = create_font(14, false);
        unsafe {
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            Gfx {
                hdc,
                font,
                old,
                px: 14,
                bold: false,
                family: "Microsoft YaHei UI".into(),
            }
        }
    }

    /// 换字号（粗体可选），字体保持默认
    pub fn font(&mut self, px: i32, bold: bool) {
        self.font_named(px, bold, "Microsoft YaHei UI");
    }

    /// 换字号 / 粗体 / 字体名。外观页的字体预览要用它按用户选的字体画样例。
    pub fn font_named(&mut self, px: i32, bold: bool, family: &str) {
        if self.px == px && self.bold == bold && self.family == family {
            return;
        }
        unsafe {
            let _ = SelectObject(self.hdc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.font.0));
            let font = create_font_named(px, bold, family);
            self.old = SelectObject(self.hdc, HGDIOBJ(font.0));
            self.font = font;
            self.px = px;
            self.bold = bold;
            self.family = family.to_string();
        }
    }

    /// 实心矩形
    pub fn fill_rect(&self, x: i32, y: i32, w: i32, h: i32, c: (u8, u8, u8)) {
        if w <= 0 || h <= 0 {
            return;
        }
        unsafe {
            let brush = CreateSolidBrush(colorref(c.0, c.1, c.2));
            let old = SelectObject(self.hdc, HGDIOBJ(brush.0));
            let _ = windows::Win32::Graphics::Gdi::FillRect(
                self.hdc,
                &RECT {
                    left: x,
                    top: y,
                    right: x + w,
                    bottom: y + h,
                },
                brush,
            );
            SelectObject(self.hdc, old);
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }
    }

    /// 圆角矩形填充
    pub fn fill_round(&self, x: i32, y: i32, w: i32, h: i32, r: i32, c: (u8, u8, u8)) {
        if w <= 0 || h <= 0 {
            return;
        }
        let r = r.max(0).min(w.min(h) / 2);
        unsafe {
            let brush = CreateSolidBrush(colorref(c.0, c.1, c.2));
            let pen = CreatePen(PS_SOLID, 1, colorref(c.0, c.1, c.2));
            let ob = SelectObject(self.hdc, HGDIOBJ(brush.0));
            let op = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let _ = windows::Win32::Graphics::Gdi::RoundRect(
                self.hdc,
                x,
                y,
                x + w,
                y + h,
                r * 2,
                r * 2,
            );
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, op);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            let _ = DeleteObject(HGDIOBJ(pen.0));
        }
    }

    /// 圆角矩形描边
    pub fn stroke_round(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        r: i32,
        c: (u8, u8, u8),
        width: i32,
    ) {
        if w <= 0 || h <= 0 {
            return;
        }
        let r = r.max(0).min(w.min(h) / 2);
        unsafe {
            let pen = CreatePen(PS_SOLID, width, colorref(c.0, c.1, c.2));
            // RoundRect 是「描边 + 填充」两件事一起做的，填充用的是**当前画刷**。
            // 不换成空刷的话，它会用默认的白刷把刚画好的色块整个盖成白色。
            let hollow = GetStockObject(NULL_BRUSH);
            let old_pen = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let old_brush = SelectObject(self.hdc, hollow);
            let _ = windows::Win32::Graphics::Gdi::RoundRect(
                self.hdc,
                x,
                y,
                x + w,
                y + h,
                r * 2,
                r * 2,
            );
            SelectObject(self.hdc, old_brush);
            SelectObject(self.hdc, old_pen);
            let _ = DeleteObject(HGDIOBJ(pen.0));
        }
    }

    /// 一条横线
    pub fn hline(&self, x1: i32, x2: i32, y: i32, c: (u8, u8, u8)) {
        unsafe {
            let pen = CreatePen(PS_SOLID, 1, colorref(c.0, c.1, c.2));
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let _ = MoveToEx(self.hdc, x1, y, None);
            let _ = windows::Win32::Graphics::Gdi::LineTo(self.hdc, x2, y);
            SelectObject(self.hdc, old);
            let _ = DeleteObject(HGDIOBJ(pen.0));
        }
    }

    /// 实心上下箭头：三角头 + 箭杆，形状对齐「⬆ / ⬇」的标准比例。
    ///
    /// 不依赖 unicode 字形（`⬆/⬇` 在无 Segoe Symbol 的机器上会回退成一像素细线，
    /// `⇧/⇩` 笔画太细浅底看不清），也不画空心轮廓（笔画粗细受限于 pen 宽度）。
    /// `w` / `h` 是外接框宽高，`cx` / `cy` 是中心，`c` 由调用方给（蓝色调），
    /// `lw` 是箭杆**整宽**；`lw <= 0` 时只画一个等腰三角（外观页的下拉指示用）。
    ///
    /// 注意：GDI 的 `Polygon` 用「当前画刷」填充、「当前画笔」描边。早先这里选入
    /// 实心画刷后又换成 `NULL_BRUSH`，结果只剩默认黑笔的一圈轮廓 —— 蓝色实心
    /// 箭头变成了黑色空心箭头。这里画刷、画笔都用同一个颜色，画完逐个恢复。
    pub fn arrow(
        &self,
        cx: i32,
        cy: i32,
        w: i32,
        h: i32,
        up: bool,
        c: (u8, u8, u8),
        lw: i32,
    ) {
        if w < 6 || h < 6 {
            return;
        }
        let top = cy - h / 2;
        let bottom = top + h;
        let half = (w / 2).max(2); // 三角头底边半宽
        let stem_half = if lw > 0 { (lw / 2).max(1) } else { 0 };
        // 有箭杆时头占 44%（标准 ⬆ 比例：宽三角 + 略粗的长杆）；
        // 纯三角时占满整个外接框，免得下拉小箭头被压成一条横线。
        let head_h = if stem_half > 0 {
            (h * 44 / 100).max(5)
        } else {
            h
        };
        let (tip_y, shoulder_y, tail_y) = if up {
            (top, top + head_h, bottom)
        } else {
            (bottom, bottom - head_h, top)
        };
        let pts: Vec<(i32, i32)> = if stem_half > 0 {
            vec![
                (cx, tip_y),
                (cx - half, shoulder_y),
                (cx - stem_half, shoulder_y),
                (cx - stem_half, tail_y),
                (cx + stem_half, tail_y),
                (cx + stem_half, shoulder_y),
                (cx + half, shoulder_y),
            ]
        } else {
            vec![(cx, tip_y), (cx - half, shoulder_y), (cx + half, shoulder_y)]
        };
        unsafe {
            let col = colorref(c.0, c.1, c.2);
            let brush = CreateSolidBrush(col);
            // 同色 1px 描边：把多边形边缘的锯齿缺口填平，看起来更饱满
            let pen = CreatePen(PS_SOLID, 1, col);
            let old_brush = SelectObject(self.hdc, HGDIOBJ(brush.0));
            let old_pen = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let points: Vec<POINT> = pts
                .iter()
                .map(|&(x, y)| POINT { x, y })
                .collect();
            let _ = Polygon(self.hdc, &points);
            SelectObject(self.hdc, old_brush);
            SelectObject(self.hdc, old_pen);
            let _ = DeleteObject(HGDIOBJ(pen.0));
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }
    }

    /// 画一段文字
    pub fn text(&self, x: i32, y: i32, w: i32, h: i32, s: &str, c: (u8, u8, u8), align: Align) {
        if s.is_empty() || w <= 0 {
            return;
        }
        unsafe {
            let _ = SetBkMode(self.hdc, TRANSPARENT);
            let _ = SetTextColor(self.hdc, colorref(c.0, c.1, c.2));
            let mut buf: Vec<u16> = s.encode_utf16().collect();
            buf.push(0);
            let mut rect = RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            let flags = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX
                | match align {
                    Align::Left => DT_LEFT,
                    Align::Center => DT_CENTER,
                    Align::Right => DT_RIGHT,
                };
            let _ = DrawTextW(self.hdc, &mut buf, &mut rect, flags);
        }
    }

    /// 量一段文字的宽
    pub fn text_w(&self, s: &str) -> i32 {
        unsafe {
            let buf: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
            let mut sz = windows::Win32::Foundation::SIZE::default();
            let _ = GetTextExtentPoint32W(self.hdc, &buf, &mut sz);
            sz.cx
        }
    }

    /// 画一个开关：轨道 + 圆钮。`on` 为真时是蓝色。
    pub fn switch(&self, x: i32, y: i32, w: i32, h: i32, on: bool) {
        let track = if on { (47, 122, 245) } else { (120, 124, 132) };
        self.fill_round(x, y, w, h, h / 2, track);
        let knob = h - 6;
        let kx = if on { x + w - knob - 3 } else { x + 3 };
        self.fill_round(kx, y + 3, knob, knob, knob / 2, (255, 255, 255));
    }

    /// 画一个颜色小块（圆角 + 细边，颜色浅的时候也要看得见轮廓）
    pub fn swatch(&self, x: i32, y: i32, w: i32, h: i32, c: (u8, u8, u8)) {
        self.fill_round(x, y, w, h, 4, c);
        let edge = crate::theme::mix(c, (0, 0, 0), 0.25);
        self.stroke_round(x, y, w, h, 4, edge, 1);
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.font.0));
        }
    }
}

/// 建一支字体。`px` 是字符高度（正数）。
///
/// 这里用 ClearType（亚像素）抗锯齿：ui 模块只服务于设置页 / 取色器这类
/// **不透明白底窗口**，文字直接画在 DIB 上再 BitBlt 到窗口，子像素颜色不会被
/// 缩放或混合，看起来和系统原生控件一样清晰。
/// 长条和弹出面板是分层窗口（逐像素 alpha），在那边用 ClearType 会在边缘
/// 留下彩色描边，所以那边继续走灰度抗锯齿（`render.rs`）。
pub fn create_font(px: i32, bold: bool) -> windows::Win32::Graphics::Gdi::HFONT {
    create_font_named(px, bold, "Microsoft YaHei UI")
}

/// 指定字体名的版本：字体名不存在时 GDI 会自己回退，不会失败。
pub fn create_font_named(px: i32, bold: bool, family: &str) -> windows::Win32::Graphics::Gdi::HFONT {
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            if bold { 700 } else { 400 },
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
            &windows::core::HSTRING::from(family),
        )
    }
}

/// 这支字体的实际行高（px）。单行 EDIT 的文本是顶对齐的，
/// 想让它看上去居中就只能把控件高度压到行高附近，再整体垂直居中摆放。
pub fn font_height(font: windows::Win32::Graphics::Gdi::HFONT) -> i32 {
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let mut tm = TEXTMETRICW::default();
        let ok = GetTextMetricsW(dc, &mut tm).as_bool();
        SelectObject(dc, old);
        let _ = ReleaseDC(None, dc);
        if ok {
            tm.tmHeight.max(1)
        } else {
            0
        }
    }
}

/// 文字太长时截断并加省略号（按像素宽度）
pub fn ellipsize(s: &str, max_w: i32, measure: impl Fn(&str) -> i32) -> String {
    if s.is_empty() || max_w <= 0 {
        return String::new();
    }
    if measure(s) <= max_w {
        return s.to_string();
    }
    let mut out: String = s.chars().collect();
    while !out.is_empty() && measure(&format!("{out}…")) > max_w {
        out.pop();
    }
    if out.is_empty() {
        "…".into()
    } else {
        format!("{out}…")
    }
}
