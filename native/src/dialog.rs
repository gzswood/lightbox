//! 系统提示框 / 确认框的统一入口。
//!
//! 两个坑都出在「直接调 `MessageBoxW`」上，散在各处写就一定会翻车：
//!
//! 1. **必须压在置顶面板之上**。面板（任务栏模式下还有长条）是
//!    `WS_EX_TOPMOST` 的分层窗口；`MessageBoxW` 的 owner 传 `None` 时对话框
//!    没有归属，在 z 序里属于普通窗口那一档，一开面板就被盖住 —— 用户只能
//!    看到露在外面的一条白边。显式给 owner 就好：owned window 永远画在
//!    owner 之上，owner 置顶它也置顶。设置页不是置顶窗，那条路要额外把面板
//!    临时降一档，否则弹窗照样被面板压住。
//!
//! 2. **一次只允许一个**。MessageBox 的模态只禁用它自己的 owner，用户换个
//!    入口（长条右键 / 面板右键 / 设置页按钮）还能再叠一个，连点能叠出四五个。
//!    这里用一个 `AtomicBool` 闸门，弹窗还开着时后到的请求直接丢掉
//!    （确认框按「取消」处理，也就是什么都不做）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, MessageBoxW, SetWindowPos, GWL_EXSTYLE, HWND_NOTOPMOST, HWND_TOPMOST, IDOK,
    MB_ICONWARNING, MB_OK, MB_OKCANCEL, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_TOPMOST,
};

/// 已经有一个系统弹窗开着
static OPEN: AtomicBool = AtomicBool::new(false);

/// 闸门的守卫：无论正常返回还是提前 return，离开作用域都会重新打开
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        OPEN.store(false, Ordering::Release);
    }
}

fn acquire() -> Option<Guard> {
    if OPEN.swap(true, Ordering::AcqRel) {
        None
    } else {
        Some(Guard)
    }
}

/// 系统弹窗正开着。
///
/// 面板的鼠标 / 键盘钩子必须靠它让路：弹窗是**面板的附属窗口**（owner = 面板），
/// 用户点弹窗上的「确定」时，钩子看到的是「在面板外面松开了左键」，
/// 于是给面板发 `WM_CLICK_OUTSIDE` —— 面板一销毁，挂在它下面的 MessageBox
/// 跟着被销毁，`MessageBoxW` 直接返回 0（不是 IDOK）。表现就是
/// 「确认框点确定以后毫无反应」，而且只有从面板进来的入口会这样
/// （设置页进来时面板早就关了，钩子根本不存在）。
pub fn is_open() -> bool {
    OPEN.load(Ordering::Acquire)
}

/// owner 的优先级：面板 > 调用方指定 > 长条 > 托盘宿主窗口。
///
/// 面板排第一是有意为之：它一直是 `WS_EX_TOPMOST`，弹窗归它所有才保证画在
/// 它上面 —— 从设置页点「解散收纳盒」时面板通常正开着并压着设置页，
/// 这时若拿设置页当 owner，对话框就会被面板盖掉（原来就是这个毛病）。
fn pick_owner(owner: Option<HWND>) -> Option<HWND> {
    if let Some(p) = crate::panel::hwnd() {
        if is_topmost(p) {
            return Some(p);
        }
    }
    if owner.is_some() {
        return owner;
    }
    if let Some(b) = crate::bar::hwnd() {
        return Some(b);
    }
    *crate::app::app().hwnd.lock().unwrap()
}

fn is_topmost(h: HWND) -> bool {
    unsafe { (GetWindowLongPtrW(h, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST.0 != 0 }
}

/// 面板是不是置顶窗。置顶的面板会盖住非置顶的 owner（设置页）弹出来的对话框，
/// 这种情况得先把面板降一档，弹窗关了再抬回去。
fn panel_covers(owner: Option<HWND>) -> bool {
    let Some(p) = crate::panel::hwnd() else { return false };
    if !is_topmost(p) {
        return false;
    }
    // owner 本身就是置顶窗（面板 / 长条）时不用降：owned window 跟着它一起置顶
    !owner.map(is_topmost).unwrap_or(false)
}

fn lower(h: HWND) {
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

fn raise(h: HWND) {
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 提示框：只有一个「确定」。已经有弹窗开着时直接忽略（不排队、不叠窗）。
pub fn tell(owner: Option<HWND>, msg: &str) {
    let Some(_guard) = acquire() else { return };
    let owner = pick_owner(owner);
    let panel = crate::panel::hwnd();
    let lowered = match (panel, panel_covers(owner)) {
        (Some(p), true) => {
            lower(p);
            true
        }
        _ => false,
    };
    unsafe {
        MessageBoxW(
            owner,
            &windows::core::HSTRING::from(msg),
            &windows::core::HSTRING::from("LightBox"),
            MB_OK | MB_ICONWARNING,
        );
    }
    if let (Some(p), true) = (panel, lowered) {
        raise(p);
    }
}

/// 确认框：点「确定」返回 true；点「取消」、直接关窗、或者已有弹窗开着
/// （说明用户连点了两下）都返回 false —— 一律当作「不动手」。
pub fn ask(owner: Option<HWND>, msg: &str) -> bool {
    let Some(_guard) = acquire() else { return false };
    let owner = pick_owner(owner);
    let panel = crate::panel::hwnd();
    let lowered = match (panel, panel_covers(owner)) {
        (Some(p), true) => {
            lower(p);
            true
        }
        _ => false,
    };
    let ok = unsafe {
        MessageBoxW(
            owner,
            &windows::core::HSTRING::from(msg),
            &windows::core::HSTRING::from("LightBox"),
            MB_OKCANCEL | MB_ICONWARNING,
        ) == IDOK
    };
    if let (Some(p), true) = (panel, lowered) {
        raise(p);
    }
    ok
}

/// 同一个动作在 `window` 内只放行一次。
///
/// 打开资源管理器 / 属性框这类「交给别的进程」的动作不做模态，连点就会开
/// 出好几个窗口，靠这个闸门挡住重复的那几下。
pub fn throttle(action: &str, window: Duration) -> bool {
    static LAST: OnceLock<Mutex<Option<(String, Instant)>>> = OnceLock::new();
    let m = LAST.get_or_init(|| Mutex::new(None));
    let now = Instant::now();
    let mut g = m.lock().unwrap();
    if let Some((key, at)) = g.as_ref() {
        if key == action && now.duration_since(*at) < window {
            return false;
        }
    }
    *g = Some((action.to_string(), now));
    true
}

/// 让面板暂时退出置顶，给系统属性框 / 资源管理器让路。
///
/// 条件是**面板下次被鼠标碰到时自动回到置顶**（见 `panel::restore_topmost`）：
/// 属性框是同步的、关掉就回来；资源管理器是异步的，只能等用户回面板再抬。
pub fn yield_topmost() {
    crate::panel::lower_topmost();
}
