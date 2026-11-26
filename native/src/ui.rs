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
    CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, GetCharWidth32W, GetStockObject,
    GetTextExtentPoint32W, GetTextMetricsW, MoveToEx, Polygon, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER,
    DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HGDIOBJ, NULL_BRUSH,
    OUT_DEFAULT_PRECIS, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::Graphics::Gdi::{CreatePen, PS_SOLID};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, GdiFlush, GetDC, ReleaseDC, BITMAPINFO,
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
    /// 位图像素起点（自上而下，32 位 BGRA）。抗锯齿图形直接写这块内存
    bits: *mut u8,
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
            Some(Buf {
                dc: mem,
                bmp: hbmp,
                old,
                w,
                h,
                bits: bits as *mut u8,
            })
        }
    }

    /// 在这张离屏位图上画
    pub fn hdc(&self) -> HDC {
        self.dc
    }

    /// 像素直写通道（抗锯齿图形用）。位图建不出来时这里是空指针，调用方会退回 GDI
    pub fn surface(&self) -> Surface {
        Surface {
            ptr: self.bits,
            w: self.w,
            h: self.h,
        }
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

/// 可直接写像素的目标：离屏位图的一块 32 位 BGRA 内存（自上而下）
///
/// GDI 的 `RoundRect` 只能画出整数像素的直角台阶 —— 开关那种「胶囊 + 圆钮」
/// 全是曲线，台阶一眼就能看出来（用户说的「颗粒感」）。要平滑就得自己算
/// 每个像素的覆盖率再混色，这条通道就是给它用的。
#[derive(Clone, Copy)]
pub struct Surface {
    ptr: *mut u8,
    w: i32,
    h: i32,
}

impl Surface {
    fn is_valid(&self) -> bool {
        !self.ptr.is_null() && self.w > 0 && self.h > 0
    }

    /// 把一个颜色按覆盖率 `cov`(0..1) 混到 (x, y)
    ///
    /// 先 `GdiFlush`：GDI 有命令缓冲，不刷的话刚用 GDI 画完的像素可能还没落到
    /// 这块内存上，读出来的底色是旧值，圆角边缘就会混错颜色。
    fn blend(&self, x: i32, y: i32, color: (u8, u8, u8), cov: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let o = ((y * self.w + x) * 4) as usize;
        // 位图是 B, G, R, A 顺序，A 由 BitBlt 忽略，不动它
        let src = [color.2, color.1, color.0];
        unsafe {
            let p = self.ptr.add(o);
            for (i, sc) in src.iter().enumerate() {
                let d = *p.add(i) as f32;
                let v = d + (*sc as f32 - d) * cov;
                *p.add(i) = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    fn flush(&self) {
        unsafe {
            let _ = GdiFlush();
        }
    }
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
    /// 像素直写通道（离屏位图才有；直接画窗口时为 None）
    surface: Option<Surface>,
    /// 视口纵向偏移：外观 / 功能页用 `SetViewportOrgEx` 整体上移，
    /// 逐像素写入不认视口，得自己补上这个偏移
    vp_y: i32,
    /// 内容区的上下边界（设备像素）。逐像素写入也不认 GDI 裁剪区，
    /// 整页滚动时开关会被画到页脚上 —— 这里手动照着裁
    clip_y: Option<(i32, i32)>,
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
                family: UI_FONT.into(),
                surface: None,
                vp_y: 0,
                clip_y: None,
            }
        }
    }

    /// 挂上像素直写通道（`Buf::surface()`）。没挂或者位图建失败时，
    /// 抗锯齿图形会自动退回 GDI 的直角版本。
    pub fn attach(&mut self, s: Surface) {
        self.surface = if s.is_valid() { Some(s) } else { None };
    }

    /// 同步 GDI 视口的纵向偏移（换了 `SetViewportOrgEx` 就要跟着改）
    pub fn viewport_y(&mut self, dy: i32) {
        self.vp_y = dy;
    }

    /// 同步 GDI 裁剪区的上下边界（`IntersectClipRect` 之后调用；解除时传 None）
    pub fn clip_y(&mut self, band: Option<(i32, i32)>) {
        self.clip_y = band;
    }

    /// 换字号（粗体可选），字体保持默认
    pub fn font(&mut self, px: i32, bold: bool) {
        self.font_named(px, bold, UI_FONT);
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

    /// 抗锯齿圆角矩形（逐像素覆盖率，边缘平滑）
    ///
    /// 和 `fill_round` 的关系：那个走 GDI `RoundRect`，只有整数像素台阶；
    /// 曲线多的形状（开关的胶囊轨道、圆钮）用它就能看出锯齿。这里在离屏
    /// 位图上按有符号距离场算覆盖率混色，效果等同 `render.rs` 里分层窗口
    /// 那套画法。
    pub fn fill_round_aa(&self, x: i32, y: i32, w: i32, h: i32, radius: i32, c: (u8, u8, u8)) {
        let Some(s) = self.surface else {
            self.fill_round(x, y, w, h, radius, c);
            return;
        };
        if w <= 0 || h <= 0 {
            return;
        }
        let y = y + self.vp_y;
        let hw = w as f32 * 0.5;
        let hh = h as f32 * 0.5;
        let cx = x as f32 + hw;
        let cy = y as f32 + hh;
        let rad = (radius as f32).clamp(0.0, hw.min(hh));
        s.flush();
        let x0 = x.max(0);
        let mut y0 = y.max(0);
        let x1 = (x + w).min(s.w);
        let mut y1 = (y + h).min(s.h);
        if let Some((c0, c1)) = self.clip_y {
            y0 = y0.max(c0);
            y1 = y1.min(c1);
        }
        // 只有四个角要算覆盖率：中间那条「十字」区域（横带 + 竖带）是满覆盖，
        // 直接整块混上去。设置页一帧要画十几个圆角块，全算 SDF 会白花一倍时间。
        let ri = rad.ceil() as i32;
        let (ix0, ix1) = (x + ri, x + w - ri);
        let (iy0, iy1) = (y + ri, y + h - ri);
        for py in y0..y1 {
            let band_h = py >= iy0 && py < iy1;
            for px in x0..x1 {
                if band_h || (px >= ix0 && px < ix1) {
                    s.blend(px, py, c, 1.0);
                    continue;
                }
                let d =
                    crate::render::sd_round_rect(px as f32 + 0.5, py as f32 + 0.5, cx, cy, hw, hh, rad);
                // 半像素过渡带：内部 1、外部 0，正好压在边缘上
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                s.blend(px, py, c, cov);
            }
        }
    }

    /// 抗锯齿圆角描边：沿圆角矩形的**内缘**画一圈 `t` 像素宽的边。
    ///
    /// 和 `stroke_round` 的关系同上（GDI `RoundRect` 只有整数台阶）。
    /// 控件边框（白底按钮 / 输入框 / 分段选择）都用这一版：直线边上正好落满
    /// 一个像素、整条边墨色一致；圆弧上外侧多出来的那半个像素是羽化过渡，
    /// 看着才是「一条线」而不是「一串点」。
    pub fn stroke_round_aa(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: i32,
        c: (u8, u8, u8),
        t: i32,
    ) {
        let Some(s) = self.surface else {
            self.stroke_round(x, y, w, h, radius, c, t.max(1));
            return;
        };
        if w <= 0 || h <= 0 || t <= 0 {
            return;
        }
        let y = y + self.vp_y;
        let hw = w as f32 * 0.5;
        let hh = h as f32 * 0.5;
        let cx = x as f32 + hw;
        let cy = y as f32 + hh;
        let rad = (radius as f32).clamp(0.0, hw.min(hh));
        let t = t as f32;
        s.flush();
        let x0 = x.max(0);
        let mut y0 = y.max(0);
        let x1 = (x + w).min(s.w);
        let mut y1 = (y + h).min(s.h);
        if let Some((c0, c1)) = self.clip_y {
            y0 = y0.max(c0);
            y1 = y1.min(c1);
        }
        // 只扫贴着边界的一条窄带，内部整块跳过
        let band = (t + 1.0).ceil() as i32;
        let (bx0, bx1) = (x + band, x + w - band);
        let (by0, by1) = (y + band, y + h - band);
        for py in y0..y1 {
            let edge_v = py < by0 || py >= by1;
            for px in x0..x1 {
                if !(edge_v || px < bx0 || px >= bx1) {
                    continue;
                }
                let d =
                    crate::render::sd_round_rect(px as f32 + 0.5, py as f32 + 0.5, cx, cy, hw, hh, rad);
                // 外缘 1、向内 `t` 像素处 0：两条边都做半像素过渡
                let outer = (0.5 - d).clamp(0.0, 1.0);
                let inner = (0.5 - (d + t)).clamp(0.0, 1.0);
                let cov = outer - inner;
                if cov <= 0.0 {
                    continue;
                }
                s.blend(px, py, c, cov);
            }
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

    /// 实心多边形（顶点按顺序给，顺逆时针都行）
    pub fn fill_poly(&self, pts: &[(i32, i32)], c: (u8, u8, u8)) {
        if pts.len() < 3 {
            return;
        }
        unsafe {
            let col = colorref(c.0, c.1, c.2);
            let brush = CreateSolidBrush(col);
            // 同色 1px 描边：把多边形边缘的锯齿缺口填平，看起来更饱满
            let pen = CreatePen(PS_SOLID, 1, col);
            let old_brush = SelectObject(self.hdc, HGDIOBJ(brush.0));
            let old_pen = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let points: Vec<POINT> = pts.iter().map(|&(x, y)| POINT { x, y }).collect();
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

    /// 量一段文字**实际进位**的宽度（光标定位用，别拿它排布局）
    ///
    /// `text_w` 用的是 `GetTextExtentPoint32W`，它对同一串文字会比 GDI 真正走出来的
    /// 宽度多算一点：16px 的「#00393B」量出 73px，`TextOutW` 画笔实际只走 65px。
    /// 光标按它摆就会离文字空出小半个字符（取色器 hex 框上就是这个毛病）。
    /// 这里改成逐字取 `GetCharWidth32W` 再相加，和绘制时的进位一致。
    pub fn text_w_exact(&self, s: &str) -> i32 {
        let mut total = 0;
        for ch in s.chars() {
            let mut w = 0i32;
            let ok = unsafe { GetCharWidth32W(self.hdc, ch as u32, ch as u32, &mut w) };
            total += if ok.as_bool() { w } else { ch.len_utf16() as i32 * self.px / 2 };
        }
        total
    }

    /// 画一个开关：轨道 + 圆钮。`on` 为真时是蓝色。
    ///
    /// 轨道和圆钮都是纯曲线，走抗锯齿版本 —— GDI 的 `RoundRect` 在这上面会
    /// 留下明显的台阶（轨道两端和圆钮一圈都不圆）。
    pub fn switch(&self, x: i32, y: i32, w: i32, h: i32, on: bool) {
        let track = if on { (47, 122, 245) } else { (120, 124, 132) };
        self.fill_round_aa(x, y, w, h, h / 2, track);
        let knob = h - 6;
        let kx = if on { x + w - knob - 3 } else { x + 3 };
        self.fill_round_aa(kx, y + 3, knob, knob, knob / 2, (255, 255, 255));
    }

    /// 画一个颜色小块（圆角 + 细边，颜色浅的时候也要看得见轮廓）
    pub fn swatch(&self, x: i32, y: i32, w: i32, h: i32, c: (u8, u8, u8)) {
        self.fill_round_aa(x, y, w, h, 4, c);
        let edge = crate::theme::mix(c, (0, 0, 0), 0.25);
        self.stroke_round_aa(x, y, w, h, 4, edge, 1);
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

/// 普通窗口（设置页 / 取色器）的正文字体。
///
/// 旧版是 WebView2，`body` 的 font-family 是
/// `"Segoe UI Variable Text", "Segoe UI", "Microsoft YaHei UI", system-ui` ——
/// 西文走 Segoe UI、中文靠 GDI 的字体链接回退到微软雅黑。原生版早先**整个**用
/// `Microsoft YaHei UI`，西文数字也跟着变成雅黑的拉丁字形，和旧版一比就不像了
/// （版本号、路径、hex 这些西文串最明显）。这里改回 `Segoe UI`：西文回 Segoe UI，
/// 中文由 GDI 自动链接到微软雅黑，两边观感都和旧版一致。
pub const UI_FONT: &str = "Segoe UI";

/// 建一支字体。`px` 是字符高度（正数）。
///
/// 这里用 ClearType（亚像素）抗锯齿：ui 模块只服务于设置页 / 取色器这类
/// **不透明白底窗口**，文字直接画在 DIB 上再 BitBlt 到窗口，子像素颜色不会被
/// 缩放或混合，看起来和系统原生控件一样清晰。
/// 长条和弹出面板是分层窗口（逐像素 alpha），在那边用 ClearType 会在边缘
/// 留下彩色描边，所以那边继续走灰度抗锯齿（`render.rs`）。
pub fn create_font(px: i32, bold: bool) -> windows::Win32::Graphics::Gdi::HFONT {
    create_font_named(px, bold, UI_FONT)
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



