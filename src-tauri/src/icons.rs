//! 用 shell 提取快捷方式的高质量图标，转成内联 PNG data URL 供前端渲染。
use crate::win32::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Cursor;
use std::path::Path;
use std::sync::Mutex;

static CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

pub fn icon_data_url(path: &Path) -> Option<String> {
    let key = path.to_string_lossy().to_ascii_lowercase();
    {
        let guard = CACHE.lock().unwrap();
        if let Some(map) = guard.as_ref() {
            if let Some(hit) = map.get(&key) {
                return hit.clone();
            }
        }
    }
    let value = extract(path);
    // 只缓存成功结果：文件刚被移动过来时 shell 偶尔还拿不到图标，
    // 把这种「失败」缓存下来会让那个图标在整个会话里一直空着。
    if value.is_some() {
        let mut guard = CACHE.lock().unwrap();
        guard.get_or_insert_with(HashMap::new).insert(key, value.clone());
    }
    value
}

/// 让某个目录下的图标缓存全部失效，下次取图时重新向 shell 要一遍。
/// 用途：文件刚被移动进盒子时 shell 可能还没准备好图标，或者用户手动刷新桌面后
/// 希望盒子里也跟着刷新 —— 不清缓存就会一直用第一次（可能是错/空）的结果。
pub fn invalidate_dir(dir: &Path) {
    let prefix = dir.to_string_lossy().to_ascii_lowercase();
    if prefix.is_empty() {
        return;
    }
    if let Some(map) = CACHE.lock().unwrap().as_mut() {
        map.retain(|k, _| !k.starts_with(&prefix));
    }
}

/// 从一个 .ico 文件里取出图标，转成指定尺寸的 PNG data URL。
///
/// 用 `LoadImageW(LR_LOADFROMFILE)` 而不是解析 ICO 结构：Windows 会自己挑最合适的那一档
/// （256/128/64/48/32…）再缩放到目标尺寸，比手写 ICO 目录解析稳得多，也顺带支持 PNG 压缩的 ico。
pub fn ico_file_data_url(path: &Path, size: i32) -> Option<String> {
    unsafe {
        let wpath = wide(&path.to_string_lossy());
        let hicon = LoadImageW(0, wpath.as_ptr(), IMAGE_ICON, size, size, LR_LOADFROMFILE);
        if hicon == 0 {
            return None;
        }
        let out = render(hicon, size);
        DestroyIcon(hicon);
        out
    }
}

fn extract(path: &Path) -> Option<String> {    unsafe {
        let wpath = wide(&path.to_string_lossy());
        let mut info = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            wpath.as_ptr(),
            0,
            &mut info,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if r == 0 || info.hIcon == 0 {
            return None;
        }
        let hicon = info.hIcon;
        let result = render(hicon, 32);
        DestroyIcon(hicon);
        result
    }
}

unsafe fn render(hicon: isize, size: i32) -> Option<String> {
    let bmi = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: size,
        biHeight: -size,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };

    let screen = GetDC(0);
    let memdc = CreateCompatibleDC(screen);
    if memdc == 0 {
        return None;
    }
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(memdc, &bmi, DIB_RGB_COLORS, &mut bits, 0, 0);
    let mut out = None;

    if hbm != 0 && !bits.is_null() {
        let old = SelectObject(memdc, hbm);
        let len = (size * size * 4) as usize;
        std::ptr::write_bytes(bits as *mut u8, 0, len);
        DrawIconEx(memdc, 0, 0, hicon, size, size, 0, 0, DI_NORMAL);

        let raw = std::slice::from_raw_parts(bits as *const u8, len);
        let mut img = RgbaImage::new(size as u32, size as u32);
        for y in 0..size as u32 {
            for x in 0..size as u32 {
                let i = ((y * size as u32 + x) * 4) as usize;
                let b = raw[i] as u32;
                let g = raw[i + 1] as u32;
                let r = raw[i + 2] as u32;
                let a = raw[i + 3] as u32;
                let (rr, gg, bb) = if a > 0 && a < 255 {
                    (
                        ((r * 255) / a).min(255),
                        ((g * 255) / a).min(255),
                        ((b * 255) / a).min(255),
                    )
                } else {
                    (r, g, b)
                };
                img.put_pixel(x, y, Rgba([rr as u8, gg as u8, bb as u8, a as u8]));
            }
        }

        SelectObject(memdc, old);
        DeleteObject(hbm);

        let mut png: Vec<u8> = Vec::new();
        let mut cursor = Cursor::new(&mut png);
        if DynamicImage::ImageRgba8(img)
            .write_to(&mut cursor, ImageFormat::Png)
            .is_ok()
        {
            out = Some(format!("data:image/png;base64,{}", STANDARD.encode(&png)));
        }
    }

    DeleteDC(memdc);
    ReleaseDC(0, screen);
    out
}
