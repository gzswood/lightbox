//! 选图标的系统文件对话框（只要 .ico）。
//!
//! 不走 tauri-plugin-dialog：为了一个「选文件」再拉一个插件（多一份依赖 + 一套前端 IPC）
//! 不值当，comdlg32 的 `GetOpenFileNameW` 就够。注意必须**在主线程**调用：
//! Explorer 风格的对话框要用 shell 命名空间（COM），而主线程是唯一初始化过 COM 的线程。
use crate::win32::*;
use std::path::PathBuf;

/// 弹「选择图标」对话框，返回选中的路径（取消返回 None）
///
/// 过滤器和 `lpstrDefExt` 都只给 .ico —— 这是第一道过滤；
/// 用户手打文件名绕过过滤器时，调用方还会再校验一次扩展名。
pub fn pick_ico(owner: isize, title: &str) -> Option<PathBuf> {
    // 注意结尾两个 0：filter 组之间一个 0，整串结尾再来一个 0
    let filter: Vec<u16> = "图标文件 (*.ico)\0*.ico\0所有文件 (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let mut buf = vec![0u16; 1024];
    let title_w = wide(title);
    let def_ext = wide("ico");

    let mut ofn = OPENFILENAMEW::default();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrTitle = title_w.as_ptr();
    ofn.lpstrDefExt = def_ext.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY
        | OFN_NOCHANGEDIR | OFN_EXPLORER;

    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// 扩展名是不是 .ico（大小写不敏感）
pub fn is_ico(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ico"))
        .unwrap_or(false)
}
