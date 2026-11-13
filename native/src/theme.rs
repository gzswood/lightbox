//! 颜色工具：HSV 互转、对比度、按背景自动取文字色。
//!
//! 一条硬规则：**文字颜色永远由背景色算出来**（取深/浅里对比度更高的一方）。
//! 用户把背景调成深色还是浅色，标题、图标名、按钮文字都不会和背景撞色 ——
//! 这也是「自定义背景色」能放开给用户的前提。

/// 相对亮度（WCAG），0 = 纯黑，1 = 纯白
pub fn rel_lum(c: (u8, u8, u8)) -> f32 {
    let f = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.0) + 0.7152 * f(c.1) + 0.0722 * f(c.2)
}

/// 对比度（WCAG），1:1 ~ 21:1
pub fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let la = rel_lum(a);
    let lb = rel_lum(b);
    let hi = la.max(lb);
    let lo = la.min(lb);
    (hi + 0.05) / (lo + 0.05)
}

/// 深色字（近黑）
pub const INK: (u8, u8, u8) = (0x1B, 0x1B, 0x1F);
/// 浅色字（近白）
pub const SNOW: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

/// 这块背景上该用深色字还是浅色字：取对比度高的那一方
pub fn text_on(bg: (u8, u8, u8)) -> (u8, u8, u8) {
    if contrast_ratio(bg, SNOW) > contrast_ratio(bg, INK) {
        SNOW
    } else {
        INK
    }
}

/// 在背景色上放一个「次级文字」色：从背景往文字色方向收 35%
pub fn muted_on(bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let fg = text_on(bg);
    mix(bg, fg, 0.62)
}

/// 强调色（链接、开关打开态）。
///
/// 底色深的时候用亮蓝，底色浅的时候用深蓝 —— 两种都得看得清。
pub fn accent_on(bg: (u8, u8, u8)) -> (u8, u8, u8) {
    if rel_lum(bg) > 0.45 {
        (0x2F, 0x6F, 0xEB)
    } else {
        (0x6C, 0xA8, 0xFF)
    }
}

/// 两个颜色按比例混合（t=0 全 a，t=1 全 b）
pub fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// 正在改哪个底板色
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ColorTarget {
    /// 收纳盒（长条）的底板色
    Box,
    /// 弹出面板的底板色
    Panel,
}

impl ColorTarget {
    pub fn label(self) -> &'static str {
        match self {
            ColorTarget::Box => "收纳盒",
            ColorTarget::Panel => "弹出窗口",
        }
    }
}

/// RGB → HSV（h 0..360，s/v 0..1）
pub fn rgb_to_hsv(c: (u8, u8, u8)) -> (f32, f32, f32) {
    let r = c.0 as f32 / 255.0;
    let g = c.1 as f32 / 255.0;
    let b = c.2 as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let mut h = 0.0;
    if d > 0.0 {
        if max == r {
            h = ((g - b) / d).rem_euclid(6.0);
        } else if max == g {
            h = (b - r) / d + 2.0;
        } else {
            h = (r - g) / d + 4.0;
        }
        h *= 60.0;
    }
    (h, if max > 0.0 { d / max } else { 0.0 }, max)
}

/// HSV → RGB（h 0..360，s/v 0..1）
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 文字色跟着背景翻面() {
        assert_eq!(text_on((0, 0, 0)), SNOW);
        assert_eq!(text_on((255, 255, 255)), INK);
        // 中灰：两边对比度接近，取深色字（和旧版 React 一致）
        assert_eq!(text_on((0x5A, 0x60, 0x6B)), SNOW);
    }

    #[test]
    fn hsv_往返一圈不变色() {
        for c in [(0x5A, 0x60, 0x6B), (255, 0, 0), (0, 128, 255), (0, 0, 0)] {
            let (h, s, v) = rgb_to_hsv(c);
            let back = hsv_to_rgb(h, s, v);
            let back = [back.0, back.1, back.2];
            let orig = [c.0, c.1, c.2];
            for i in 0..3 {
                assert!(
                    (back[i] as i32 - orig[i] as i32).abs() <= 1,
                    "{orig:?} → {back:?}"
                );
            }
        }
    }

    #[test]
    fn 纯红在hsv里是色相0() {
        let (h, s, v) = rgb_to_hsv((255, 0, 0));
        assert!(h.abs() < 0.01 || (h - 360.0).abs() < 0.01);
        assert!((s - 1.0).abs() < 0.01);
        assert!((v - 1.0).abs() < 0.01);
    }
}
