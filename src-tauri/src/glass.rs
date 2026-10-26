//! 毛玻璃背景：截取面板背后那一块屏幕 → 强降采样 + 模糊 → PNG data URL，
//! 前端把它铺在底板下面，就是「磨砂玻璃」。
//!
//! 为什么不用系统的 DWM 亚克力（`SetWindowCompositionAttribute`）：
//!  1. 它受系统「透明效果」开关控制。实测这台机器
//!     `HKCU\...\Themes\Personalize\EnableTransparency = 0`，开与不开的截图差
//!     只有 7/255（肉眼完全看不出），等于白写；
//!  2. 它按**整窗矩形**画模糊，不吃 `SetWindowRgn` 的圆角区域 —— 一旦真生效，
//!     四个圆角之外会多出一块方形模糊，圆角就废了。
//!
//! 自己截自己糊两个问题都没有：形状由窗口区域 + CSS 决定，系统设置关着也照样有玻璃感。
//! 代价只是「背后画面的快照」：面板开着的时候背后的窗口变了，玻璃里的影子不会跟着变
//! （面板本来就是点开看一眼就收的，每次打开都会重新截一张）。
use crate::win32::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{ImageFormat, RgbImage};
use std::ffi::c_void;
use std::io::Cursor;

/// 降采样倍数：截图 432×480 → 62×69 再糊，等效模糊半径一下子就上去了，
/// 同时 CPU 只在小图上跑，耗时不到 1ms。前端再把小图放大，双线性插值继续糊。
const DOWN: i32 = 7;
/// 小图上的模糊半径（像素）。等效原图半径 ≈ BLUR × DOWN ≈ 14px
const BLUR: usize = 2;

/// 截取屏幕上一块矩形，返回「已经糊掉」的 PNG data URL（面板毛玻璃用）
pub fn backdrop_data_url(x: i32, y: i32, w: i32, h: i32) -> Option<String> {
    grab_data_url(x, y, w, h, true)
}

/// 截取屏幕上一块矩形，返回**原样**的 PNG data URL。
///
/// 收纳盒用：盒子是 Progman 的子窗口，没有逐像素 alpha，想让「背景透明」只能把
/// 背后那块桌面截下来贴回去。这里不能糊 —— 平时它要看起来和桌面完全一致；
/// 悬停时的模糊交给 CSS `filter: blur()`（只糊一次，还能做过渡）。
pub fn raw_data_url(x: i32, y: i32, w: i32, h: i32) -> Option<String> {
    grab_data_url(x, y, w, h, false)
}

fn grab_data_url(x: i32, y: i32, w: i32, h: i32, blur: bool) -> Option<String> {
    if w <= 0 || h <= 0 {
        return None;
    }
    // 不糊的时候不做降采样：1:1 取原图，贴回桌面才看不出接缝
    let (down, radius) = if blur { (DOWN, BLUR) } else { (1, 0) };
    let (_sw, _sh, rgb) = unsafe { grab_scaled(x, y, w, h, down, radius)? };
    let mut png: Vec<u8> = Vec::new();
    let mut cursor = Cursor::new(&mut png);
    if image::DynamicImage::ImageRgb8(rgb)
        .write_to(&mut cursor, ImageFormat::Png)
        .is_err()
    {
        return None;
    }
    Some(format!("data:image/png;base64,{}", STANDARD.encode(&png)))
}

/// 屏幕 → 32bpp DIB → 降采样 → 模糊。返回 (宽, 高, RGB 像素)
///
/// `down = 1` 且 `radius = 0` 时就是原图（收纳盒的「透明背景」用这个）。
unsafe fn grab_scaled(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    down: i32,
    radius: usize,
) -> Option<(u32, u32, RgbImage)> {
    let screen = CreateDCW(wide("DISPLAY").as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null_mut());
    if screen == 0 {
        return None;
    }
    let mem = CreateCompatibleDC(screen);
    let bmi = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h, // 自顶向下，和屏幕坐标一致
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, 0, 0);
    if hbm == 0 || bits.is_null() || mem == 0 {
        if hbm != 0 {
            DeleteObject(hbm);
        }
        if mem != 0 {
            DeleteDC(mem);
        }
        DeleteDC(screen);
        return None;
    }

    let old = SelectObject(mem, hbm);
    let blt = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY);
    let mut out = None;
    if blt != 0 {
        let raw = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
        let sw = (w / down).max(1) as u32;
        let sh = (h / down).max(1) as u32;
        // 降采样：每个小像素取 DOWN×DOWN 块的平均（顺便把噪点抹平）
        let mut small = vec![0u8; (sw * sh * 3) as usize];
        for sy in 0..sh {
            for sx in 0..sw {
                let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
                for dy in 0..down {
                    for dx in 0..down {
                        let px = sx as i32 * down + dx;
                        let py = sy as i32 * down + dy;
                        if px >= w || py >= h {
                            continue;
                        }
                        let i = ((py * w + px) * 4) as usize;
                        b += raw[i] as u32;
                        g += raw[i + 1] as u32;
                        r += raw[i + 2] as u32;
                        n += 1;
                    }
                }
                let n = n.max(1);
                let o = ((sy * sw + sx) * 3) as usize;
                small[o] = (r / n) as u8;
                small[o + 1] = (g / n) as u8;
                small[o + 2] = (b / n) as u8;
            }
        }
        blur_rgb(&mut small, sw as usize, sh as usize, radius);
        let img = RgbImage::from_raw(sw, sh, small)?;
        out = Some((sw, sh, img));
    }

    SelectObject(mem, old);
    DeleteObject(hbm);
    DeleteDC(mem);
    DeleteDC(screen);
    out
}

/// 可分离的箱式模糊（横竖各两趟，接近高斯）
fn blur_rgb(px: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 || w < 3 || h < 3 {
        return;
    }
    let mut tmp = vec![0u8; px.len()];
    for _ in 0..2 {
        // 横向
        for y in 0..h {
            for x in 0..w {
                let (mut s0, mut s1, mut s2, mut n) = (0u32, 0u32, 0u32, 0u32);
                let lo = x.saturating_sub(r);
                let hi = (x + r).min(w - 1);
                for xx in lo..=hi {
                    let i = (y * w + xx) * 3;
                    s0 += px[i] as u32;
                    s1 += px[i + 1] as u32;
                    s2 += px[i + 2] as u32;
                    n += 1;
                }
                let o = (y * w + x) * 3;
                tmp[o] = (s0 / n) as u8;
                tmp[o + 1] = (s1 / n) as u8;
                tmp[o + 2] = (s2 / n) as u8;
            }
        }
        // 纵向
        for y in 0..h {
            for x in 0..w {
                let (mut s0, mut s1, mut s2, mut n) = (0u32, 0u32, 0u32, 0u32);
                let lo = y.saturating_sub(r);
                let hi = (y + r).min(h - 1);
                for yy in lo..=hi {
                    let i = (yy * w + x) * 3;
                    s0 += tmp[i] as u32;
                    s1 += tmp[i + 1] as u32;
                    s2 += tmp[i + 2] as u32;
                    n += 1;
                }
                let o = (y * w + x) * 3;
                px[o] = (s0 / n) as u8;
                px[o + 1] = (s1 / n) as u8;
                px[o + 2] = (s2 / n) as u8;
            }
        }
    }
}
