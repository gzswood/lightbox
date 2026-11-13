//! 长条收纳条：一个顶层分层窗口，横着排 N 个分段，每段是一个收纳盒。
//!
//! 为什么是**顶层**窗口而不是挂在桌面上（Progman/WorkerW）？
//! 阶段 0 的实测结论：Win11 上「分层子窗口」根本不渲染 —— 挂在桌面层里的
//! 窗口没有 alpha 通道，逐像素透明做不出来。顶层窗口则一切正常，
//! 代价是层级要自己管（置顶、Win+D 之后自己恢复）。
//!
//! 代价换来的好处：真·逐像素透明 + 抗锯齿圆角，而且**只有 1 个窗口**，
//! 不再是 N 个盒子各一个 webview（那是原来 800MB 内存的根源）。
//!
//! 形态是**横排胶囊**：一个盒子 = 一段（左边一个图标，右边两个汉字），
//! 整条是**一个**胶囊，嵌在任务栏里。点哪一段就弹出哪个盒子。
//! 新建收纳盒走右键菜单（长条上不再留「+」）。

use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CLOAKED,
    DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::IDropTarget;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetAncestor, GetClassNameW, GetCursorPos, GetDesktopWindow,
    GetShellWindow, GetTopWindow, GetWindow, GetWindowLongPtrW,
    GetWindowPlacement, GetWindowRect, IsIconic, IsWindow, IsWindowVisible, LoadCursorW,
    PostMessageW, RegisterClassW,
    SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    ShowWindow, WindowFromPoint,
    GA_ROOT, GWL_EXSTYLE, GW_HWNDNEXT, GW_HWNDPREV, HWND_BOTTOM, HWND_TOP, HWND_TOPMOST, IDC_ARROW,
    IDC_HAND, MA_NOACTIVATE, SC_MINIMIZE, SIZE_MINIMIZED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_RESTORE,
    SW_SHOWNOACTIVATE, WINDOWPLACEMENT, WM_CONTEXTMENU, WM_ERASEBKGND, WM_LBUTTONDBLCLK,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCALCSIZE,
    WM_NCHITTEST, WM_RBUTTONUP, WM_SHOWWINDOW, WM_SIZE, WM_SYSCOMMAND, WM_TIMER, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WNDCLASSW, HTCLIENT,
};

use crate::app::app;
use crate::config::{BoxConfig, Config};
use crate::geom;
use crate::icons;
use crate::log;
use crate::render::{Canvas, IconBmp};
use crate::util::wide;

const CLASS_NAME: &str = "LightBoxBar";

/// 鼠标离开窗口（windows crate 把它放在 UI::Controls 里，这里直接用值）
const WM_MOUSELEAVE: u32 = 0x02A3;

/// 「桌面留守」看门狗的定时器 id 和周期（毫秒）
const TIMER_WATCHDOG: usize = 1;
const WATCHDOG_MS: u32 = 200;

/// 自己给自己投递的「把长条重新摆正」消息（WM_APP + 1）
const WM_APP_REASSERT: u32 = 0x8000 + 1;

/// 长条离屏幕边缘的最小留白（浮在桌面上时）
const SCREEN_MARGIN: i32 = 24;
/// 桌面模式拖动时，离工作区边缘小于这个距离就吸上去（物理像素）
const SNAP_DIST: i32 = 18;
/// 胶囊左右内边距（同时也是每个分段的内边距，段与段之间会贴得很近）
const PAD_X: i32 = 3;
/// 胶囊上下内边距
const PAD_Y: i32 = 3;
/// 图标最大边长（会再按缩放和任务栏高度收一收）
/// 参考同类软件的任务栏样式：图标略小、文字加粗，整体更清爽
const ICON_MAX: i32 = 26;
/// 图标右侧到文字的间距
const GAP: i32 = 3;
/// 一个分段里文字最多几个字
const LABEL_CHARS: usize = 2;
/// 名称字号 = 任务栏厚度 × 0.30（物理像素）。
/// 这是参考软件（旧网页版 `BoxView.tsx`）的原公式：
/// `fontSize: designH * 0.3`，`designH` 就是任务栏厚度。
const TEXT_RATIO: f32 = 0.30;
/// 参考软件里图标 = 任务栏厚度 × 0.66；桌面模式没有任务栏可量，
/// 就用「文字 ÷ 图标」的同一比例反推，保持两者的相对大小不变。
const ICON_RATIO: f32 = 0.66;

/// 鼠标悬停在哪
#[derive(Clone, Copy, PartialEq)]
enum Hover {
    /// 某个盒子段
    Seg(usize),
}

#[derive(Clone)]
struct Seg {
    id: String,
    icon: Option<IconBmp>,
    /// 这个分段在长条里的横坐标（相对窗口左边）
    x: i32,
    /// 这个分段的宽度
    w: i32,
    /// 分段右侧那两个字（空 = 不显示）
    label: String,
}

struct BarState {
    hwnd: Option<HWND>,
    segs: Vec<Seg>,
    /// 横向滚动：当前第一个可见分段的序号
    scroll: usize,
    /// 当前窗口里放得下几段
    visible: usize,
    /// 悬停在哪
    hover: Option<Hover>,
    /// 鼠标左键是不是还按着
    pressed: bool,
    /// 按下之后有没有移动过（移动过 = 拖动，不是点击）
    drag_moved: bool,
    /// 按下时鼠标的**屏幕**坐标
    ///
    /// 拖动的位移必须按屏幕坐标算：客户端坐标会随着窗口自己移动而变，
    /// 拿它当位移就成了「窗口移动 → 鼠标坐标变 → 窗口再移动」的正反馈，
    /// 整条会来回抖（看起来就是闪烁）。
    press_screen: POINT,
    /// 按下时长条左上角（拖动时的基准，避免越拖越快）
    press_origin: POINT,
    /// 长条左上角（屏幕坐标）
    origin: POINT,
    topmost: bool,
    win_w: i32,
    win_h: i32,
    /// 图标边长
    icon: i32,
    /// 文字字号
    text_px: i32,
    /// 是否嵌在任务栏里
    in_bar: bool,
    /// 第一次排布时才需要把窗口显示出来；之后的重排不许改变显隐
    /// （否则被 Win+D 最小化 / 隐藏后，一次定时重排就又自己冒出来）
    shown_once: bool,
    /// 长条是不是「应该显示」。托盘菜单「隐藏主界面」会把它置 false，
    /// 看门狗看到 false 就不再把它从桌面上拽回来。
    keep: bool,
    /// 「显示桌面」期间是不是已经把它抬到桌面窗口上面了（省得每 200ms 重复抬）
    raised_for_desktop: bool,
    /// Win11 任务栏模式：任务栏自己不可用（被最小化 / 句柄失效）时，
    /// 是不是已经把 owner 摘掉自己撑着了
    pin_detached: bool,
    /// 上一次看门狗是不是判定「被盖住 / 被藏起来」（诊断日志只在状态翻转时记一条）
    was_lost: bool,
    layout_sig: String,
    /// 长条作为拖放目标（把桌面图标拖进某个分段）
    drop_target: Option<IDropTarget>,
}

static BAR: OnceLock<Mutex<BarState>> = OnceLock::new();

// 状态只在主线程里读写，HWND 只是句柄。
unsafe impl Send for BarState {}

fn state() -> &'static Mutex<BarState> {
    BAR.get_or_init(|| {
        Mutex::new(BarState {
            hwnd: None,
            segs: Vec::new(),
            scroll: 0,
            visible: 1,
            hover: None,
            pressed: false,
            drag_moved: false,
            press_screen: POINT::default(),
            press_origin: POINT::default(),
            origin: POINT::default(),
            topmost: false,
            win_w: 0,
            win_h: 0,
            icon: ICON_MAX,
            text_px: 0,
            in_bar: false,
            shown_once: false,
            keep: true,
            raised_for_desktop: false,
            pin_detached: false,
            was_lost: false,
            layout_sig: String::new(),
            drop_target: None,
        })
    })
}

/// 鼠标点到了第几段（绝对序号）。滚动之前的段不算。
fn seg_at(x: i32, y: i32) -> Option<usize> {
    let s = state().lock().unwrap();
    if y < 0 || y >= s.win_h {
        return None;
    }
    for (i, seg) in s.segs.iter().enumerate().skip(s.scroll) {
        if x >= seg.x && x < seg.x + seg.w {
            return Some(i);
        }
    }
    None
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE => LRESULT(0),
        WM_ERASEBKGND => LRESULT(1),
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let x = (lp.0 & 0xffff) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xffff) as i16 as i32;
            on_mouse_move(hwnd, x, y);
            // 登记「鼠标离开」通知：没有它，鼠标快速移出长条后收不到消息，
            // 上一次的悬停高亮就会一直留着。
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            on_mouse_leave(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_mouse_down();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_mouse_up(x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            // 双击不额外处理：单击已经会开面板，双击再来一次等于把它关掉
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            match seg_at(x_of(lp), y_of(lp)) {
                Some(i) => crate::menu::box_menu(i),
                None => crate::menu::bar_menu(),
            }
            LRESULT(0)
        }
        WM_CONTEXTMENU => LRESULT(0),
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xffff) as i16;
            scroll_by(if delta > 0 { -1 } else { 1 });
            LRESULT(0)
        }
        WM_SYSCOMMAND => {
            // 拒绝 SC_MINIMIZE，尽量不要被「显示桌面」收走
            if (wp.0 & 0xfff0) as u32 == SC_MINIMIZE {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_SIZE => {
            // 「显示桌面 / Win+D」会绕过 WM_SYSCOMMAND，直接把顶层窗口最小化。
            // 长条是桌面的一部分，不能跟着消失：收到最小化就立刻无激活地放回来
            // （不抢焦点、不动位置、不改变 z-order），其他窗口照常最小化。
            if wp.0 as u32 == SIZE_MINIMIZED {
                if app().running.load(Ordering::Relaxed) && state().lock().unwrap().keep {
                    log::info("长条收到最小化（Win+D / 显示桌面），无激活地放回来");
                    restore_without_activate(hwnd);
                    repaint(hwnd);
                    watchdog(hwnd);
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_SHOWWINDOW => {
            // 被系统「藏」起来同样要反弹。只弹自己该显示的情况（用户点
            // 「隐藏主界面」时 keep=false，这里不能把它拽回来）。
            //
            // 不能在这个消息里直接 ShowWindow —— 会在 WM_SHOWWINDOW 里递归，
            // 投递一条自己的消息，等这一轮走完再动手。
            if wp.0 == 0 && app().running.load(Ordering::Relaxed) && state().lock().unwrap().keep {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_APP_REASSERT, WPARAM(0), LPARAM(0));
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_APP_REASSERT => {
            reassert(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            if wp.0 == TIMER_WATCHDOG {
                watchdog(hwnd);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn x_of(lp: LPARAM) -> i32 {
    (lp.0 & 0xffff) as i16 as i32
}

fn y_of(lp: LPARAM) -> i32 {
    ((lp.0 >> 16) & 0xffff) as i16 as i32
}

/// 窗口类名（拿不到就给空串）
fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// 这个窗口是不是「桌面」本身（Progman / WorkerW）。
fn is_desktop_window(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    unsafe {
        if hwnd == GetShellWindow() || hwnd == GetDesktopWindow() {
            return true;
        }
    }
    matches!(class_of(hwnd).as_str(), "Progman" | "WorkerW")
}

/// 长条中心点现在归谁？如果归桌面（桌面图标也算），说明长条被「显示桌面」
/// 盖住了。
///
/// 只在长条**不是**置顶的时候才有意义：置顶时中心点当然归长条自己。
/// 用「点在哪」而不是「前台是谁」是有原因的：按 Win+D 之后桌面并不会
/// 拿走前台（前台还是刚才那个程序），只看前台会漏判 —— 上一版就是这么
/// 失效的。
fn covered_by_desktop(hwnd: HWND) -> bool {
    unsafe {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return false;
        }
        let pt = POINT {
            x: (rc.left + rc.right) / 2,
            y: (rc.top + rc.bottom) / 2,
        };
        let hit = WindowFromPoint(pt);
        if hit.0.is_null() {
            return false;
        }
        let root = GetAncestor(hit, GA_ROOT);
        is_desktop_window(if root.0.is_null() { hit } else { root })
    }
}

/// 普通窗口层最顶上那个窗口是不是桌面 —— 也就是「显示桌面 / Win+D」
/// 现在生不生效。
///
/// 桌面被抬起时它就压在普通窗口层的最上面；我们为了不被盖住已经换成
/// 置顶层，所以遍历时要把置顶窗口（含自己）和隐藏窗口跳过。
fn desktop_raised() -> bool {
    unsafe {
        let mut h = GetTopWindow(None).unwrap_or_default();
        // z-order 里可能有各种零星窗口，走 64 个还找不到就认为没抬起来
        for _ in 0..64 {
            if h.0.is_null() {
                break;
            }
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
            if ex & WS_EX_TOPMOST.0 as isize != 0 || !IsWindowVisible(h).as_bool() {
                h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
                continue;
            }
            return is_desktop_window(h);
        }
        false
    }
}

/// 把长条抬到置顶层（「显示桌面」抬起来的桌面在普通层，压不到置顶层）。
fn raise_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW,
        );
    }
}

/// 抬到**置顶带的最前面**（任务栏模式用）。
///
/// 长条在任务栏里时本来就是置顶窗口，再调一次 `HWND_TOPMOST` 对 z-order 是空操作；
/// 要压过「显示桌面」抬起来的桌面，必须再补一次 `HWND_TOP` —— 对置顶窗口来说
/// 它表示「排在置顶带最前」，而不是掉回普通层。
fn raise_topmost_front(hwnd: HWND) {
    raise_topmost(hwnd);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW,
        );
    }
}

/// 把被最小化 / 藏起来的长条放回来，**不抢焦点**。
///
/// 长条挂着 `WS_EX_NOACTIVATE`，`SW_RESTORE` 那种「恢复并激活」会打断用户
/// 正在打字的窗口。`ShowWindow(SW_SHOWNOACTIVATE)` 对最小化的窗口一般不生效，
/// 所以再补一次 `SetWindowPlacement`（showCmd 显式写 SW_SHOWNOACTIVATE）——
/// 这是官方推荐的「恢复但不激活」组合。
///
/// 还不行（Win11 的「显示桌面」会跟这里抢）才退回 `SW_RESTORE`：宁可短暂
/// 抢一下前台，也不能让长条一直看不见。
fn restore_without_activate(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if IsIconic(hwnd).as_bool() {
            let mut wp = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            if GetWindowPlacement(hwnd, &mut wp).is_ok() {
                wp.showCmd = SW_SHOWNOACTIVATE.0 as u32;
                let _ = SetWindowPlacement(hwnd, &wp);
            }
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            log::info("无激活恢复没生效，退回 SW_RESTORE");
        }
    }
}

/// 把长条重新摆正：显示 + 置顶 + 立刻重画。
///
/// 用在「系统刚把长条藏起来」的反弹路径上（见 `WM_SHOWWINDOW`）。
fn reassert(hwnd: HWND) {
    if !app().running.load(Ordering::Relaxed) || !state().lock().unwrap().keep {
        return;
    }
    log::info("长条被系统藏起来，强制重新显示");
    restore_without_activate(hwnd);
    unsafe {
        // SWP_SHOWWINDOW 会连 WS_VISIBLE 一起重新断言一遍 —— 有些「隐式隐藏」
        // （owner 被藏、任务栏重排）只看 IsWindowVisible 是看不出来的。
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW | SWP_SHOWWINDOW,
        );
    }
    repaint(hwnd);
}

/// 任务栏模式（Win11）：任务栏自己不可用时先把 owner 摘掉。
///
/// 贴条是**被任务栏拥有**的窗口。Win11 按 Win+D 时任务栏会被最小化/收走，
/// 被拥有的窗口跟着一起不可见 —— 但 `IsWindowVisible` 仍然是 `true`、
/// `IsIconic` 也是 `false`，看门狗完全看不出来，只有用户点一下任务栏
/// （owner 重新显示）长条才回来，这正是「按了 Win+D 就再也不显示」的成因。
///
/// 所以任务栏不可用时先 `unpin`（摘掉 owner）让它自己撑着，任务栏回来再挂回去。
/// 任务栏只是**被自动隐藏**（没最小化）时不动它，否则自动隐藏任务栏的用户
/// 会看到长条浮在屏幕最下沿。
fn maintain_pin(hwnd: HWND) {
    if !crate::taskbar::is_win11() {
        return;
    }
    let Some(tb) = crate::taskbar::taskbar() else {
        return;
    };
    let detached = state().lock().unwrap().pin_detached;
    let usable = unsafe { IsWindow(Some(tb)).as_bool() && !IsIconic(tb).as_bool() };
    match (usable, detached) {
        (false, false) => {
            crate::taskbar::unpin(hwnd);
            state().lock().unwrap().pin_detached = true;
            log::info("任务栏被最小化/失效：长条先摘掉 owner 自己撑着");
        }
        (true, true) => {
            crate::taskbar::pin(hwnd);
            state().lock().unwrap().pin_detached = false;
            log::info("任务栏恢复：长条重新挂回任务栏");
        }
        _ => {}
    }
}

/// 窗口是不是被 DWM「隐身」了（cloaked）。
///
/// 「显示桌面」/ 任务栏重排时，shell 会把窗口标记成 cloaked：窗口的
/// `WS_VISIBLE` 还在、`IsWindowVisible` 也还是 true，但 DWM 根本不合成它，
/// 屏幕上就是一片空白。这条只用来写诊断日志 —— 我们自己没法取消 cloaked，
/// 但它能证明「按 Win+D 后看不见」到底是哪种情况。
fn cloaked(hwnd: HWND) -> bool {
    let mut v: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut v as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && v != 0
    }
}

/// 长条上面压的是不是任务栏本身。
///
/// 贴条贴在任务栏那一条里，任务栏盖在它头上等于「看不见」。任务栏是置顶窗口，
/// 用户点它 / 系统刷新它都会把它提到置顶带最前面，光靠 `WS_EX_TOPMOST`
/// 挡不住，得把它重新提到置顶带最前。
///
/// 只认任务栏本身：别的置顶窗口（输入法候选框等）压在头上是正常的，
/// 一律抬走会跟输入法打架。
fn covered_by_taskbar(hwnd: HWND) -> bool {
    let Some(tb) = crate::taskbar::taskbar() else {
        return false;
    };
    unsafe {
        let mut h = GetWindow(hwnd, GW_HWNDPREV);
        // 往上走一小段就够了：任务栏是置顶窗口，真压在上面的话几步内就能撞到
        for _ in 0..16 {
            let Ok(cur) = h else { break };
            if cur.0.is_null() {
                break;
            }
            if cur == tb {
                return true;
            }
            h = GetWindow(cur, GW_HWNDPREV);
        }
    }
    false
}

/// 长条现在是不是在置顶层
fn is_topmost(hwnd: HWND) -> bool {
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    ex & WS_EX_TOPMOST.0 as isize != 0
}

/// 「留守」看门狗（每 200ms 一次）。
///
/// 「显示桌面 / Win+D」在不同系统上干的事不一样：
///  · Win10 / 桌面模式：把**桌面窗口抬到普通窗口层的最上面**，把所有普通窗口盖住。
///    这时长条的 `IsWindowVisible` / `IsIconic` 全都正常，前台窗口也还是原来那个
///    程序 —— 按前后台判断会完全失效；
///  · Win11 / 任务栏模式：贴条是顶层窗口（owner = 任务栏），会被一起**最小化或藏起来**，
///    而且再按一次 Win+D 也不会自己回来（要等用户点一下任务栏）。
///
/// 所以这里分两步：先「救活」（藏了/最小化就无激活地放回来），再按模式管层级。
fn watchdog(hwnd: HWND) {
    if !app().running.load(Ordering::Relaxed) {
        return;
    }
    let user_topmost = app().cfg.lock().unwrap().bar.topmost;
    let (in_bar, keep, raised, laid_out) = {
        let s = state().lock().unwrap();
        (s.in_bar, s.keep, s.raised_for_desktop, s.win_w > 0)
    };
    // 用户点了「隐藏主界面」、或者还没排过版（窗口还是 10×10 的初始大小）时别动它
    if !keep || !laid_out {
        return;
    }

    // ① 救活：Win+D 把它藏了 / 最小化了就无激活地放回来。
    //    任务栏模式同样要救 —— 以前这里对 `in_bar` 直接 return，结果一按 Win+D
    //    长条就再也不回来了（Win11 上连按第二次 Win+D 也没用）。
    let hidden = unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() };
    if hidden {
        let first = {
            let mut s = state().lock().unwrap();
            let first = !s.was_lost;
            s.was_lost = true;
            first
        };
        if first {
            log::info(&format!(
                "看门狗：长条不可见（iconic={} cloaked={}），开始无激活恢复",
                unsafe { IsIconic(hwnd).as_bool() },
                cloaked(hwnd)
            ));
        }
        restore_without_activate(hwnd);
        repaint(hwnd);
    } else {
        state().lock().unwrap().was_lost = false;
    }

    if in_bar {
        // ② 任务栏模式：长条压在任务栏之上，本身就是置顶层。
        //
        //    Win11 的「显示桌面」有三种坑，逐一堵：
        //    a. 任务栏被最小化 → 被拥有的贴条跟着不可见（IsWindowVisible 还是 true）；
        //       `maintain_pin` 负责摘掉 owner 让它自己撑着；
        //    b. 桌面被抬到置顶带最前面 → 中心点不再归自己；
        //    c. 任务栏自己被提到置顶带最前面 → 任务栏盖在贴条头上（贴条就在它那条里）。
        //
        //    判据只认「桌面」「任务栏」这两类窗口。不能写成「只要中心点不是自己
        //    就抬」—— Win11 的任务栏内容由 XAML 岛渲染，那个岛有时会出现在
        //    `WindowFromPoint` 结果里，那样每 200ms 都会跟任务栏抢一次 z-order。
        maintain_pin(hwnd);
        // Win10 的长条是任务栏的**子窗口**，z-order 由任务栏管、也压不到任务栏之上，
        // 「被任务栏盖住」这条判据只在 Win11 的顶层贴条上成立。
        let win11 = crate::taskbar::is_win11();
        let lost = !is_topmost(hwnd) || covered_by_desktop(hwnd) || (win11 && covered_by_taskbar(hwnd));
        if lost {
            let first = {
                let mut s = state().lock().unwrap();
                let first = !s.was_lost;
                s.was_lost = true;
                first
            };
            if first {
                log::info(&format!(
                    "看门狗：长条被盖住（topmost={} 桌面={} 任务栏={}），抬到置顶带最前",
                    is_topmost(hwnd),
                    covered_by_desktop(hwnd),
                    covered_by_taskbar(hwnd)
                ));
            }
            raise_topmost_front(hwnd);
        } else {
            state().lock().unwrap().was_lost = false;
        }
        return;
    }

    // 用户本来就选了置顶：层级交给系统，不用我们管
    if user_topmost {
        if raised {
            state().lock().unwrap().raised_for_desktop = false;
        }
        return;
    }
    if !raised {
        if covered_by_desktop(hwnd) {
            raise_topmost(hwnd);
            state().lock().unwrap().raised_for_desktop = true;
        }
        return;
    }
    // 已经为了「显示桌面」置顶了。桌面还在普通层顶部就别动；
    // 顺便确认置顶没被别处（拖动重排、任务栏切换）弄丢。
    if desktop_raised() {
        if !is_topmost(hwnd) {
            raise_topmost(hwnd);
        }
        return;
    }
    // 普通层最上面已经不是桌面了：先降回普通层最底下，再用中心点复查一遍。
    // 还归桌面就说明「显示桌面」其实没结束（z-order 判断不可靠时），抬回去；
    // 不归桌面就说明真的结束了，让它留在桌面上。
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW,
        );
    }
    if covered_by_desktop(hwnd) {
        raise_topmost(hwnd);
    } else {
        state().lock().unwrap().raised_for_desktop = false;
    }
}

/// 建长条窗口
pub fn init() -> Result<(), String> {
    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let cls = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: windows::core::PCWSTR(cls.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).ok().unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| format!("创建长条窗口失败：{e}"))?;

        // 关掉 DWM 画的窗口边框和系统圆角。
        //
        // 长条是分层窗口，形状（圆角胶囊）是我们自己在位图里画出来的。
        // 但 DWM 还会额外给顶层窗口描 1px 边框、并且按自己的半径(16px)裁圆角，
        // 我们的胶囊半径是 20px —— DWM 那圈弧线落在胶囊外面，
        // 在透明角上就露出一根细细的白线（长条底色和任务栏一样时最明显）。
        let none = DWMWA_COLOR_NONE;
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &none as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const DWM_WINDOW_CORNER_PREFERENCE as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );

        state().lock().unwrap().hwnd = Some(hwnd);
        // 长条本身也是放置目标：把桌面上的图标拖到某个分段上就收进那个盒子
        let dt = crate::dnd::register_drop_target(hwnd, crate::dnd::DropMode::Bar);
        state().lock().unwrap().drop_target = dt;
        // 桌面留守看门狗：Win+D 把桌面抬到前面时把长条抬回来
        SetTimer(Some(hwnd), TIMER_WATCHDOG, WATCHDOG_MS, None);
        Ok(())
    }
}

/// 屏幕坐标落在哪个分段上（拖放用）
pub fn box_id_at_screen(sx: i32, sy: i32) -> Option<String> {
    let (origin, segs) = {
        let s = state().lock().unwrap();
        (s.origin, s.segs.clone())
    };
    let idx = seg_at(sx - origin.x, sy - origin.y)?;
    segs.get(idx).map(|s| s.id.clone())
}

/// 这个屏幕点落在长条上吗
///
/// 面板的鼠标钩子靠它判断「这次点击算不算点了面板外面」——
/// 点在长条上不算，长条自己会处理（切盒子 / 收起面板）。
pub fn contains_screen(x: i32, y: i32) -> bool {
    let s = state().lock().unwrap();
    x >= s.origin.x
        && x < s.origin.x + s.win_w
        && y >= s.origin.y
        && y < s.origin.y + s.win_h
}

/// 长条在任务栏里能拖到的范围。
///
/// 返回 (左沿最左能到哪, 右沿最右能到哪)：左边不许越过任务栏左沿
/// （Win10 上还要避开开始按钮和固定图标），右边不许压到托盘图标。
fn drag_limits() -> Option<(i32, i32)> {
    let r = crate::taskbar::strip()?;
    let right = crate::taskbar::tray_left().unwrap_or(r.right - 24) - 4;
    let left = crate::taskbar::left_limit()
        .unwrap_or(r.left + 8)
        .max(r.left + 8);
    Some((left, right))
}

/// 屏幕坐标 → 窗口坐标。
///
/// Win10 上长条是任务栏的**子窗口**，SetWindowPos 要的是相对父窗口的坐标；
/// Win11 上它是顶层窗口，直接用屏幕坐标。
fn to_window_xy(screen_x: i32, screen_y: i32) -> (i32, i32) {
    let child = state().lock().unwrap().in_bar && !crate::taskbar::is_win11();
    if child {
        let org = crate::taskbar::client_origin().unwrap_or(POINT { x: 0, y: 0 });
        (screen_x - org.x, screen_y - org.y)
    } else {
        (screen_x, screen_y)
    }
}

/// 从配置重建分段
pub fn refresh() {
    let cfg: Config = app().cfg.lock().unwrap().clone();
    {
        let mut s = state().lock().unwrap();
        s.topmost = cfg.bar.topmost;
        s.scroll = cfg.bar.scroll.max(0) as usize;
    }
    layout_and_paint();
}

/// 分段图标：自定义图标（PNG data URL）优先，没有就用「首字色块」
fn load_seg_icon(b: &BoxConfig) -> Option<IconBmp> {
    let data = b.icon.as_ref()?;
    icons::decode_png_data_url(data)
}

/// 一段的宽度：左右内边距 + 图标 + 间距 + 文字
fn seg_width(label: &str, icon: i32, text_px: i32) -> i32 {
    if label.is_empty() {
        PAD_X * 2 + icon
    } else {
        let tw = crate::render::bar_text(label, text_px)
            .map(|t| t.w)
            .unwrap_or(0);
        PAD_X * 2 + icon + GAP + tw
    }
}

/// 从第 `from` 段开始，预算内最多能放几段
fn fit_count(segs: &[Seg], from: usize, budget: i32) -> usize {
    let mut used = 0;
    let mut c = 0;
    for s in segs.iter().skip(from) {
        if c > 0 && used + s.w > budget {
            break;
        }
        used += s.w;
        c += 1;
    }
    c
}

/// 这一批段的总宽
fn width_of(segs: &[Seg]) -> i32 {
    segs.iter().map(|s| s.w).sum()
}

/// 算窗口尺寸、位置并重绘
fn layout_and_paint() {
    let cfg = app().cfg.lock().unwrap().clone();
    let hwnd = match state().lock().unwrap().hwnd {
        Some(h) => h,
        None => return,
    };

    // —— 放在哪儿 ——
    // 首选：整条嵌进任务栏（一个胶囊），左端贴着任务栏左侧、右端随内容伸缩。
    // 用户明确要求「以左边为基准」：托盘里的图标/时钟宽度会变，拿右边当基准
    // 长条就会跟着左右跑。任务栏不是底部横条（侧边 / 自动隐藏），或者用户
    // 在设置里选了「桌面模式」时，退回「浮在桌面上」。
    let mut strip = if cfg.bar.desktop {
        None
    } else {
        crate::taskbar::strip()
    };
    // 任务栏里可排的区间 [left, right)
    let mut slot = (0, 0);
    // 长条左沿能到的最左位置（拖动时的边界）
    let mut hard_left = 0;
    if let Some(r) = strip {
        // 右边界：托盘左边。托盘那块装着时钟、网络、音量、输入法 ——
        // 压上去就是遮住用户正在看的东西。
        let right = crate::taskbar::tray_left().unwrap_or(r.right - 24) - 4;
        // 左边界：避开左侧的开始按钮 / 固定图标（Win10 取任务按钮区左沿）。
        // 这个位置只随用户固定/取消固定图标变化，托盘图标增减不影响它。
        hard_left = crate::taskbar::left_limit()
            .unwrap_or(r.left + 8)
            .max(r.left + 8);
        // 胶囊再宽也不许超过任务栏的六成 —— 免得它变成一条横贯的墙
        let max_w = ((r.right - r.left) * 3 / 5).min(right - hard_left);
        if right - hard_left >= 120 {
            // 左端固定贴着任务栏左侧，右端随内容长短伸缩
            slot = (hard_left, hard_left + max_w.max(120));
        } else {
            strip = None; // 任务栏里塞不下 → 浮在桌面上
        }
    }

    let mon = match &strip {
        Some(r) => geom::work_area_at((r.left + r.right) / 2, (r.top + r.bottom) / 2),
        None => geom::work_area_at(cfg.bar.x, cfg.bar.y),
    };
    let scale = (mon.scale as f32 / 96.0).clamp(0.75, 3.0);

    // —— 尺寸 ——
    let (win_h, icon) = match &strip {
        Some(r) => {
            let h = (r.bottom - r.top - 4).max(20);
            let i = (((ICON_MAX as f32) * scale).round() as i32)
                .min(h - 4)
                .max(14);
            (h, i)
        }
        None => {
            let i = (((ICON_MAX as f32) * scale).round() as i32).max(16);
            (i + PAD_Y * 2, i)
        }
    };
    let text_px = match &strip {
        // 参考软件：名称字号 = 任务栏厚度 × 0.30（任务栏厚度就是物理像素，
        // 各缩放比例下都不用再乘一次 scale）
        Some(r) => (((r.bottom - r.top) as f32) * TEXT_RATIO).round() as i32,
        // 桌面模式没有任务栏可量：按参考里「文字 / 图标」的比例换算
        None => ((icon as f32) * (TEXT_RATIO / ICON_RATIO)).round() as i32,
    }
    .max(9);

    // —— 分段 ——
    let mut segs: Vec<Seg> = cfg
        .boxes
        .iter()
        .map(|b| Seg {
            id: b.id.clone(),
            icon: load_seg_icon(b),
            x: 0,
            w: 0,
            label: if cfg.bar_hide_label {
                String::new()
            } else {
                b.name.chars().take(LABEL_CHARS).collect()
            },
        })
        .collect();
    for s in &mut segs {
        s.w = seg_width(&s.label, icon, text_px);
    }

    // —— 可用宽度 ——
    let max_w = match &strip {
        Some(_) => (slot.1 - slot.0).max(80),
        None => (mon.work_width() - SCREEN_MARGIN * 2).max(120),
    };
    // 预算里只留左右内边距（「+」已去掉，新建收纳盒走右键菜单）
    let budget = (max_w - PAD_X * 2).max(40);

    // —— 挑出可见的那几段 ——
    let scroll0 = state().lock().unwrap().scroll.min(segs.len());
    let (sc, count) = if segs.is_empty() {
        (0, 0)
    } else if width_of(&segs) <= budget {
        (0, segs.len())
    } else {
        let c0 = fit_count(&segs, 0, budget);
        let mut sc = scroll0.min(segs.len().saturating_sub(1));
        let mut c = fit_count(&segs, sc, budget);
        // 滚到末尾时窗口会变空：改成停在「最后 c0 段」，只要放得下
        if c < c0 && segs.len() > c0 {
            let alt = segs.len() - c0;
            if width_of(&segs[alt..alt + c0]) <= budget {
                sc = alt;
                c = c0;
            }
        }
        (sc, c)
    };

    // 给可见的段排位置
    let mut x = PAD_X;
    for k in 0..count {
        let i = sc + k;
        segs[i].x = x;
        x += segs[i].w;
    }
    let used = x - PAD_X;
    let win_w = PAD_X * 2 + used;

    // —— 位置 ——
    let (screen_x, screen_y) = match &strip {
        // 左端贴任务栏左侧，右端随内容伸缩。用户拖过的话按拖出来的偏移往右挪，
        // 但不许越过左边界，也不许压到托盘上（宽度挤不下时就停在能放的最右位置）。
        Some(r) => {
            let right = crate::taskbar::tray_left().unwrap_or(r.right - 24) - 4;
            let max_x = (right - win_w).max(hard_left);
            let x = (slot.0 + cfg.bar.drag_dx).clamp(hard_left, max_x);
            (x, r.top + 2)
        }
        None => {
            // 浮在桌面上：配置里记的是左上角；没记过（0,0）就落在
            // 任务栏上边缘（工作区下沿）、屏幕水平中间
            let (mut px, mut py) = (cfg.bar.x, cfg.bar.y);
            if px == 0 && py == 0 {
                px = mon.work.left + (mon.work_width() - win_w) / 2;
                py = mon.work.bottom - win_h;
            }
            let mut rect = RECT {
                left: px,
                top: py,
                right: px + win_w,
                bottom: py + win_h,
            };
            geom::clamp_to_work(&mut rect, &mon.work);
            (rect.left, rect.top)
        }
    };

    // 先把窗口挂进任务栏 / 摘出来，再决定坐标按哪个原点算。
    // 如果 pin 没成功（拿不到任务栏 / SetParent 被拒），就不能再用子窗口坐标，
    // 否则会被摆到屏幕最上方 —— 这时退回屏幕坐标。
    let pinned = if strip.is_some() {
        crate::taskbar::pin(hwnd)
    } else {
        crate::taskbar::unpin(hwnd);
        false
    };
    // Win10 下长条是任务栏的**子窗口**，坐标要相对任务栏客户区；
    // Win11 是顶层窗口（任务栏只当 owner），屏幕坐标直接用。
    let child_of_taskbar = pinned && !crate::taskbar::is_win11();
    let (win_x, win_y) = if child_of_taskbar {
        let org = crate::taskbar::client_origin().unwrap_or(POINT { x: 0, y: 0 });
        (screen_x - org.x, screen_y - org.y)
    } else {
        (screen_x, screen_y)
    };

    {
        let mut s = state().lock().unwrap();
        s.segs = segs;
        s.scroll = sc;
        s.visible = count;
        s.win_w = win_w;
        s.win_h = win_h;
        s.icon = icon;
        s.text_px = text_px;
        s.in_bar = strip.is_some();
        s.origin = POINT {
            x: screen_x,
            y: screen_y,
        };
    }
    // 只有第一次排布负责把窗口显示出来；之后的重排（定时检查、设置应用等）
    // 只改大小/位置，不许改变显隐 —— 否则 Win+D 收起后会被一次重排又拉回来。
    let first_show = {
        let mut s = state().lock().unwrap();
        let f = !s.shown_once;
        s.shown_once = true;
        f
    };
    if sc as i32 != cfg.bar.scroll {
        let mut g = app().cfg.lock().unwrap();
        g.bar.scroll = sc as i32;
        crate::config::save(&g);
    }

    // 先画（UpdateLayeredWindow 会定下大小），再定位 —— 位置最后说了算
    paint(hwnd, win_w, win_h);
    // 正为了「显示桌面」置顶时，重排（拖动、定时检查）不能把置顶弄丢，
    // 否则长条会在拖动过程中瞬间掉回桌面底下
    let raised = state().lock().unwrap().raised_for_desktop;
    unsafe {
        let after = if child_of_taskbar {
            None
        } else if cfg.bar.topmost || strip.is_some() || raised {
            Some(HWND_TOPMOST)
        } else {
            Some(HWND_TOP)
        };
        let flags = if first_show {
            SWP_NOACTIVATE | SWP_SHOWWINDOW
        } else {
            SWP_NOACTIVATE
        };
        let _ = SetWindowPos(hwnd, after, win_x, win_y, win_w, win_h, flags);
    }
}

/// 把长条画到分层窗口上
fn paint(hwnd: HWND, w: i32, h: i32) {
    // 锁的顺序统一成「先 cfg 后 state」，别和别处反着来形成死锁
    let cfg = app().cfg.lock().unwrap().clone();
    let (segs, scroll, hover, icon, text_px) = {
        let s = state().lock().unwrap();
        (
            s.segs.clone(),
            s.scroll,
            s.hover,
            s.icon,
            s.text_px,
        )
    };
    let bg = crate::util::parse_hex(&cfg.box_bg).unwrap_or((0x5A, 0x60, 0x6B));
    // 文字颜色永远由背景色算出来 —— 用户把底板调深调浅都不会撞色
    let fg = crate::theme::text_on(bg);

    let mut canvas = Canvas::new(w, h);
    // 底板：整条一个胶囊（圆角半径 = 高度的一半，看起来就是个药丸）
    let radius = (h as f32 / 2.0).min(20.0);
    canvas.fill_round_rect(0, 0, w, h, radius, bg, 255);

    // 悬停高亮
    if let Some(hv) = hover {
        let (hx, hw) = match hv {
            Hover::Seg(i) => match segs.get(i) {
                Some(s) if i >= scroll => (s.x, s.w),
                _ => (0, 0),
            },
        };
        if hw > 4 {
            canvas.fill_round_rect(hx + 2, 2, hw - 4, h - 4, radius.min(12.0), (255, 255, 255), 26);
        }
    }

    // 每个分段：左边一个图标，右边最多两个汉字
    let icon_y = (h - icon) / 2;
    for seg in segs.iter().skip(scroll) {
        if seg.x <= 0 {
            continue; // 滚出可视区的段不画
        }
        let icon_x = seg.x + PAD_X;
        match &seg.icon {
            Some(img) => {
                let tmp = fit_icon(img, icon);
                canvas.blend_icon(
                    icon_x + (icon - tmp.w) / 2,
                    icon_y + (icon - tmp.h) / 2,
                    &tmp,
                );
            }
            None => draw_default_icon(&mut canvas, icon_x, icon_y, icon),
        }
        if !seg.label.is_empty() {
            // 参考软件的任务栏样式：微软雅黑 600、字号 = 任务栏厚度 × 0.30
            if let Some(txt) = crate::render::bar_text(&seg.label, text_px) {
                canvas.blend_text(icon_x + icon + GAP, (h - txt.h) / 2, &txt.bmp, fg);
            }
        }
    }

    crate::render::push_layered(hwnd, &canvas);
}

/// 图标等比缩放到不超过 size（不放大）
fn fit_icon(src: &IconBmp, size: i32) -> IconBmp {
    if src.w <= 0 || src.h <= 0 || size <= 0 {
        return IconBmp::empty(0, 0);
    }
    let k = (size as f32 / src.w.max(src.h) as f32).min(1.0);
    let dw = ((src.w as f32) * k).round().max(1.0) as i32;
    let dh = ((src.h as f32) * k).round().max(1.0) as i32;
    if dw == src.w && dh == src.h {
        src.clone()
    } else {
        scale_bilinear(src, dw, dh)
    }
}

/// 没有自定义图标时画内置的「收纳盒」图标。
/// 用的是 v0.9.33 前端那枚 BoxGlyph（蓝盒身 + 浅蓝盒盖 + 白卡扣），
/// 不再画「首字色块」—— 色块看不出是收纳盒，而且每个盒子颜色不一样反而更乱。
fn draw_default_icon(canvas: &mut Canvas, x: i32, y: i32, size: i32) {
    canvas.blend_icon(x, y, &icons::box_glyph(size));
}

/// 双线性缩放
fn scale_bilinear(src: &IconBmp, dw: i32, dh: i32) -> IconBmp {
    let mut out = IconBmp::empty(dw.max(0), dh.max(0));
    if dw <= 0 || dh <= 0 || src.w <= 0 || src.h <= 0 {
        return out;
    }
    for y in 0..dh {
        for x in 0..dw {
            let sx = ((x as f32 + 0.5) * src.w as f32 / dw as f32 - 0.5)
                .clamp(0.0, (src.w - 1) as f32) as i32;
            let sy = ((y as f32 + 0.5) * src.h as f32 / dh as f32 - 0.5)
                .clamp(0.0, (src.h - 1) as f32) as i32;
            let i = (sy * src.w + sx) as usize * 4;
            let o = (y * dw + x) as usize * 4;
            out.px[o..o + 4].copy_from_slice(&src.px[i..i + 4]);
        }
    }
    out
}

/// 鼠标悬停在哪
fn hover_at(x: i32, y: i32) -> Option<Hover> {
    seg_at(x, y).map(Hover::Seg)
}

/// 鼠标当前所在的屏幕坐标
fn cursor_pos() -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
}

/// 拖动时的目标位置（屏幕坐标）
///
/// `delta` 是鼠标从按下那一刻起走过的**屏幕**位移。在任务栏里只能左右挪，
/// 纵向锁死，右沿也不许压到托盘图标；浮在桌面上时可以在整块工作区里挪，
/// 靠近边缘就吸过去。
fn drag_target(
    press_origin: POINT,
    delta: POINT,
    win_w: i32,
    win_h: i32,
    in_bar: bool,
) -> POINT {
    let mut nx = press_origin.x + delta.x;
    let mut ny = if in_bar {
        press_origin.y
    } else {
        press_origin.y + delta.y
    };
    if in_bar {
        if let Some((lo, hi)) = drag_limits() {
            // 长条比能放的地方还宽时（lo > hi - 宽），就贴着左边放
            let max_x = (hi - win_w).max(lo);
            nx = nx.clamp(lo, max_x);
        }
        return POINT { x: nx, y: ny };
    }
    // 桌面模式：先把长条夹进光标所在屏的工作区，再看边缘要不要吸。
    // 工作区左/上/右沿就是屏幕边缘，下沿就是任务栏上沿 —— 一套坐标同时覆盖
    // 「屏幕边缘」和「任务栏边缘」两种吸附。夹紧已经覆盖了拖过头的情况，
    // 吸合只负责「还差一点点」的近距离。
    let mon = geom::work_area_at(nx + win_w / 2, ny + win_h / 2);
    let work = mon.work;
    let max_x = (work.right - win_w).max(work.left);
    let max_y = (work.bottom - win_h).max(work.top);
    nx = nx.clamp(work.left, max_x);
    ny = ny.clamp(work.top, max_y);
    if (nx - work.left).abs() <= SNAP_DIST {
        nx = work.left;
    } else if (max_x - nx).abs() <= SNAP_DIST {
        nx = max_x;
    }
    if (ny - work.top).abs() <= SNAP_DIST {
        ny = work.top;
    } else if (max_y - ny).abs() <= SNAP_DIST {
        ny = max_y;
    }
    POINT { x: nx, y: ny }
}

/// 把长条挪到指定位置（屏幕坐标）
fn move_to(hwnd: HWND, p: POINT) {
    unsafe {
        let (wx, wy) = to_window_xy(p.x, p.y);
        // SWP_NOREDRAW：分层窗口的内容是 UpdateLayeredWindow 画上去的，
        // 挪位置时没必要让系统再来一次重绘（重绘就是闪烁的来源之一）。
        let _ = SetWindowPos(
            hwnd,
            None,
            wx,
            wy,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOREDRAW,
        );
    }
}

fn on_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let (pressed, press_screen, press_origin, in_bar, win_w, win_h) = {
        let s = state().lock().unwrap();
        (
            s.pressed,
            s.press_screen,
            s.press_origin,
            s.in_bar,
            s.win_w,
            s.win_h,
        )
    };
    if pressed {
        let cur = cursor_pos();
        let delta = POINT {
            x: cur.x - press_screen.x,
            y: cur.y - press_screen.y,
        };
        let mut s = state().lock().unwrap();
        if delta.x.abs() > 3 || delta.y.abs() > 3 {
            s.drag_moved = true;
        }
        if !s.drag_moved {
            // 还没越过拖动阈值：先别动窗口，否则点一下也会把长条挪走两像素
            return;
        }
        let target = drag_target(press_origin, delta, win_w, win_h, in_bar);
        // 位置没变就别再挪窗口 —— 贴着边界拖的时候每挪一次都是白折腾
        let changed = s.origin.x != target.x || s.origin.y != target.y;
        s.origin = target;
        drop(s);
        if changed {
            move_to(hwnd, target);
        }
        return;
    }
    let rel = hover_at(x, y);
    let changed = {
        let mut s = state().lock().unwrap();
        let c = s.hover != rel;
        s.hover = rel;
        c
    };
    if changed {
        unsafe {
            let _ = SetCursor(
                LoadCursorW(None, if rel.is_some() { IDC_HAND } else { IDC_ARROW }).ok(),
            );
        }
        repaint(hwnd);
    }
}

/// 鼠标离开长条：把悬停高亮清掉
fn on_mouse_leave(hwnd: HWND) {
    let changed = {
        let mut s = state().lock().unwrap();
        s.hover.take().is_some()
    };
    if changed {
        repaint(hwnd);
    }
}

fn repaint(hwnd: HWND) {
    let (w, h) = {
        let s = state().lock().unwrap();
        (s.win_w, s.win_h)
    };
    if w > 0 && h > 0 {
        paint(hwnd, w, h);
    }
}

fn on_mouse_down() {
    let cur = cursor_pos();
    let mut s = state().lock().unwrap();
    s.pressed = true;
    s.drag_moved = false;
    s.press_screen = cur;
    s.press_origin = s.origin;
    drop(s);
    // 抓住鼠标：拖到长条外面去也能收到松手
    let h = { state().lock().unwrap().hwnd };
    if let Some(h) = h {
        unsafe {
            let _ = SetCapture(h);
        }
    }
}

fn on_mouse_up(x: i32, y: i32) {
    unsafe {
        let _ = ReleaseCapture();
    }
    let (was_pressed, moved) = {
        let mut s = state().lock().unwrap();
        let p = s.pressed;
        s.pressed = false;
        (p, s.drag_moved)
    };
    if !was_pressed {
        return;
    }
    if moved {
        // 松手时按当前指针位置再收一次尾，别让最后几像素留在后面
        let (press_origin, press_screen, in_bar, win_w, win_h, hwnd) = {
            let s = state().lock().unwrap();
            (
                s.press_origin,
                s.press_screen,
                s.in_bar,
                s.win_w,
                s.win_h,
                s.hwnd,
            )
        };
        let cur = cursor_pos();
        let delta = POINT {
            x: cur.x - press_screen.x,
            y: cur.y - press_screen.y,
        };
        let target = drag_target(press_origin, delta, win_w, win_h, in_bar);
        {
            let mut s = state().lock().unwrap();
            s.origin = target;
        }
        if let Some(h) = hwnd {
            move_to(h, target);
        }
        // 拖动结束：把新位置写回配置
        let origin = target;
        if in_bar {
            // 记的是「相对贴着任务栏左侧的默认位置往右挪了多少」——
            // 基准取左边，托盘宽度变了（时钟字体、通知图标增减）位置才不会跑偏
            let dx = drag_limits()
                .map(|(lo, _hi)| origin.x - lo)
                .unwrap_or(0)
                .max(0);
            let mut g = app().cfg.lock().unwrap();
            g.bar.drag_dx = dx;
            crate::config::save(&g);
        } else {
            let mut g = app().cfg.lock().unwrap();
            g.bar.x = origin.x;
            g.bar.y = origin.y;
            crate::config::save(&g);
        }
        layout_and_paint();
        return;
    }
    // 点击：看落在哪个分段上
    if let Some(i) = seg_at(x, y) {
        open_panel(i);
        return;
    }
    // 长条空白处：面板开着就收起来
    crate::panel::close();
}

/// 打开某个分段的弹出面板
pub fn open_panel(idx: usize) {
    let (id, seg_x, seg_w, bar) = {
        let s = state().lock().unwrap();
        if idx >= s.segs.len() {
            log::info(&format!("open_panel: 越界 idx={idx}"));
            return;
        }
        (
            s.segs[idx].id.clone(),
            s.segs[idx].x,
            s.segs[idx].w,
            RECT {
                left: s.origin.x,
                top: s.origin.y,
                right: s.origin.x + s.win_w,
                bottom: s.origin.y + s.win_h,
            },
        )
    };
    // 面板已经开着这个盒子 → 再点一次就是收起来
    if crate::panel::is_open_for(&id) {
        crate::panel::close();
        return;
    }
    crate::panel::open(&id, seg_x, seg_w, &bar);
}

/// 滚轮横向滚动
fn scroll_by(delta: i32) {
    let next = {
        let mut s = state().lock().unwrap();
        let n = s.segs.len();
        if n == 0 {
            return;
        }
        let max_scroll = n.saturating_sub(s.visible.max(1));
        let next = (s.scroll as i32 + delta).clamp(0, max_scroll as i32);
        if next as usize == s.scroll {
            return;
        }
        s.scroll = next as usize;
        next
    };
    {
        let mut g = app().cfg.lock().unwrap();
        g.bar.scroll = next;
        crate::config::save(&g);
    }
    // 可见段数变了，宽度也要重排
    layout_and_paint();
}

/// 滚到最后一段（新建盒子后调用，让新盒子露出来）
pub fn scroll_to_end() {
    {
        let mut s = state().lock().unwrap();
        let n = s.segs.len();
        if n == 0 {
            return;
        }
        s.scroll = n - 1;
    }
    layout_and_paint();
}

/// 显示长条（托盘图标单击 / 双击、菜单「显示主界面」都只调它）。
///
/// 只负责「显示」，**不会隐藏**（隐藏走 [`hide`]）。不能因为
/// `IsWindowVisible` 已经是 true 就提前返回：被「显示桌面」盖住的窗口
/// 这两个标志完全正常，可它其实看不见。
pub fn show() {
    let Some(hwnd) = state().lock().unwrap().hwnd else {
        return;
    };
    {
        let mut s = state().lock().unwrap();
        s.keep = true;
        // 这次抬升是用户点名要的，不算「显示桌面」留下的置顶；
        // 若桌面此刻还盖着，等下面的看门狗重新判定并抬到置顶层
        s.raised_for_desktop = false;
    }
    // 先重排一次：拿回正确的大小/位置，顺带把长条抬到普通窗口最上层
    // （「显示桌面」把桌面抬上去了，光这一步还不够，靠看门狗补到置顶层）
    layout_and_paint();

    unsafe {
        // 只有真的被藏起来 / 最小化了才需要抢前台（结束「显示桌面」状态）：
        // 分层窗口平时挂着 WS_EX_NOACTIVATE 永远进不了前台，必须先摘掉，
        // 拿到前台后立刻还回去。只是被桌面盖住时别去偷焦点。
        let restore = !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool();
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let noact = WS_EX_NOACTIVATE.0 as isize;
        let had_noact = ex & noact != 0;
        if restore && had_noact {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !noact);
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        if restore {
            let _ = SetForegroundWindow(hwnd);
        }
        if restore && had_noact {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        }
    }
    // 还被「显示桌面」盖着的话，看门狗会立刻把它补到置顶层
    watchdog(hwnd);
    repaint(hwnd);
}

/// 托盘菜单「隐藏主界面」：收起长条，并告诉看门狗别再把它拽回来。
pub fn hide() {
    let Some(hwnd) = state().lock().unwrap().hwnd else {
        return;
    };
    {
        let mut s = state().lock().unwrap();
        s.keep = false;
        s.raised_for_desktop = false;
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// 每秒滴答：显示器布局或任务栏变了就重新排一次
pub fn on_tick() {
    let mut sig = geom::monitors()
        .iter()
        .map(|m| {
            format!(
                "{},{},{},{}@{}",
                m.work.left,
                m.work.top,
                m.work_width(),
                m.work_height(),
                m.scale
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    // 任务栏的位置/托盘区变了，胶囊的可排范围也跟着变
    if let Some(r) = crate::taskbar::strip() {
        sig.push_str(&format!(
            "|tb:{},{},{},{}:{}:{}",
            r.left,
            r.top,
            r.right,
            r.bottom,
            crate::taskbar::left_limit().unwrap_or(-1),
            crate::taskbar::tray_left().unwrap_or(-1)
        ));
    }
    let changed = {
        let mut s = state().lock().unwrap();
        let c = s.layout_sig != sig;
        s.layout_sig = sig;
        c
    };
    if changed {
        layout_and_paint();
    }
}
