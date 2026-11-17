//! 剪贴板：面板图标的「复制 / 粘贴」。
//!
//! 只认**文件列表**（`CF_HDROP`）这一种格式：复制时把盒子里的 `.lnk` 路径放上去
//! （资源管理器也能直接粘），粘贴时再取回来。
//!
//! 文本 / 图片 / HTML 这些一概不碰 —— 盒子里装的是快捷方式，粘一段文字或一张
//! 截图进来没有任何意义。用户要的就是「只识别快捷方式这类信息」。

use std::path::Path;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Ole::CF_HDROP;

/// 把一个文件列表放进剪贴板。
///
/// 内存块交给系统之后**不能**再自己释放：`SetClipboardData` 成功就归剪贴板所有。
pub fn set_files(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let files: Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
    let hg = crate::dnd::make_hdrop(&files).ok_or_else(|| "分配剪贴板内存失败".to_string())?;
    unsafe {
        OpenClipboard(None).map_err(|e| format!("打开剪贴板失败：{e}"))?;
        // EmptyClipboard 失败也没关系（原本就是空的），接着写
        let _ = EmptyClipboard();
        let r = SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(hg.0)));
        let _ = CloseClipboard();
        if let Err(e) = r {
            // 写失败时这块内存还归我们，得自己还回去，不然每失败一次漏一次
            let _ = GlobalFree(Some(hg));
            return Err(format!("写入剪贴板失败：{e}"));
        }
    }
    Ok(())
}

/// 剪贴板里的文件列表（没有 `CF_HDROP` 就是空）。
///
/// 别的程序正占着剪贴板时会打开失败，这里当作「没有」处理 —— 菜单里那一项
/// 直接是灰的，点不动，不会弹出莫名其妙的错误框。
pub fn files() -> Vec<String> {
    unsafe {
        if OpenClipboard(None).is_err() {
            return Vec::new();
        }
        let out = match GetClipboardData(CF_HDROP.0 as u32) {
            Ok(h) if !h.is_invalid() => crate::dnd::parse_hdrop(HGLOBAL(h.0)),
            _ => Vec::new(),
        };
        let _ = CloseClipboard();
        out
    }
}

/// 剪贴板里**能粘贴进盒子**的条目：只要快捷方式（`.lnk` / `.url`）。
///
/// 盒子只装快捷方式（`config::is_shortcut` 同一套规则），所以别的类型即使
/// 复制进来了也不列出来 —— 粘进去也不会成为盒子里的一个图标。
pub fn pasteable() -> Vec<String> {
    files()
        .into_iter()
        .filter(|p| {
            matches!(
                Path::new(p).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
                Some("lnk") | Some("url")
            )
        })
        .collect()
}

/// 现在有没有东西可粘（右键菜单靠它决定「粘贴」是不是灰的）
pub fn can_paste() -> bool {
    !pasteable().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只有快捷方式算「可粘贴」，文本 / 图片 / 普通文件都不算
    #[test]
    fn 只认快捷方式() {
        let ok = |p: &str| {
            matches!(
                Path::new(p).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
                Some("lnk") | Some("url")
            )
        };
        assert!(ok("C:/Users/x/Desktop/微信.lnk"));
        assert!(ok("C:/Users/x/Desktop/主页.URL"));
        assert!(!ok("C:/Users/x/a.txt"));
        assert!(!ok("C:/Users/x/a.png"));
        assert!(!ok("C:/Users/x/无扩展名"));
    }
}
