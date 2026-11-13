//! 毛玻璃：把面板背后那一块屏幕截下来、糊掉，当作面板的背景。
//!
//! 为什么不用系统的 DWM 亚克力（`SetWindowCompositionAttribute`）：
//!  1. 它受系统「透明效果」开关控制。开着才有效果，关着等于白写；
//!  2. 它按**整窗矩形**画模糊，不吃圆角区域 —— 一旦真生效，四个圆角之外
//!     会多出一块方形模糊，圆角就废了。
//!
//! 自己截自己糊两个问题都没有：形状由窗口的圆角决定，系统设置关着也照样有玻璃感。
//! 代价只是「背后画面的快照」：面板开着的时候背后的窗口变了，玻璃里的影子
//! 不会跟着变（面板本来就是点开看一眼就收的，每次打开都会重新截一张）。

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HGDIOBJ, SRCCOPY,
};

use crate::render::IconBmp;

/// 降采样倍数：整块截下来之后按 N×N 取均值缩到 1/N，模糊半径一下就上去，CPU 只在小图上跑
const DOWN: i32 = 6;
/// 小图上的模糊半径（像素）。等效原图半径 ≈ BLUR × DOWN ≈ 24 px
const BLUR: i32 = 4;

/// 截取屏幕上的一块矩形并糊掉，返回 BGRA 位图（alpha 全 255）。
///
/// 返回的位图尺寸是 `w/DOWN × h/DOWN` —— 画的时候再用双线性放大回去。
pub fn backdrop(x: i32, y: i32, w: i32, h: i32) -> Option<IconBmp> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let sw = (w / DOWN).max(1);
    let sh = (h / DOWN).max(1);
    // 先按**原尺寸**把整块区域抓下来，再在内存里做 N×N 均值降采样。
    //
    // ⚠️ 不能直接把屏幕 DC `BitBlt` 到小尺寸的 DIB 上：BitBlt **不做缩放**，
    // 它只会把源矩形左上角那 sw×sh 一小块拷过来。那样等于只截了面板背后左上角
    // 一小片、再放大 N 倍铺满整个面板 —— 看起来就是「背后界面的字被放大很多倍」。
    let full = grab(x, y, w, h)?;
    let mut bmp = downsample(&full, w, h, sw, sh);
    blur(&mut bmp, BLUR);
    Some(bmp)
}

/// 从屏幕抓一块 `w×h` 的原始像素（BGRA，顶向下，alpha 统一置 255）
fn grab(x: i32, y: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 顶向下，和屏幕坐标一致
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
                let _ = DeleteDC(mem);
                let _ = ReleaseDC(None, screen);
                return None;
            }
        };
        let old = SelectObject(mem, HGDIOBJ(hbmp.0));
        let ok = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok();
        let mut px = vec![0u8; (w * h * 4) as usize];
        if ok && !bits.is_null() {
            std::ptr::copy_nonoverlapping(bits as *const u8, px.as_mut_ptr(), px.len());
        }
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, screen);
        if !ok {
            return None;
        }
        // GDI 给的是 BGRX，alpha 位不可靠，统一置成不透明
        for i in 0..(w * h) as usize {
            px[i * 4 + 3] = 255;
        }
        Some(px)
    }
}

/// 按面积均值把 `w×h` 的 BGRA 原图缩到 `sw×sh`。
///
/// 每一小格覆盖的原图范围用**比例**算（不是固定 DOWN），尺寸除不尽时
/// 右边/下边不会留下没被采样的黑边。
fn downsample(src: &[u8], w: i32, h: i32, sw: i32, sh: i32) -> IconBmp {
    let mut out = IconBmp::empty(sw, sh);
    if w <= 0 || h <= 0 || sw <= 0 || sh <= 0 || src.len() < (w * h * 4) as usize {
        return out;
    }
    for sy in 0..sh {
        let y0 = (sy as i64 * h as i64 / sh as i64) as i32;
        let y1 = ((((sy as i64 + 1) * h as i64) / sh as i64) as i32)
            .max(y0 + 1)
            .min(h);
        for sx in 0..sw {
            let x0 = (sx as i64 * w as i64 / sw as i64) as i32;
            let x1 = ((((sx as i64 + 1) * w as i64) / sw as i64) as i32)
                .max(x0 + 1)
                .min(w);
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for yy in y0..y1 {
                let mut i = ((yy * w + x0) * 4) as usize;
                for _ in x0..x1 {
                    b += src[i] as u32;
                    g += src[i + 1] as u32;
                    r += src[i + 2] as u32;
                    n += 1;
                    i += 4;
                }
            }
            let n = n.max(1);
            let o = ((sy * sw + sx) * 4) as usize;
            out.px[o] = (b / n) as u8;
            out.px[o + 1] = (g / n) as u8;
            out.px[o + 2] = (r / n) as u8;
            out.px[o + 3] = 255;
        }
    }
    out
}

/// 盒式模糊（半径 r 的方形核），在位图上就地做
fn blur(bmp: &mut IconBmp, r: i32) {
    if r <= 0 || bmp.w <= 0 || bmp.h <= 0 {
        return;
    }
    let (w, h) = (bmp.w, bmp.h);
    let src = bmp.px.clone();
    // 横向
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0u32; 3];
            let mut n = 0;
            for d in -r..=r {
                let sx = (x + d).clamp(0, w - 1);
                let i = ((y * w + sx) * 4) as usize;
                acc[0] += src[i] as u32;
                acc[1] += src[i + 1] as u32;
                acc[2] += src[i + 2] as u32;
                n += 1;
            }
            let o = ((y * w + x) * 4) as usize;
            bmp.px[o] = (acc[0] / n) as u8;
            bmp.px[o + 1] = (acc[1] / n) as u8;
            bmp.px[o + 2] = (acc[2] / n) as u8;
        }
    }
    // 纵向
    let mid = bmp.px.clone();
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0u32; 3];
            let mut n = 0;
            for d in -r..=r {
                let sy = (y + d).clamp(0, h - 1);
                let i = ((sy * w + x) * 4) as usize;
                acc[0] += mid[i] as u32;
                acc[1] += mid[i + 1] as u32;
                acc[2] += mid[i + 2] as u32;
                n += 1;
            }
            let o = ((y * w + x) * 4) as usize;
            bmp.px[o] = (acc[0] / n) as u8;
            bmp.px[o + 1] = (acc[1] / n) as u8;
            bmp.px[o + 2] = (acc[2] / n) as u8;
        }
    }
}

/// 把一张小图平滑放大到目标尺寸（双线性插值）。
///
/// 早先这里写成了最近邻（注释说是双线性，实际只取整不插值），1/DOWN 的小图
/// 放大后能看到一格一格的方块，也是「毛玻璃一块一块」的来源之一。
pub fn scale_up(src: &IconBmp, dw: i32, dh: i32) -> IconBmp {
    let mut out = IconBmp::empty(dw.max(0), dh.max(0));
    if dw <= 0 || dh <= 0 || src.w <= 0 || src.h <= 0 {
        return out;
    }
    if src.px.len() < (src.w * src.h * 4) as usize {
        return out;
    }
    // 每一列 / 每一行的源坐标和权重只算一次，所有行 / 列共用
    let mut cols: Vec<(usize, usize, u32)> = Vec::with_capacity(dw as usize);
    for x in 0..dw {
        let fx = ((x as f32 + 0.5) * src.w as f32 / dw as f32 - 0.5).clamp(0.0, (src.w - 1) as f32);
        let x0 = fx.floor() as i32;
        let x1 = (x0 + 1).min(src.w - 1);
        let wx = ((fx - x0 as f32) * 256.0).round().clamp(0.0, 256.0) as u32;
        cols.push((x0 as usize, x1 as usize, wx));
    }
    let mut rows: Vec<(usize, usize, u32)> = Vec::with_capacity(dh as usize);
    for y in 0..dh {
        let fy = ((y as f32 + 0.5) * src.h as f32 / dh as f32 - 0.5).clamp(0.0, (src.h - 1) as f32);
        let y0 = fy.floor() as i32;
        let y1 = (y0 + 1).min(src.h - 1);
        let wy = ((fy - y0 as f32) * 256.0).round().clamp(0.0, 256.0) as u32;
        rows.push((y0 as usize, y1 as usize, wy));
    }
    let sw = src.w as usize;
    for (y, &(y0, y1, wy)) in rows.iter().enumerate() {
        let r0 = y0 * sw;
        let r1 = y1 * sw;
        for (x, &(x0, x1, wx)) in cols.iter().enumerate() {
            let a = (r0 + x0) * 4;
            let b = (r0 + x1) * 4;
            let c = (r1 + x0) * 4;
            let d = (r1 + x1) * 4;
            let o = (y * dw as usize + x) * 4;
            for k in 0..4 {
                let top = src.px[a + k] as u32 * (256 - wx) + src.px[b + k] as u32 * wx;
                let bot = src.px[c + k] as u32 * (256 - wx) + src.px[d + k] as u32 * wx;
                let v = (top * (256 - wy) + bot * wy) >> 16;
                out.px[o + k] = v.min(255) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 降采样取的是整块均值而不是左上角() {
        // 8×8 的图：左半边全黑、右半边全白
        let (w, h) = (8, 8);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let v = if x < 4 { 0 } else { 200 };
                src[i] = v;
                src[i + 1] = v;
                src[i + 2] = v;
                src[i + 3] = 255;
            }
        }
        let small = downsample(&src, w, h, 4, 4);
        assert_eq!(small.w, 4);
        // 第 0、1 列来自左半边（黑），第 2、3 列来自右半边（白）
        assert!(small.px[0] < 20, "左半边应仍是黑的");
        assert!(small.px[3 * 4] > 180, "右半边应仍是白的");
    }

    #[test]
    fn 放大不越界且保持四角() {
        let mut src = IconBmp::empty(2, 2);
        src.px = vec![10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255, 40, 40, 40, 255];
        let out = scale_up(&src, 8, 8);
        assert_eq!(out.px.len(), 8 * 8 * 4);
        // 左上角最接近源 (0,0)，右下角最接近源 (1,1)
        assert!(out.px[0] >= 10 && out.px[0] <= 14, "左上角应接近 10");
        let last = (7 * 8 + 7) * 4;
        assert!(out.px[last] >= 36 && out.px[last] <= 40, "右下角应接近 40");
    }
}
