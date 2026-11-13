//! 任务栏：找到它、量出它、把长条贴进去。
//!
//! 「嵌入任务栏」在两个系统上是两件完全不同的事：
//!  · Win10：`Shell_TrayWnd` 是普通窗口容器，子窗口挂上去既能画出来也能收到鼠标消息；
//!  · Win11：任务栏交给 XAML / DirectComposition 岛屿渲染，`Shell_TrayWnd` 只剩个壳。
//!    子窗口挂得上去、画得出来，**但鼠标消息全被任务栏的合成层吃掉**：
//!    点不动、拖不动、右键也弹不出来，只能删配置才救回来（老版本实测复现）。
//!
//! 所以 Win11 走「置顶贴条」：不 SetParent，做顶层窗口贴在任务栏那一条里，
//! 并把任务栏设成贴条的 **owner**（不是 parent）—— 被拥有的窗口永远压在 owner 之上，
//! 而且仍然是顶层窗口、鼠标点击正常。只置顶是不够的：任务栏自己也是 topmost，
//! 点一下任务栏系统就把它提到置顶带最前面，贴条会被盖住。

use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetAncestor, GetWindowRect, SetParent, SetWindowLongPtrW,
    GA_PARENT, GWLP_HWNDPARENT,
};

use crate::util::wide;

/// 是不是 Windows 11（build >= 22000）。结果缓存，进程内只查一次。
///
/// 用注册表里的 `CurrentBuildNumber` 判断，比 `GetVersionEx` 可靠：
/// 后者在有兼容性清单的进程里会被「谎报」成旧版本。
pub fn is_win11() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| build_number() >= 22000)
}

/// Windows 构建号（读不到就当 0）
fn build_number() -> u32 {
    unsafe {
        let path = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
        let mut hkey = HKEY::default();
        let rc = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            windows::core::PCWSTR(path.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut hkey,
        );
        if rc != windows::Win32::Foundation::ERROR_SUCCESS {
            return 0;
        }
        let name = wide("CurrentBuildNumber");
        let mut buf = [0u16; 32];
        let mut cb: u32 = (buf.len() * 2) as u32;
        let mut ty = REG_VALUE_TYPE::default();
        let rc = RegQueryValueExW(
            hkey,
            windows::core::PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut cb),
        );
        let _ = RegCloseKey(hkey);
        if rc != windows::Win32::Foundation::ERROR_SUCCESS {
            return 0;
        }
        let len = (cb as usize / 2).min(buf.len());
        let end = buf[..len].iter().position(|c| *c == 0).unwrap_or(len);
        String::from_utf16_lossy(&buf[..end]).trim().parse().unwrap_or(0)
    }
}

/// 主任务栏窗口（`Shell_TrayWnd`）
pub fn taskbar() -> Option<HWND> {
    unsafe {
        let cls = wide("Shell_TrayWnd");
        FindWindowW(
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
        )
        .ok()
    }
}

/// 任务栏托盘区（`TrayNotifyWnd`）的左边界，屏幕物理像素。
///
/// 托盘那块里装着输入法、网络、音量、时钟和「显示桌面」按钮 —— 胶囊排位时
/// 一律不许越过这条线，否则就是把用户正在看的时间/通知盖住了。
///
/// 取不到（Win11 任务栏交给 XAML 岛渲染、自动隐藏、语言版本差异）就返回 `None`，
/// 调用方退回「整条任务栏」，至少不会算出更差的结果。
pub fn tray_left() -> Option<i32> {
    unsafe {
        let tb = taskbar()?;
        let cls = wide("TrayNotifyWnd");
        let h = FindWindowExW(
            Some(tb),
            None,
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
        )
        .ok()?;
        if !windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(h).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(h, &mut r).ok()?;
        Some(r.left)
    }
}

/// 任务栏里「任务按钮区」的矩形（固定图标 + 正在运行的程序按钮）。
///
/// Win10：任务按钮都在 `Shell_TrayWnd` → `ReBarWindow32` → `MSTaskSwWClass` 里。
/// Win11 的 XAML 任务栏没有这套子窗口，取不到就返回 `None`。
fn task_buttons_rect() -> Option<RECT> {
    unsafe {
        let tb = taskbar()?;
        let cls_bar = wide("ReBarWindow32");
        let rebar = FindWindowExW(
            Some(tb),
            None,
            windows::core::PCWSTR(cls_bar.as_ptr()),
            windows::core::PCWSTR::null(),
        )
        .ok()?;
        let cls_task = wide("MSTaskSwWClass");
        let h = FindWindowExW(
            Some(rebar),
            None,
            windows::core::PCWSTR(cls_task.as_ptr()),
            windows::core::PCWSTR::null(),
        )
        .ok()?;
        if !windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(h).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(h, &mut r).ok()?;
        Some(r)
    }
}

/// 任务栏里「胶囊能贴的最左边界」（避开左侧的开始按钮 / 固定图标）。
///
/// Win10：任务按钮区（`MSTaskSwWClass`）的左沿正好是「开始按钮 + 固定图标」的右边界。
/// 胶囊排到它左边就会盖住开始菜单。Win11 的 XAML 任务栏没有这套子窗口，
/// 拿不到就返回 `None`（调用方退回任务栏左沿 —— Win11 开始按钮居中，
/// 左侧基本没有固定图标，贴到那里是安全的）。
pub fn left_limit() -> Option<i32> {
    task_buttons_rect().map(|r| r.left)
}

/// 任务栏占的那一条矩形（物理像素）。
///
/// 只有「确实贴在屏幕底部、横跨整屏」的任务栏才算数 —— 侧边任务栏、
/// 自动隐藏（被推到屏幕外）的都不成立，这时长条不能往里嵌。
pub fn strip() -> Option<RECT> {
    let tb = taskbar()?;
    let mut r = RECT::default();
    unsafe {
        GetWindowRect(tb, &mut r).ok()?;
    }
    let mon = crate::geom::monitors()
        .into_iter()
        .find(|m| {
            r.left >= m.rect.left && r.right <= m.rect.right && r.bottom <= m.rect.bottom
        })
        .or_else(|| {
            crate::geom::monitors()
                .into_iter()
                .find(|m| r.left < m.rect.right && r.right > m.rect.left)
        })?;
    let full_bottom = mon.rect.bottom;
    let work_bottom = mon.work.bottom;
    // ① 贴屏幕底（自动隐藏时任务栏会被推到屏幕外，这里就不成立）
    if (r.bottom - full_bottom).abs() > 1 {
        return None;
    }
    // ② 上沿正好是工作区下沿（确实是占了屏幕底部那一条）
    if (r.top - work_bottom).abs() > 1 {
        return None;
    }
    // ③ 横跨整屏（侧边任务栏宽度不对）
    if r.right - r.left < (mon.rect.right - mon.rect.left) * 9 / 10 {
        return None;
    }
    // Win11：`Shell_TrayWnd` 的窗口矩形比它**画出来的那条**高 1px —— 顶上那一行是
    // 任务栏自己的描边（实测：任务栏 71..118，其中 71 是 1px 灰描边，72..118 才是底）。
    // 贴条压到描边行上，就会在任务栏上沿露出一条、看着「比任务栏高一点点」。
    // 所以把可用带整体下移 1px（高度也少 1px，底边不动）。Win10 没有这行描边。
    let d = if is_win11() { 1 } else { 0 };
    Some(RECT {
        left: r.left,
        top: r.top + d,
        right: r.right,
        bottom: r.bottom,
    })
}

/// 任务栏客户区原点的屏幕坐标
///
/// Win10 下长条是任务栏的**子窗口**，坐标是相对客户区的；
/// 换算成屏幕坐标要知道客户区 (0,0) 在屏幕的哪儿。
pub fn client_origin() -> Option<POINT> {
    let tb = taskbar()?;
    let mut p = POINT::default();
    unsafe {
        if !ClientToScreen(tb, &mut p).as_bool() {
            return None;
        }
    }
    Some(p)
}

/// 把窗口挂进任务栏。
///
/// Win10：真嵌入（`SetParent` 成 `Shell_TrayWnd` 的子窗口）。
/// Win11：贴条 —— 顶层窗口 + 任务栏当 owner + 置顶。
///
/// 返回 false 表示这条路走不通（调用方该退回「浮在桌面上」）。
pub fn pin(hwnd: HWND) -> bool {
    let Some(tb) = taskbar() else {
        return false;
    };
    unsafe {
        if is_win11() {
            // Win11：**绝对不能**挂成任务栏的子窗口（挂上去就点不动、拖不动）。
            if GetAncestor(hwnd, GA_PARENT) == tb {
                let _ = SetParent(hwnd, None);
            }
            // 关键一步：把任务栏设成贴条的 **owner**（不是 parent）。
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, tb.0 as isize);
            true
        } else {
            if SetParent(hwnd, Some(tb)).is_err() {
                return false;
            }
            GetAncestor(hwnd, GA_PARENT) == tb
        }
    }
}

/// 把窗口从任务栏里摘出来（变回普通顶层窗口）
pub fn unpin(hwnd: HWND) {
    unsafe {
        if is_win11() {
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, 0);
        } else if GetAncestor(hwnd, GA_PARENT) == taskbar().unwrap_or_default() {
            let _ = SetParent(hwnd, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 构建号读得到() {
        // 读不到也别 panic —— 只是拿不到 Win11 判断
        let _ = build_number();
    }

    #[test]
    fn 任务栏能定位() {
        // 本机必然有任务栏
        assert!(taskbar().is_some());
    }
}
