//! 图标：从 shell 拿快捷方式的图标，解码成可以直接贴到画布上的位图。
//!
//! 分层窗口是逐像素透明的，图标必须带**真实的 alpha** 才不会露出方框。
//! `SHGetFileInfo` 给的是 HICON，画进 32 位 DIB 之后有两种可能：
//! alpha 被写进去了（现代 Windows 的正常行为），或者整块 alpha 都是 0
//! （GDI 没写 alpha 的老行为）。后者要用图标自带的 AND 掩码补出透明度。

use base64::{engine::general_purpose::STANDARD, Engine};
use image::ImageFormat;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use windows::Win32::Foundation::{COLORREF, HANDLE, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    DrawTextW, GetDC, GetDIBits, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, FW_NORMAL, HBITMAP, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, DrawIconEx, GetIconInfo, ICONINFO, LoadImageW, DI_NORMAL, HICON,
    IMAGE_ICON, LR_LOADFROMFILE,
};

use crate::render::IconBmp;
use crate::util::wide;

static CACHE: Mutex<Option<HashMap<String, Option<IconBmp>>>> = Mutex::new(None);

/// 取一个路径的图标位图。重复调用走缓存，不再问 shell。
pub fn icon_bmp(path: &Path, size: i32) -> Option<IconBmp> {
    let key = format!("{}|{}", path.to_string_lossy().to_ascii_lowercase(), size);
    {
        let guard = CACHE.lock().unwrap();
        if let Some(map) = guard.as_ref() {
            if let Some(hit) = map.get(&key) {
                return hit.clone();
            }
        }
    }
    let value = extract(path, size);
    // 只缓存成功结果：文件刚被移动过来时 shell 偶尔还拿不到图标，
    // 把这种「失败」缓存下来会让那个图标在整个会话里一直空着。
    if value.is_some() {
        let mut guard = CACHE.lock().unwrap();
        guard.get_or_insert_with(HashMap::new).insert(key, value.clone());
    }
    value
}

/// 解码一段 PNG 字节（内嵌图标用，比如托盘图标）
pub fn decode_png_bytes(raw: &[u8]) -> Option<IconBmp> {
    let img = image::load_from_memory_with_format(raw, ImageFormat::Png).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = IconBmp::empty(w as i32, h as i32);
    for (i, p) in rgba.pixels().enumerate() {
        out.px[i * 4] = p[2]; // B
        out.px[i * 4 + 1] = p[1]; // G
        out.px[i * 4 + 2] = p[0]; // R
        out.px[i * 4 + 3] = p[3]; // A
    }
    Some(out)
}

/// 解码 PNG data URL（配置里存的自定义图标就是这个格式）
pub fn decode_png_data_url(data: &str) -> Option<IconBmp> {
    let b64 = data.split_once(',')?.1.trim();
    let raw = STANDARD.decode(b64).ok()?;
    decode_png_bytes(&raw)
}

/// 从一个 .ico 文件里取出图标，转成 PNG data URL（设置页保存自定义图标用）
pub fn ico_file_data_url(path: &Path, size: i32) -> Option<String> {
    unsafe {
        let wpath = wide(&path.to_string_lossy());
        let hicon = LoadImageW(
            None,
            windows::core::PCWSTR(wpath.as_ptr()),
            IMAGE_ICON,
            size,
            size,
            LR_LOADFROMFILE,
        )
        .ok()?;
        let hicon = HICON(hicon.0);
        let bmp = render_icon(hicon, size)?;
        let _ = DestroyIcon(hicon);
        let rgba = to_rgba(&bmp)?;
        let mut png: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut png);
        if image::RgbaImage::from_raw(rgba.0, rgba.1, rgba.2)
            .and_then(|i| image::DynamicImage::ImageRgba8(i).write_to(&mut cursor, ImageFormat::Png).ok())
            .is_some()
        {
            return Some(format!("data:image/png;base64,{}", STANDARD.encode(&png)));
        }
        None
    }
}

fn to_rgba(bmp: &IconBmp) -> Option<(u32, u32, Vec<u8>)> {
    let mut out = vec![0u8; (bmp.w * bmp.h * 4) as usize];
    for i in 0..(bmp.w * bmp.h) as usize {
        let (b, g, r, a) = (
            bmp.px[i * 4],
            bmp.px[i * 4 + 1],
            bmp.px[i * 4 + 2],
            bmp.px[i * 4 + 3],
        );
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = a;
    }
    Some((bmp.w as u32, bmp.h as u32, out))
}

fn extract(path: &Path, size: i32) -> Option<IconBmp> {
    // 快捷方式：图标其实在目标程序（exe）里。shell 给的只有 32×32，
    // 面板上的图标要 48~64 像素，所以直接去目标程序里按尺寸抽一张。
    if let Some(bmp) = extract_from_target(path, size) {
        return Some(bmp);
    }
    extract_from_shell(path, size)
}

/// 从快捷方式指向的程序里按尺寸抽图标
fn extract_from_target(path: &Path, size: i32) -> Option<IconBmp> {
    let sc = crate::shortcut::resolve_lnk(path)?;
    let target = std::path::PathBuf::from(&sc.target);
    if !target.is_file() {
        return None;
    }
    unsafe {
        let wpath = wide(&target.to_string_lossy());
        // hInstance 传 None：路径指到文件上，走 LR_LOADFROMFILE 从文件里读图标资源。
        // 现代 exe 里一般有 256×256 的图标，缩到 64 依然很清楚。
        let hicon = LoadImageW(
            None,
            windows::core::PCWSTR(wpath.as_ptr()),
            IMAGE_ICON,
            size,
            size,
            LR_LOADFROMFILE,
        )
        .ok()?;
        let hicon = HICON(hicon.0);
        let out = render_icon(hicon, size);
        let _ = DestroyIcon(hicon);
        out
    }
}

/// 退回 shell：它给的是 32×32 的图标
fn extract_from_shell(path: &Path, size: i32) -> Option<IconBmp> {
    unsafe {
        let wpath = wide(&path.to_string_lossy());
        let mut info = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            windows::core::PCWSTR(wpath.as_ptr()),
            Default::default(),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if r == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let hicon = info.hIcon;
        let out = render_icon(hicon, size);
        let _ = DestroyIcon(hicon);
        out
    }
}

/// 把 HICON 画进 32 位 DIB 并取出像素；alpha 不可用时用 AND 掩码补
unsafe fn render_icon(hicon: HICON, size: i32) -> Option<IconBmp> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [Default::default(); 1],
    };

    let screen = GetDC(None);
    let memdc = CreateCompatibleDC(Some(screen));
    if memdc.is_invalid() {
        let _ = ReleaseDC(None, screen);
        return None;
    }
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let hbmp = match CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None::<HANDLE>, 0)
    {
        Ok(h) => h,
        Err(_) => {
            let _ = ReleaseDC(None, screen);
            return None;
        }
    };
    let old = SelectObject(memdc, HGDIOBJ(hbmp.0));
    let len = (size * size * 4) as usize;
    std::ptr::write_bytes(bits as *mut u8, 0, len);
    let _ = DrawIconEx(memdc, 0, 0, hicon, size, size, 0, None, DI_NORMAL);

    let mut bmp = IconBmp::empty(size, size);
    std::ptr::copy_nonoverlapping(bits as *const u8, bmp.px.as_mut_ptr(), len);

    SelectObject(memdc, old);
    let _ = DeleteObject(HGDIOBJ(hbmp.0));
    let _ = ReleaseDC(None, screen);

    // alpha 全是 0 = GDI 没写 alpha，用图标自己的 AND 掩码补出透明区域
    let any_alpha = bmp.px.chunks_exact(4).any(|p| p[3] != 0);
    if !any_alpha {
        if let Some(mask) = and_mask(hicon) {
            for i in 0..(size * size) as usize {
                bmp.px[i * 4 + 3] = if mask[i] { 0 } else { 255 };
            }
        } else {
            for i in 0..(size * size) as usize {
                bmp.px[i * 4 + 3] = 255;
            }
        }
        return Some(bmp);
    }

    // 预乘过的缓冲区要反预乘，否则半透明边缘会发暗
    if is_premultiplied(&bmp) {
        for i in 0..(size * size) as usize {
            let a = bmp.px[i * 4 + 3] as u32;
            if a == 0 {
                bmp.px[i * 4] = 0;
                bmp.px[i * 4 + 1] = 0;
                bmp.px[i * 4 + 2] = 0;
                continue;
            }
            for c in 0..3 {
                let v = bmp.px[i * 4 + c] as u32 * 255 / a;
                bmp.px[i * 4 + c] = v.min(255) as u8;
            }
        }
    }
    Some(bmp)
}

/// 缓冲区看起来是不是预乘过的（每个像素的颜色通道都 ≤ alpha）
fn is_premultiplied(bmp: &IconBmp) -> bool {
    for p in bmp.px.chunks_exact(4) {
        let a = p[3] as u32;
        let m = p[0].max(p[1]).max(p[2]) as u32;
        if m > a {
            return false;
        }
    }
    true
}

/// 读图标的 AND 掩码：true = 该像素透明
unsafe fn and_mask(hicon: HICON) -> Option<Vec<bool>> {
    let mut ii = ICONINFO::default();
    if GetIconInfo(hicon, &mut ii).is_err() {
        return None;
    }
    let mask = ii.hbmMask;
    if mask.is_invalid() {
        return None;
    }
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(Some(screen));
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 0,
            biHeight: 0,
            biPlanes: 1,
            biBitCount: 1,
            biCompression: 0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [Default::default(); 1],
    };
    // 先问尺寸
    let mut probe = BITMAPINFO::default();
    let _ = GetDIBits(dc, mask, 0, 0, None, &mut probe, DIB_RGB_COLORS);
    let w = probe.bmiHeader.biWidth;
    let h = probe.bmiHeader.biHeight.abs();
    if w <= 0 || h <= 0 {
        let _ = ReleaseDC(None, screen);
        return None;
    }
    let stride = ((w + 31) / 32) * 4;
    let mut buf = vec![0u8; (stride * h) as usize];
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h; // 顶向下
    if GetDIBits(
        dc,
        mask,
        0,
        h as u32,
        Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    ) == 0
    {
        let _ = ReleaseDC(None, screen);
        return None;
    }
    let _ = ReleaseDC(None, screen);

    let mut out = vec![false; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let byte = buf[(y * stride + x / 8) as usize];
            let bit = (byte >> (7 - x % 8)) & 1;
            out[(y * w + x) as usize] = bit == 1;
        }
    }
    Some(out)
}

/// 位图 → HICON：32bpp 带 alpha 的颜色位图 + 全 0 的掩码（透明全交给 alpha）。
///
/// 托盘图标要用：v0.9.33 的托盘是内嵌的 32×32 PNG，得转成 HICON 才能挂到
/// `Shell_NotifyIconW` 上。用完必须 `DestroyIcon`。
pub fn bmp_to_hicon(bmp: &IconBmp) -> Option<HICON> {
    let (w, h) = (bmp.w, bmp.h);
    if w <= 0 || h <= 0 || bmp.px.len() < (w * h * 4) as usize {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let cleanup = |color: Option<windows::Win32::Graphics::Gdi::HBITMAP>,
                       mask: Option<windows::Win32::Graphics::Gdi::HBITMAP>| {
            if let Some(c) = color {
                let _ = DeleteObject(HGDIOBJ(c.0));
            }
            if let Some(m) = mask {
                let _ = DeleteObject(HGDIOBJ(m.0));
            }
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(None, screen);
        };
        // 颜色位图：HICON 按 bottom-up 解释，所以要把顶向下的 IconBmp 逐行倒着拷
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color = match CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) if !bits.is_null() => b,
            _ => {
                cleanup(None, None);
                return None;
            }
        };
        for y in 0..h {
            let src = (y * w * 4) as usize;
            let dst = ((h - 1 - y) * w * 4) as usize;
            std::ptr::copy_nonoverlapping(
                bmp.px.as_ptr().add(src),
                (bits as *mut u8).add(dst),
                (w * 4) as usize,
            );
        }
        // 掩码：1bpp，整块 0。AND 掩码全 0 = 「不挖洞」，真正的透明由 alpha 决定
        let stride = ((w + 15) / 16) * 2;
        let mask_bits = vec![0u8; (stride * h) as usize];
        let mask = CreateBitmap(w, h, 1, 1, Some(mask_bits.as_ptr() as *const core::ffi::c_void));
        if mask.0.is_null() {
            cleanup(Some(color), None);
            return None;
        }
        let info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).ok();
        cleanup(Some(color), Some(mask));
        icon
    }
}

/// 默认「收纳盒」图标：蓝色盒身 + 浅蓝盒盖 + 白色卡扣。
///
/// 和 v0.9.33 前端里的 `BoxGlyph` SVG 同一套几何（48 单位 viewBox），
/// 按 4× 超采样渲染，圆角和渐变的边缘才不毛糙。同一个尺寸只算一次。
pub fn box_glyph(size: i32) -> IconBmp {
    static CACHE: Mutex<Option<HashMap<i32, IconBmp>>> = Mutex::new(None);
    if size <= 0 {
        return IconBmp::empty(0, 0);
    }
    {
        let mut guard = CACHE.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some(hit) = map.get(&size) {
            return hit.clone();
        }
    }
    let bmp = render_box_glyph(size);
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(size, bmp.clone());
    bmp
}

/// 圆角矩形命中判定（`left/top/w/h/r` 都是 viewBox 单位）
fn in_round_rect(x: f32, y: f32, left: f32, top: f32, w: f32, h: f32, r: f32) -> bool {
    let (right, bottom) = (left + w, top + h);
    if x < left || x > right || y < top || y > bottom {
        return false;
    }
    let cx = x.clamp(left + r, right - r);
    let cy = y.clamp(top + r, bottom - r);
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= r * r
}

/// 一个 48 单位 viewBox 内某点的颜色 + 覆盖率（`None` = 完全在形状之外）
fn sample_box_glyph(u: f32, v: f32) -> Option<(f32, f32, f32, f32)> {
    let lerp = |a: (u8, u8, u8), b: (u8, u8, u8), t: f32| {
        (
            a.0 as f32 + (b.0 as f32 - a.0 as f32) * t,
            a.1 as f32 + (b.1 as f32 - a.1 as f32) * t,
            a.2 as f32 + (b.2 as f32 - a.2 as f32) * t,
        )
    };
    // 卡扣（最上层，白 90%）
    if in_round_rect(u, v, 19.0, 13.0, 10.0, 4.2, 2.1) {
        return Some((255.0, 255.0, 255.0, 230.0));
    }
    // 盒盖：浅蓝渐变
    if in_round_rect(u, v, 3.5, 9.0, 41.0, 12.0, 6.0) {
        let t = ((v - 9.0) / 12.0).clamp(0.0, 1.0);
        let (r, g, b) = lerp((0xA8, 0xC8, 0xFF), (0x6B, 0x9C, 0xFA), t);
        return Some((r, g, b, 255.0));
    }
    // 盒身：蓝渐变 + 左侧一道 12% 的白色高光面
    if in_round_rect(u, v, 6.0, 17.0, 36.0, 25.0, 7.0) {
        let t = ((v - 17.0) / 25.0).clamp(0.0, 1.0);
        let (mut r, mut g, mut b) = lerp((0x6F, 0xA4, 0xFF), (0x2F, 0x6F, 0xEB), t);
        if (13.0..=23.0).contains(&u) {
            r += (255.0 - r) * 0.12;
            g += (255.0 - g) * 0.12;
            b += (255.0 - b) * 0.12;
        }
        return Some((r, g, b, 255.0));
    }
    None
}

fn render_box_glyph(size: i32) -> IconBmp {
    const SS: i32 = 4; // 每像素 4×4 超采样
    let mut out = IconBmp::empty(size, size);
    let unit = 48.0 / size as f32;
    let n = (SS * SS) as f32;
    for py in 0..size {
        for px in 0..size {
            let (mut ar, mut ag, mut ab, mut aa) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (px as f32 + (sx as f32 + 0.5) / SS as f32) * unit;
                    let v = (py as f32 + (sy as f32 + 0.5) / SS as f32) * unit;
                    if let Some((r, g, b, a)) = sample_box_glyph(u, v) {
                        ar += r * a;
                        ag += g * a;
                        ab += b * a;
                        aa += a;
                    }
                }
            }
            let i = ((py * size + px) * 4) as usize;
            if aa > 0.0 {
                out.px[i] = (ab / aa).clamp(0.0, 255.0) as u8;
                out.px[i + 1] = (ag / aa).clamp(0.0, 255.0) as u8;
                out.px[i + 2] = (ar / aa).clamp(0.0, 255.0) as u8;
                out.px[i + 3] = (aa / n).clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

// ── 右键菜单左侧的小图标 ───────────────────────────────────────
//
// 系统菜单项想带图标只能走 `SetMenuItemBitmaps`：给它一张位图，系统用
// `BitBlt` 原样贴上去 —— **不认 alpha**，透明像素会变成黑块。所以这里
// 分两步：先用超采样算出「覆盖率」当 alpha，再把图标合成到菜单背景色上
// 得到一张不透明位图。菜单背景色从 `GetSysColor(COLOR_MENU)` 取，
// 深浅主题都能贴住，不会出现一圈色块。

/// 右键菜单左侧的小图标种类
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MenuGlyph {
    /// 主界面模式：显示器
    Mode,
    /// 设置：齿轮
    Settings,
    /// 新建收纳盒：方框 + 加号
    NewBox,
    /// 解散收纳盒：方框 + 向右的箭头
    Dissolve,
    /// 删除收纳盒：垃圾桶
    Delete,
}

/// 点到线段的距离（16 单位 viewBox）
fn seg_dist(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= 1e-6 {
        0.0
    } else {
        (((x - ax) * dx + (y - ay) * dy) / len2).clamp(0.0, 1.0)
    };
    let (px, py) = (ax + t * dx, ay + t * dy);
    ((x - px) * (x - px) + (y - py) * (y - py)).sqrt()
}

/// 宽 `w` 的圆头线段
fn in_seg(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32, w: f32) -> bool {
    seg_dist(x, y, ax, ay, bx, by) <= w * 0.5
}

/// 圆环（半径 r、环宽 w）
fn in_ring(x: f32, y: f32, cx: f32, cy: f32, r: f32, w: f32) -> bool {
    let d = ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt();
    (d - r).abs() <= w * 0.5
}

/// 圆角矩形的**描边**（线宽 `sw`）
fn in_stroke_round_rect(x: f32, y: f32, l: f32, t: f32, w: f32, h: f32, r: f32, sw: f32) -> bool {
    in_round_rect(x, y, l, t, w, h, r)
        && !in_round_rect(
            x,
            y,
            l + sw,
            t + sw,
            w - sw * 2.0,
            h - sw * 2.0,
            (r - sw).max(0.0),
        )
}

/// 16 单位 viewBox 内某点是否落在图标笔画上
fn sample_menu_glyph(g: MenuGlyph, u: f32, v: f32) -> bool {
    match g {
        // 显示器：屏幕框 + 支架 + 底座
        MenuGlyph::Mode => {
            in_stroke_round_rect(u, v, 1.5, 2.4, 13.0, 8.2, 1.2, 1.7)
                || in_seg(u, v, 8.0, 10.6, 8.0, 12.6, 1.7)
                || in_seg(u, v, 5.0, 13.3, 11.0, 13.3, 1.7)
        }
        // 齿轮：圆环 + 8 个短齿（齿间留 1.7 单位空隙，缩到 16px 才分得开）
        MenuGlyph::Settings => {
            if in_ring(u, v, 8.0, 8.0, 4.0, 1.8) {
                return true;
            }
            (0..8).any(|k| {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let (s, c) = (a.sin(), a.cos());
                in_seg(u, v, 8.0 + c * 4.4, 8.0 + s * 4.4, 8.0 + c * 6.0, 8.0 + s * 6.0, 2.4)
            })
        }
        // 方框 + 右下角的加号
        MenuGlyph::NewBox => {
            in_stroke_round_rect(u, v, 1.5, 2.0, 8.5, 8.5, 1.2, 1.7)
                || in_seg(u, v, 9.9, 11.8, 14.5, 11.8, 2.0)
                || in_seg(u, v, 12.2, 9.5, 12.2, 14.1, 2.0)
        }
        // 方框 + 向右的箭头（图标被放回桌面）
        MenuGlyph::Dissolve => {
            in_stroke_round_rect(u, v, 1.5, 2.5, 7.5, 7.5, 1.2, 1.7)
                || in_seg(u, v, 9.6, 12.8, 14.4, 12.8, 1.9)
                || in_seg(u, v, 12.4, 10.8, 14.6, 12.8, 1.9)
                || in_seg(u, v, 12.4, 14.8, 14.6, 12.8, 1.9)
        }
        // 垃圾桶：盖 + 梯形桶身（16px 里再画提手和竖纹就糊了，省掉）
        MenuGlyph::Delete => {
            in_seg(u, v, 2.6, 3.6, 13.4, 3.6, 1.9)
                || in_seg(u, v, 4.4, 4.8, 5.4, 13.4, 1.9)
                || in_seg(u, v, 11.6, 4.8, 10.6, 13.4, 1.9)
                || in_seg(u, v, 5.4, 13.4, 10.6, 13.4, 1.9)
        }
    }
}

/// 渲染菜单图标的**覆盖率**（rgb 不用，只看 alpha 通道）
fn render_menu_glyph(g: MenuGlyph, size: i32) -> IconBmp {
    const SS: i32 = 6; // 每像素 6×6 超采样，小图标才不毛糙
    let mut out = IconBmp::empty(size, size);
    let unit = 16.0 / size as f32;
    let n = (SS * SS) as f32;
    for py in 0..size {
        for px in 0..size {
            let mut hit = 0.0f32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (px as f32 + (sx as f32 + 0.5) / SS as f32) * unit;
                    let v = (py as f32 + (sy as f32 + 0.5) / SS as f32) * unit;
                    if sample_menu_glyph(g, u, v) {
                        hit += 1.0;
                    }
                }
            }
            if hit > 0.0 {
                let i = ((py * size + px) * 4) as usize;
                out.px[i + 3] = (hit / n * 255.0).clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// 每个图标用 Segoe MDL2 Assets 里的哪个字形。
///
/// 这是 Windows 10/11 都自带的图标字体，小字号下字形本身做过调整，
/// 比用几何线条自己拼清晰得多（16 px 手绘的齿轮、箭头基本糊成一团）。
fn glyph_char(g: MenuGlyph) -> u32 {
    match g {
        MenuGlyph::Mode => 0xE7F4,     // TVMonitor：显示器
        MenuGlyph::Settings => 0xE713, // Setting：齿轮
        MenuGlyph::NewBox => 0xE710,   // Add：加号
        MenuGlyph::Dissolve => 0xE72A, // Share：向右的箭头
        MenuGlyph::Delete => 0xE74D,   // Delete：垃圾桶
    }
}

/// 把图标字体的字形画到 `size × size` 的不透明位图上（底色 `bg`、字形 `fg`）。
///
/// 关键点：**先按 4 倍大小渲染，再降采样**。16 px 的字形笔画只有 1 px 宽，
/// 直接画会落在两个像素中间，抗锯齿后变成半透明灰蓝，看着发虚；
/// 放大到 64 px 时笔画有 4 px 宽，缩回去正好是实心 1 px。
pub fn font_glyph(cp: u32, size: i32, bg: (u8, u8, u8), fg: (u8, u8, u8)) -> Option<IconBmp> {
    const SS: i32 = 4;
    if size <= 0 {
        return None;
    }
    let big = size * SS;
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: big,
                biHeight: -big,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None::<HANDLE>, 0).ok()?;
        if bits.is_null() {
            return None;
        }
        let px = bits as *mut u8;
        let n = (big * big) as usize;
        for i in 0..n {
            *px.add(i * 4) = bg.2;
            *px.add(i * 4 + 1) = bg.1;
            *px.add(i * 4 + 2) = bg.0;
            *px.add(i * 4 + 3) = 255;
        }

        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, HGDIOBJ(hbmp.0));
        let font = CreateFontW(
            -big,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
            &windows::core::HSTRING::from("Segoe MDL2 Assets"),
        );
        let oldf = SelectObject(dc, HGDIOBJ(font.0));
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(
            dc,
            COLORREF(((fg.2 as u32) << 16) | ((fg.1 as u32) << 8) | fg.0 as u32),
        );
        let mut text = [cp as u16];
        let flags = DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
        // 这套字体里有些字形是细线（加号、箭头），有些是粗线（齿轮、垃圾桶）。
        // 细线缩到 16px 只剩不到 1px，看着发虚。在 4 倍画布上叠三遍
        // （右移 2、下移 2），等于把笔画加粗半像素，粗细就统一了。
        for (dx, dy) in [(0, 0), (2, 0), (0, 2)] {
            let mut rc = RECT {
                left: dx,
                top: dy,
                right: big + dx,
                bottom: big + dy,
            };
            let _ = DrawTextW(dc, &mut text, &mut rc, flags);
        }

        // 4×4 块平均降采样
        let mut out = IconBmp::empty(size, size);
        let ss = (SS * SS) as u32;
        for y in 0..size {
            for x in 0..size {
                let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                for sy in 0..SS {
                    for sx in 0..SS {
                        let i = (((y * SS + sy) * big + (x * SS + sx)) * 4) as usize;
                        let s = *px.add(i) as u32;
                        let s1 = *px.add(i + 1) as u32;
                        let s2 = *px.add(i + 2) as u32;
                        b += s;
                        g += s1;
                        r += s2;
                    }
                }
                let o = ((y * size + x) * 4) as usize;
                out.px[o] = (b / ss) as u8;
                out.px[o + 1] = (g / ss) as u8;
                out.px[o + 2] = (r / ss) as u8;
                out.px[o + 3] = 255;
            }
        }

        SelectObject(dc, oldf);
        let _ = DeleteObject(HGDIOBJ(font.0));
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(dc);
        Some(out)
    }
}

/// 给 `SetMenuItemBitmaps` 用的不透明位图：图标合成到菜单背景色上。
///
/// 同一种图标 + 同一套颜色只生成一次；位图挂在进程生命周期里不释放
/// （16×16 的图，几张加起来不到几 KB，菜单反复弹出也复用它）。
pub fn menu_bitmap(g: MenuGlyph, size: i32, bg: (u8, u8, u8), fg: (u8, u8, u8)) -> Option<HBITMAP> {
    static CACHE: Mutex<Option<HashMap<(MenuGlyph, i32, u8, u8, u8, u8, u8, u8), usize>>> =
        Mutex::new(None);

    if size <= 0 {
        return None;
    }
    let key = (g, size, bg.0, bg.1, bg.2, fg.0, fg.1, fg.2);
    if let Some(map) = CACHE.lock().unwrap().as_ref() {
        if let Some(&h) = map.get(&key) {
            return Some(HBITMAP(h as *mut core::ffi::c_void));
        }
    }

    // 图标字体优先；万一系统里没有（精简版镜像），退回自己画的几何图标
    let font_ok = |b: &IconBmp| {
        b.px.chunks(4)
            .any(|p| p[0] != bg.2 || p[1] != bg.1 || p[2] != bg.0)
    };
    let bmp = match font_glyph(glyph_char(g), size, bg, fg) {
        Some(b) if font_ok(&b) => b,
        _ => render_menu_fallback(g, size, bg, fg),
    };

    let hbmp = unsafe { bgra_to_bitmap(size, size, &bmp.px) }?;
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, hbmp.0 as usize);
    Some(hbmp)
}

/// 字体拿不到时的兜底：把几何图标的覆盖率合成到背景色上
fn render_menu_fallback(g: MenuGlyph, size: i32, bg: (u8, u8, u8), fg: (u8, u8, u8)) -> IconBmp {
    let alpha = render_menu_glyph(g, size);
    let mut out = IconBmp::empty(size, size);
    for i in 0..(size * size) as usize {
        let a = alpha.px[i * 4 + 3] as f32 / 255.0;
        // 内存里是 BGRA：0=B 1=G 2=R
        out.px[i * 4] = (bg.2 as f32 * (1.0 - a) + fg.2 as f32 * a) as u8;
        out.px[i * 4 + 1] = (bg.1 as f32 * (1.0 - a) + fg.1 as f32 * a) as u8;
        out.px[i * 4 + 2] = (bg.0 as f32 * (1.0 - a) + fg.0 as f32 * a) as u8;
        out.px[i * 4 + 3] = 255;
    }
    out
}

/// 把一段 BGRA 像素包成 HBITMAP（32bpp、自上而下）
unsafe fn bgra_to_bitmap(w: i32, h: i32, px: &[u8]) -> Option<HBITMAP> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // 负数 = 自上而下，和 px 的顺序一致
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None::<HANDLE>, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        return None;
    }
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
    Some(hbmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动跑：`cargo test --release 导出图标预览 -- --ignored --nocapture`
    /// 会在临时目录生成预览图，用来肉眼确认默认盒子图标 / 托盘图标的样子。
    #[test]
    #[ignore]
    fn 导出图标预览() {
        let dir = std::env::temp_dir().join("lightbox-icon-preview");
        let _ = std::fs::create_dir_all(&dir);
        // 盒子图标：常用尺寸各来一张，放大 6 倍方便看
        for size in [16, 20, 24, 32] {
            let g = box_glyph(size);
            let mut img = image::RgbaImage::new((size * 6) as u32, (size * 6) as u32);
            for y in 0..size * 6 {
                for x in 0..size * 6 {
                    let sx = (x / 6) as usize;
                    let sy = (y / 6) as usize;
                    let i = (sy * size as usize + sx) * 4;
                    let a = g.px[i + 3] as u32;
                    let inv = 255 - a;
                    // 铺在深色任务栏底色上，模拟真实观感
                    let bg = (32u32, 32u32, 32u32);
                    let px = image::Rgba([
                        ((g.px[i + 2] as u32 * a + bg.0 * inv) / 255) as u8,
                        ((g.px[i + 1] as u32 * a + bg.1 * inv) / 255) as u8,
                        ((g.px[i] as u32 * a + bg.2 * inv) / 255) as u8,
                        255,
                    ]);
                    img.put_pixel(x as u32, y as u32, px);
                }
            }
            let p = dir.join(format!("box-{size}-x6.png"));
            img.save(&p).unwrap();
            println!("{}", p.display());
        }
        // 托盘图标原图直接存一份
        let tray = decode_png_bytes(include_bytes!("../assets/tray.png")).unwrap();
        assert_eq!((tray.w, tray.h), (32, 32));
        println!("托盘图标 {}x{} 解码正常", tray.w, tray.h);
    }

    /// 手动跑：`cargo test --release 导出菜单图标预览 -- --ignored --nocapture`
    /// 把五个菜单图标画在浅色底上放大 10 倍，肉眼确认造型和描边粗细。
    #[test]
    #[ignore]
    fn 导出菜单图标预览() {
        let dir = std::env::temp_dir().join("lightbox-icon-preview");
        let _ = std::fs::create_dir_all(&dir);
        let glyphs = [
            MenuGlyph::Mode,
            MenuGlyph::Settings,
            MenuGlyph::NewBox,
            MenuGlyph::Dissolve,
            MenuGlyph::Delete,
        ];
        let size = 16;
        let zoom = 10;
        let gap = 20;
        let bg = (240u8, 240u8, 240u8);
        let fg = (0x2Fu8, 0x6Fu8, 0xEBu8);
        let w = size * zoom * glyphs.len() as i32 + gap * (glyphs.len() as i32 - 1);
        let mut img = image::RgbaImage::new(w as u32, (size * zoom) as u32);
        for p in img.pixels_mut() {
            *p = image::Rgba([240, 240, 240, 255]);
        }
        for (k, g) in glyphs.iter().enumerate() {
            let bmp = font_glyph(glyph_char(*g), size, bg, fg)
                .unwrap_or_else(|| render_menu_fallback(*g, size, bg, fg));
            let ox = k as i32 * (size * zoom + gap);
            for y in 0..size * zoom {
                for x in 0..size * zoom {
                    let sx = (x / zoom) as usize;
                    let sy = (y / zoom) as usize;
                    let i = (sy * size as usize + sx) * 4;
                    img.put_pixel(
                        (ox + x) as u32,
                        y as u32,
                        image::Rgba([
                            bmp.px[i + 2],
                            bmp.px[i + 1],
                            bmp.px[i],
                            255,
                        ]),
                    );
                }
            }
        }
        let p = dir.join("menu-icons.png");
        img.save(&p).unwrap();
        println!("{}", p.display());
    }

    /// 手动跑：`cargo test --release 托盘图标转换 -- --ignored --nocapture`
    /// 走一遍「PNG → IconBmp → HICON → 画回来」，确认 bmp_to_hicon 的
    /// 行序 / alpha / 掩码都对（画回来应该和原图基本一致）。
    #[test]
    #[ignore]
    fn 托盘图标转换往返() {
        let src = decode_png_bytes(include_bytes!("../assets/tray.png")).unwrap();
        let hicon = bmp_to_hicon(&src).expect("转 HICON 失败");
        let back = unsafe { render_icon(hicon, 32) }.expect("画回来失败");
        unsafe { let _ = DestroyIcon(hicon); }

        // 蓝色底应该还在：统计不透明像素占比 + 平均色
        let (mut n, mut r, mut g, mut b) = (0u32, 0u32, 0u32, 0u32);
        for i in 0..(32 * 32) as usize {
            if back.px[i * 4 + 3] > 128 {
                n += 1;
                b += back.px[i * 4] as u32;
                g += back.px[i * 4 + 1] as u32;
                r += back.px[i * 4 + 2] as u32;
            }
        }
        assert!(n > 800, "不透明像素太少：{n}");
        let (mr, mg, mb) = (r / n, g / n, b / n);
        println!("不透明 {n} px，平均色 #{mr:02X}{mg:02X}{mb:02X}");
        // 旧托盘是蓝底白字：蓝通道应该明显高于红通道
        assert!(mb > mr + 40, "颜色不对：蓝 {mb} 红 {mr}");
    }
}
