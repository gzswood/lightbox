//! 通用小工具：宽字符串、颜色解析。

/// 字符串 → 以 NUL 结尾的 UTF-16 缓冲
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 宽字符串缓冲 → String（去掉尾部 NUL）
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// `#RRGGBB` / `#RGB` → (r, g, b)
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let t = s.trim().trim_start_matches('#');
    let (r, g, b) = match t.len() {
        3 => {
            let d = |i: usize| -> Option<u8> {
                let v = u8::from_str_radix(&t[i..i + 1], 16).ok()?;
                Some(v * 17)
            };
            (d(0)?, d(1)?, d(2)?)
        }
        6 => (
            u8::from_str_radix(&t[0..2], 16).ok()?,
            u8::from_str_radix(&t[2..4], 16).ok()?,
            u8::from_str_radix(&t[4..6], 16).ok()?,
        ),
        _ => return None,
    };
    Some((r, g, b))
}

/// (r, g, b) → `#RRGGBB`
pub fn hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// 把一段文本放进系统剪贴板（`CF_UNICODETEXT`）。成功返回 true。
///
/// 关于页的「作者 / 微信 / 开源链接」都是一点即复制。剪贴板同一时刻只能被
/// 一个进程打开，所以 `OpenClipboard` 失败要重试几次（别的程序可能正开着）；
/// 交给 `SetClipboardData` 之后那块全局内存归系统管，**不能再 GlobalFree**，
/// 否则会双重释放。
pub fn copy_text(s: &str) -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let buf = wide(s);
    let bytes = buf.len() * 2;
    unsafe {
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !opened {
            return false;
        }
        let ok = (|| -> bool {
            if EmptyClipboard().is_err() {
                return false;
            }
            let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
                return false;
            };
            let p = GlobalLock(hmem);
            if p.is_null() {
                return false;
            }
            std::ptr::copy_nonoverlapping(buf.as_ptr(), p as *mut u16, buf.len());
            let _ = GlobalUnlock(hmem);
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hmem.0))).is_ok()
        })();
        let _ = CloseClipboard();
        ok
    }
}
