//! 与桌面 shell 打交道：桌面目录、文件增删通知、刷新桌面、用 shell 打开东西。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    ShellExecuteW, SHChangeNotify, SHGetKnownFolderPath, FOLDERID_Desktop, SHCNE_CREATE,
    SHCNE_DELETE, SHCNE_UPDATEDIR, SHCNF_PATHW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, SendMessageTimeoutW, SW_SHOWNORMAL, WM_COMMAND,
};

use crate::util::wide;

/// DefView 的「刷新」命令号
const FCIDM_SHVIEW_REFRESH: usize = 0x7103;
/// SendMessageTimeout 的标志：目标窗口无响应就立刻放弃
const SMTO_ABORTIFHUNG: u32 = 0x0002;

/// 上次安排刷新的时刻（毫秒）。用来把连续拖放合并成一次刷新。
static LAST_REFRESH_MS: AtomicU64 = AtomicU64::new(0);


/// 桌面目录（真实路径，OneDrive 重定向也能对上）
pub fn desktop_dir() -> PathBuf {
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Desktop, Default::default(), None) {
            let mut len = 0usize;
            while *p.as_ptr().add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(p.as_ptr(), len);
            let s = String::from_utf16_lossy(slice);
            CoTaskMemFree(Some(p.as_ptr() as *const std::ffi::c_void));
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Desktop")
}

/// 用系统默认程序打开一个路径（文件 / 文件夹）
pub fn shell_open(path: &str) -> bool {
    unsafe {
        let r = ShellExecuteW(
            None,
            &windows::core::HSTRING::from("open"),
            &windows::core::HSTRING::from(path),
            None,
            None,
            SW_SHOWNORMAL,
        );
        r.0 as isize > 32
    }
}

/// 告诉 shell 某个路径消失了（收纳进盒子后，桌面图标要立刻消失）
pub fn notify_gone(path: &Path) {
    let p = wide(&path.to_string_lossy());
    unsafe {
        SHChangeNotify(
            SHCNE_DELETE,
            SHCNF_PATHW,
            Some(p.as_ptr() as *const std::ffi::c_void),
            None,
        );
    }
}

/// 告诉 shell 某个路径出现了（例如从盒子放回桌面）
pub fn notify_appeared(path: &Path) {
    let p = wide(&path.to_string_lossy());
    unsafe {
        SHChangeNotify(
            SHCNE_CREATE,
            SHCNF_PATHW,
            Some(p.as_ptr() as *const std::ffi::c_void),
            None,
        );
    }
}

/// 广播「桌面目录内容变了」，轻量、不打扰用户视野
pub fn notify_desktop_changed() {
    let dir = desktop_dir();
    let p = wide(&dir.to_string_lossy());
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEDIR,
            SHCNF_PATHW,
            Some(p.as_ptr() as *const std::ffi::c_void),
            None,
        );
    }
}

/// 桌面图标视图的窗口句柄（Progman 下面挂着 SHELLDLL_DefView）
fn defview() -> Option<HWND> {
    unsafe {
        let cls_progman = wide("Progman");
        let progman = FindWindowW(
            windows::core::PCWSTR(cls_progman.as_ptr()),
            windows::core::PCWSTR::null(),
        )
        .ok()?;
        // 正常结构：Progman → SHELLDLL_DefView
        let cls_defview = wide("SHELLDLL_DefView");
        if let Ok(dv) = FindWindowExW(
            Some(progman),
            None,
            windows::core::PCWSTR(cls_defview.as_ptr()),
            windows::core::PCWSTR::null(),
        ) {
            return Some(dv);
        }
        // 壁纸软件会把 DefView 挪到某个 WorkerW 底下
        let cls_worker = wide("WorkerW");
        let mut worker = HWND::default();
        loop {
            let Ok(w) = FindWindowExW(
                None,
                Some(worker),
                windows::core::PCWSTR(cls_worker.as_ptr()),
                windows::core::PCWSTR::null(),
            ) else {
                return None;
            };
            worker = w;
            if let Ok(dv) = FindWindowExW(
                Some(worker),
                None,
                windows::core::PCWSTR(cls_defview.as_ptr()),
                windows::core::PCWSTR::null(),
            ) {
                return Some(dv);
            }
        }
    }
}

/// 强制刷新桌面图标（等价于用户手动按 F5）
///
/// 背景：我们已经逐条发过 `SHCNE_DELETE`，但 Explorer 在某些情况下
/// （图标缓存未失效、路径来自网络/OneDrive、拖拽落点是 Progman 而不是 DefView）
/// 就是不处理，桌面上那个图标会一直留着，用户得自己按一次 F5。
/// 这里补两件事：让 DefView 自己走一遍刷新流程 + 广播目录变更。
///
/// 跨进程发消息一定用 `SendMessageTimeoutW` + `SMTO_ABORTIFHUNG`，
/// 直接 SendMessage 会在 Explorer 繁忙时把本进程一起卡住。
pub fn refresh_desktop() {
    unsafe {
        if let Some(dv) = defview() {
            let mut out: usize = 0;
            let _ = SendMessageTimeoutW(
                dv,
                WM_COMMAND,
                WPARAM(FCIDM_SHVIEW_REFRESH),
                LPARAM(0),
                windows::Win32::UI::WindowsAndMessaging::SEND_MESSAGE_TIMEOUT_FLAGS(SMTO_ABORTIFHUNG),
                500,
                Some(&mut out as *mut usize),
            );
        }
    }
    notify_desktop_changed();
}

/// 稍后刷新一次桌面图标。
///
/// 已经逐条发过增删通知了，但 Explorer 有概率不处理（见 `refresh_desktop` 的说明），
/// 图标就一直赖在桌面上。一次刷新很便宜（一次跨进程消息 + 一次目录广播），
/// 所以宁可多刷一次。
///
/// 800ms 内的连续操作合并成一次 —— 这次刷新会把它们一起覆盖掉。
pub fn schedule_refresh() {
    let now = now_ms();
    let last = LAST_REFRESH_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 800 {
        return;
    }
    LAST_REFRESH_MS.store(now, Ordering::Relaxed);
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(300));
        refresh_desktop();
        // 再补一轮轻量广播：Explorer 处理刷新有自己的节奏，
        // 个别机器上图标要等第二次目录变更通知才真的从桌面上抹掉。
        std::thread::sleep(std::time::Duration::from_millis(900));
        notify_desktop_changed();
    });
}

/// 单调时钟毫秒
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

