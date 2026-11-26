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

