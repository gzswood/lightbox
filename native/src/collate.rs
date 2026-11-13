//! 名称排序：中文按拼音、英文数字按数值大小比。
//!
//! 用 Windows 自带的 NLS 比较函数，不自己实现排序规则 —— 系统的排序表已经把
//! 「拼音序」和「数字序」做好了，和用户在资源管理器里看到的顺序一致。
//!
//! 旧版（WebView）用的是 `Intl.Collator("zh-Hans-CN", { numeric: true })`，
//! 这里对应到 `CompareStringEx`：locale 固定 zh-CN（拼音序是中文的默认序），
//! `SORT_DIGITSASNUMBERS` 让「第2项」排在「第10项」前面，
//! `NORM_IGNORECASE` 对应旧版的 `sensitivity: "base"`（大小写不敏感）。

use std::cmp::Ordering;

use windows::Win32::Globalization::{
    CompareStringEx, NORM_IGNORECASE, SORT_DIGITSASNUMBERS,
};
use windows::core::PCWSTR;

/// 比较两个名字。返回 `Less` / `Equal` / `Greater`。
pub fn cmp_name(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    // locale 写死中文：不跟系统语言走，免得英文系统上中文名按码位排、
    // 和用户预期差得太远。
    let locale = crate::util::wide("zh-CN");
    let aw: Vec<u16> = a.encode_utf16().collect();
    let bw: Vec<u16> = b.encode_utf16().collect();
    unsafe {
        let r = CompareStringEx(
            PCWSTR(locale.as_ptr()),
            SORT_DIGITSASNUMBERS | NORM_IGNORECASE,
            &aw,
            &bw,
            None,
            None,
            None,
        );
        match r.0 {
            1 => Ordering::Less,    // CSTR_LESS_THAN
            2 => Ordering::Equal,   // CSTR_EQUAL
            3 => Ordering::Greater, // CSTR_GREATER_THAN
            // 比较失败（空串、非法字符等）：退回字节序，保证全序不 panic
            _ => a.cmp(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 数字按数值比() {
        // 「第2项」应该排在「第10项」前面
        assert_eq!(cmp_name("第2项", "第10项"), Ordering::Less);
        assert_eq!(cmp_name("item2", "item10"), Ordering::Less);
    }

    #[test]
    fn 大小写不敏感() {
        assert_eq!(cmp_name("Adobe", "adobe"), Ordering::Equal);
    }

    #[test]
    fn 中文按拼音排() {
        // 拼音：bei < hao → 「北京」在「杭州」前面；a < b → 「阿里」在「百度」前面
        assert_eq!(cmp_name("北京", "杭州"), Ordering::Less);
        assert_eq!(cmp_name("阿里", "百度"), Ordering::Less);
    }

    #[test]
    fn 相同名字相等() {
        assert_eq!(cmp_name("微信", "微信"), Ordering::Equal);
    }
}
