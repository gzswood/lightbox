//! 桌面层窗口处理：把 LightBox 的收纳盒窗口挂到 Progman / WorkerW 上，
//! 使其贴在壁纸之上、普通窗口之下，且不会出现在任务栏与 Alt+Tab 中。
#![allow(dead_code)]

use crate::win32::*;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static PINNED: Mutex<Vec<isize>> = Mutex::new(Vec::new());

unsafe fn w(s: &str) -> Vec<u16> {
    wide(s)
}

/// 找到桌面图标所在的宿主窗口（Progman，或被 Wallpaper Engine 之类改挂的 WorkerW）
pub fn desktop_host() -> isize {
    unsafe {
        let cls_progman = w("Progman");
        let progman = FindWindowW(cls_progman.as_ptr(), std::ptr::null());

        if progman != 0 {
            let cls_defview = w("SHELLDLL_DefView");
            let defview = FindWindowExW(progman, 0, cls_defview.as_ptr(), std::ptr::null());
            if defview != 0 {
                let parent = GetParent(defview);
                return if parent != 0 { parent } else { progman };
            }
        }

        // 遍历顶层 WorkerW，找挂着 DefView 的那个
        let cls_worker = w("WorkerW");
        let cls_defview = w("SHELLDLL_DefView");
        let mut worker: isize = 0;
        loop {
            worker = FindWindowExW(0, worker, cls_worker.as_ptr(), std::ptr::null());
            if worker == 0 {
                break;
            }
            let dv = FindWindowExW(worker, 0, cls_defview.as_ptr(), std::ptr::null());
            if dv != 0 {
                return worker;
            }
        }
        progman
    }
}

fn set_toolwindow(hwnd: isize) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & WS_EX_TOOLWINDOW == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW);
        }
    }
}

/// 主任务栏窗口（`Shell_TrayWnd`）。
///
/// 多屏时副屏的任务栏是 `Shell_SecondaryTrayWnd`，本程序暂不支持把盒子嵌到副屏任务栏。
pub fn taskbar() -> isize {
    unsafe { FindWindowW(w("Shell_TrayWnd").as_ptr(), std::ptr::null()) }
}

/// 任务栏托盘区（`TrayNotifyWnd`）的左边界，屏幕物理像素。
///
/// 托盘那块里装着输入法、网络、音量、时钟和「显示桌面」按钮 —— 胶囊排位时
/// 一律不许越过这条线，否则就是把用户正在看的时间/通知盖住了。
///
/// 取不到（Win11 任务栏交给 XAML 岛渲染、自动隐藏、语言版本差异）就返回 `None`，
/// 调用方退回「整条任务栏」，至少不会算出更差的结果。
pub fn taskbar_tray_left() -> Option<i32> {
    unsafe {
        let tb = taskbar();
        if tb == 0 {
            return None;
        }
        let cls = w("TrayNotifyWnd");
        let h = FindWindowExW(tb, 0, cls.as_ptr(), std::ptr::null());
        if h == 0 || IsWindowVisible(h) == 0 {
            return None;
        }
        window_rect(h).map(|r| r.x)
    }
}

/// 是不是 Windows 11（build >= 22000）。结果缓存，进程内只查一次。
///
/// 这个判断决定了「嵌入任务栏」到底怎么实现 —— 两个系统上任务栏根本不是同一种东西：
///  · Win10：`Shell_TrayWnd` 是普通窗口容器，子窗口挂上去既能画出来也能收到鼠标消息；
///  · Win11：任务栏交给 XAML/DirectComposition 岛屿渲染，`Shell_TrayWnd` 只剩个壳。
///    子窗口挂得上去、画得出来，**但鼠标消息全被任务栏的合成层吃掉**：
///    点不动、拖不动、右键也弹不出，只能删配置才救回来（已实测复现）。
///
/// 所以 Win11 走「置顶贴条」：不 SetParent，做顶层窗口贴在任务栏那一条里，
/// 观感和真嵌入一样，交互完全正常。Win10 保持原来的真嵌入，一行不改。
pub fn win11() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| unsafe {
        let mut vi: OSVERSIONINFOW = std::mem::zeroed();
        vi.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        RtlGetVersion(&mut vi) == 0 && vi.dwMajorVersion >= 10 && vi.dwBuildNumber >= 22000
    })
}

/// 窗口当前是不是置顶（`WS_EX_TOPMOST`）。
pub fn is_topmost(hwnd: isize) -> bool {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST != 0 }
}

/// 挂在任务栏里的窗口（hwnd）。必须和桌面层那批分开管：
/// 守护线程不能把它们 repin 回 Progman，否则盒子会被从任务栏里拽出来。
static BAR: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// 这个窗口现在是不是挂在任务栏下面。
///
/// 用来识别「切换中途留下的半成品」：配置里已经是普通盒子（`in_bar = false`），
/// 窗口却还是任务栏的子窗口 —— 它只能显示在任务栏那一条里，摆到工作区就被父窗口
/// 裁掉，用户看到的就是「任务栏里没有、桌面上也没有」。
pub fn parent_is_taskbar(hwnd: isize) -> bool {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    let tb = taskbar();
    tb != 0 && unsafe { GetAncestor(hwnd, GA_PARENT) } == tb
}

/// 把窗口挂进任务栏，成为 `Shell_TrayWnd` 的子窗口。
///
/// 实测（Win10 19045）：子窗口能盖在 `ReBarWindow32` / `TrayNotifyWnd` 之上，任务栏自身
/// 重绘不会盖住它（`WS_CLIPCHILDREN`），把任务栏提到最前也一样可见 ——
/// 因为子窗口和父窗口在同一个 z 带里，不存在「被任务栏挡住」这回事。
pub fn pin_to_taskbar(hwnd: isize) -> bool {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    let tb = taskbar();
    if tb == 0 {
        return false;
    }
    unsafe {
        set_toolwindow(hwnd);
        if win11() {
            // Win11：**绝对不能**挂成任务栏的子窗口（挂上去就点不动、拖不动，见 win11()）。
            // 改成「顶层窗口 + 置顶」贴在任务栏那一条里。
            if GetAncestor(hwnd, GA_PARENT) == tb {
                SetParent(hwnd, 0);
            }
            // 关键一步：把任务栏设成贴条的 **owner**（不是 parent）。
            // 被拥有的窗口永远压在 owner 之上，而且仍然是顶层窗口、鼠标点击正常。
            // 只置顶是不够的：任务栏也是 topmost，点一下任务栏系统就把任务栏提到置顶带
            // 最前面，贴条被盖住（用户看到「点任务栏后贴条消失，过一会儿又冒出来」——
            // 冒出来那下就是守护线程每 2.5s 重新提层）。有了 owner 关系就不会再被盖。
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, tb);
            // 从桌面层名单里划掉：守护线程会把它 repin 回 Progman，那样会被任务栏盖住
            PINNED.lock().unwrap().retain(|h| *h != hwnd);
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let mut l = BAR.lock().unwrap();
            if !l.contains(&hwnd) {
                l.push(hwnd);
            }
            return true;
        }
        SetParent(hwnd, tb);
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        if GetAncestor(hwnd, GA_PARENT) == tb {
            let mut l = BAR.lock().unwrap();
            if !l.contains(&hwnd) {
                l.push(hwnd);
            }
            true
        } else {
            false
        }
    }
}

/// 嵌入任务栏的盒子该多宽。
///
/// 默认（显示名字）= 高（= 任务栏厚度）+ 右侧两个汉字的宽度；
/// 设置里打开「嵌入任务栏时隐藏名称」后就是**正方形**（宽 = 高），只放图标。
///
/// 公式只依赖厚度和一个开关，所以 `heal_bar_box` 自己就能算出期望值，不用外部传参。
/// 0.78 是「两行字 + 左右内边距」按厚度换算的比例（40px 任务栏 → 约 31px）。
pub fn bar_box_w(h: i32, hide_label: bool) -> i32 {
    if hide_label {
        h
    } else {
        h + ((h as f64) * 0.78).round() as i32
    }
}

/// 把「嵌入任务栏」的盒子摆到屏幕坐标 (x, y)，宽 w_px、高 h_px，并裁圆角。
///
/// 尺寸一律按**物理像素**给：任务栏厚度本身就是物理像素，走 `strip_chrome` 的
/// 「逻辑 × 缩放」会在 125% 屏上多绕一圈、多出 1px 误差（区域比窗口小就会被硬裁）。
/// 圆角取厚度的 15%（40px 任务栏 ≈ 6px），和任务栏按钮的观感一致。
pub fn place_bar_box(hwnd: isize, x: i32, y: i32, w_px: i32, h_px: i32, show: bool) {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 || w_px <= 0 || h_px <= 0 {
        return;
    }
    let size_px = h_px;
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let mut ns = style
            & !(WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX);
        ns |= WS_CLIPCHILDREN | WS_CLIPSIBLINGS;
        if show {
            ns |= WS_VISIBLE;
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, ns);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW);

        // Win11 的贴条是**顶层窗口**，必须显式置顶才压得住任务栏；
        // Win10 的子窗口和任务栏同 z 带，保持原来的 SWP_NOZORDER（不自作主张改层级）。
        let (z, mut flags) = if win11() {
            (HWND_TOPMOST, SWP_NOACTIVATE | SWP_FRAMECHANGED)
        } else {
            (HWND_TOP, SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED)
        };
        if show {
            flags |= SWP_SHOWWINDOW;
        }
        let (wx, wy) = to_place_xy(hwnd, x, y);
        SetWindowPos(hwnd, z, wx, wy, w_px, size_px, flags);

        // 位置自纠一次：子窗口坐标换算一旦有偏差就会整块偏移（和 strip_chrome 同理）
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r) != 0 {
            let (ddx, ddy) = (x - r.left, y - r.top);
            if ddx.abs() > 2
                || ddy.abs() > 2
                || (r.right - r.left - w_px).abs() > 1
                || (r.bottom - r.top - size_px).abs() > 1
            {
                let (nx, ny) = to_place_xy(hwnd, x, y);
                SetWindowPos(
                    hwnd,
                    0,
                    nx,
                    ny,
                    w_px,
                    size_px,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        let radius = ((size_px as f64) * 0.15).round().max(2.0) as i32;
        let (fw, fh) = window_rect(hwnd)
            .map(|rr| (rr.w, rr.h))
            .unwrap_or((w_px, size_px));
        set_region(hwnd, fw, fh, radius);

        // Win11 的贴条是**顶层窗口**，系统会给顶层窗口套一圈 1px 描边 + 圆角 + 投影。
        // 它贴在任务栏上沿，那圈描边/投影看上去就像「比任务栏高出一点点」（仔细看才看得出）。
        // Win10 的贴条是任务栏的子窗口，没有这些，所以那边一切正常。
        // 圆角我们自己用窗口区域裁，这里把系统那层外观关掉。
        if win11() {
            let policy: u32 = DWMNCRP_DISABLED;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_NCRENDERING_POLICY,
                &policy as *const u32 as *const c_void,
                4,
            );
            let none = MARGINS {
                cxLeftWidth: 0,
                cxRightWidth: 0,
                cyTopHeight: 0,
                cyBottomHeight: 0,
            };
            DwmExtendFrameIntoClientArea(hwnd, &none);
            let corner: u32 = DWMWCP_DONOTROUND;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const u32 as *const c_void,
                4,
            );
            let border: u32 = DWMWA_COLOR_NONE;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &border as *const u32 as *const c_void,
                4,
            );
        }
    }
}

/// 任务栏里的盒子是否还挂着、尺寸是否跟得上任务栏。返回值表示「修过」。
pub fn heal_bar_box(hwnd: isize) -> bool {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    if (unsafe { GetAsyncKeyState(0x01) } as u16) & 0x8000 != 0 {
        return false;
    }
    let tb = taskbar();
    if tb == 0 {
        return false;
    }
    // 父窗口没了（Explorer 重启）：重新挂进新任务栏。
    // Win11 反过来 —— 贴条必须是**顶层**窗口，挂到任务栏下面就等于废掉（见 win11()）。
    let parent = unsafe { GetAncestor(hwnd, GA_PARENT) };
    if win11() {
        if parent == tb {
            return pin_to_taskbar(hwnd);
        }
        // owner 掉了（Explorer 重启、窗口被重建、别的程序改过层级）→ 重新认任务栏当 owner。
        // 少了这层关系，点一下任务栏贴条就会被盖住。
        if unsafe { GetWindow(hwnd, GW_OWNER) } != tb {
            return pin_to_taskbar(hwnd);
        }
        // 每轮把贴条提回 topmost 带最前：Explorer 重绘任务栏、插拔显示器之后
        // z 序会被打乱，胶囊就被任务栏盖住了（看着像「又消失了」）。
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    } else if parent != tb {
        return pin_to_taskbar(hwnd);
    }
    // 贴条的「可用带」直接问 lib：它已经把 Win11 任务栏顶部那 1px 描边行让开了，
    // 这里若还按裸窗口矩形算，守护线程会把贴条又摆回描边行上。
    let br = match crate::taskbar_rect() {
        Some(r) => r,
        None => return false,
    };
    let r = match window_rect(hwnd) {
        Some(r) => r,
        None => return false,
    };
    // 任务栏高度变了（改缩放、任务栏加高）→ 按新厚度重摆；宽度也要跟着重算
    // （宽度还跟「是否隐藏名称」有关，所以直接问 crate 里那套统一算法）
    //
    // 纵坐标同样要纠：以前只比宽高，位置一旦偏上就会永远偏着。Win10 的贴条是任务栏
    // 子窗口，高出的部分被父窗口裁掉看不出来；Win11 是顶层窗口，偏 2px 就露在
    // 任务栏上沿（用户反馈的「比任务栏稍微高一点点」）。
    let (want_w, want_h) = crate::bar_box_size();
    let want_x = r.x.max(br.x).min((br.x + br.w - want_w).max(br.x));
    if (r.w - want_w).abs() > 1
        || (r.h - want_h).abs() > 1
        || (r.y - br.y).abs() > 1
        || (r.x - want_x).abs() > 1
    {
        place_bar_box(hwnd, want_x, br.y, want_w, want_h, true);
        return true;
    }
    false
}


/// 把窗口挂到桌面层：真嵌成 Progman 的子窗口，层级关系天然正确，
/// Explorer 怎么提层都盖不住它。代价是子窗口没有逐像素 alpha，
/// 盒子只能画实色底板（真透明做不到，见 styles.css 里的说明）。
pub fn pin_to_desktop(hwnd: isize) -> bool {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    // Progman 在系统刚启动 / Explorer 正在重启时可能暂时找不到，等一小会儿
    let mut host = desktop_host();
    for _ in 0..20 {
        if host != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        host = desktop_host();
    }
    if host == 0 {
        return false;
    }
    unsafe {
        set_toolwindow(hwnd);
        // 注意：这里**不要**用 AttachThreadInput 去挂到 explorer 的输入队列。
        // 一旦 explorer 或本进程任一方在等对方，两边会互相卡住（表现为整个桌面像死机）。
        // 实测同权限级别下直接 SetParent 就能成功，跨进程 attach 属于纯粹的风险来源。
        SetParent(hwnd, host);

        // 提到同级窗口最上层（压在桌面图标之上）
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );

        if GetAncestor(hwnd, GA_PARENT) == host {
            register(hwnd);
            true
        } else {
            false
        }
    }
}

/// 恢复成普通顶层窗口（用于「置顶」模式）
pub fn unpin(hwnd: isize) {
    unsafe {
        SetParent(hwnd, 0);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !WS_EX_TOOLWINDOW);
        let mut list = PINNED.lock().unwrap();
        list.retain(|h| *h != hwnd);
    }
}

pub fn register(hwnd: isize) {
    let mut list = PINNED.lock().unwrap();
    if !list.contains(&hwnd) {
        list.push(hwnd);
    }
}

/// 从守护名单里摘掉（窗口即将销毁时用，不要再碰它的父窗口/样式）
pub fn forget(hwnd: isize) {
    let mut list = PINNED.lock().unwrap();
    list.retain(|h| *h != hwnd);
    drop(list);
    BAR.lock().unwrap().retain(|h| *h != hwnd);
}

/// 清掉守护名单里已经死掉的窗口句柄。
///
/// Explorer 重启会把挂在 Progman 下的子窗口一起销毁，那些 hwnd 从此无效；
/// 不清理的话名单只增不减（每个盒子每次重建都会加一条）。守护线程每轮顺手调一次。
pub fn prune_dead() {
    let mut list = PINNED.lock().unwrap();
    list.retain(|h| unsafe { IsWindow(*h) } != 0);
    drop(list);
    let mut bar = BAR.lock().unwrap();
    bar.retain(|h| unsafe { IsWindow(*h) } != 0);
}

/// 当前挂在任务栏里的窗口快照（守护线程用）
pub fn bar_windows() -> Vec<isize> {
    BAR.lock().unwrap().clone()
}

/// 窗口所在显示器的缩放系数（100% = 1.0，125% = 1.25，150% = 1.5）。
///
/// 为什么需要它：Tauri 的 `inner_size(96, 96)` 是**逻辑**尺寸，1.25 倍缩放下 tao 会把窗口
/// 建成 120×120 物理像素；而我们的 `strip_chrome` / `reclip` 一直把 96 当**物理**像素用，
/// 于是 SetWindowPos 又把窗口按 96 物理像素摆了一遍 —— 视口只剩 96/1.25 = 76.8 CSS px，
/// 里面 96px 的图标装不下，整体挤到右下角（就是 125% 缩放下「图标错位」的根因）。
///
/// 现在所有尺寸都按「逻辑值传进来、这里乘系数」，位置仍然是物理像素（GetWindowRect /
/// 鼠标坐标本来就是物理的）。
pub fn scale_of(hwnd: isize) -> f64 {
    unsafe {
        if hwnd != 0 {
            let dpi = GetDpiForWindow(hwnd);
            if dpi > 0 {
                return dpi as f64 / 96.0;
            }
        }
        // 兜底（Win10 1607 之前没有 GetDpiForWindow）：问屏幕 DC 要 LOGPIXELSX
        let hdc = GetDC(0);
        let dpi = if hdc != 0 {
            let d = GetDeviceCaps(hdc, LOGPIXELSX);
            ReleaseDC(0, hdc);
            d
        } else {
            96
        };
        if dpi > 0 {
            dpi as f64 / 96.0
        } else {
            1.0
        }
    }
}

fn px(logical: i32, scale: f64) -> i32 {
    (logical as f64 * scale).round() as i32
}

/// 一块显示器的信息，坐标全是**物理**像素
#[derive(Clone, Copy, Debug)]
pub struct MonInfo {
    /// 工作区（已排除任务栏）
    pub work: crate::Rect,
    /// 整块屏幕（**含**任务栏占的那一条）
    pub full: crate::Rect,
    /// 缩放系数：1.0 = 100%，1.25 = 125%
    pub scale: f64,
}

fn mon_info(h: HMONITOR) -> Option<MonInfo> {
    if h == 0 {
        return None;
    }
    let mut mi = MONITORINFO {
        cb_size: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(h, &mut mi) } == 0 {
        return None;
    }
    let (mut dx, mut dy) = (96u32, 96u32);
    let rc = unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
    let scale = if rc == 0 && dx >= 96 { dx as f64 / 96.0 } else { 1.0 };
    let (l, t, r, b) = (
        mi.rc_work.left,
        mi.rc_work.top,
        mi.rc_work.right,
        mi.rc_work.bottom,
    );
    if r - l <= 0 || b - t <= 0 {
        return None;
    }
    let (ml, mt, mr, mb) = (
        mi.rc_monitor.left,
        mi.rc_monitor.top,
        mi.rc_monitor.right,
        mi.rc_monitor.bottom,
    );
    Some(MonInfo {
        work: crate::Rect { x: l, y: t, w: r - l, h: b - t },
        // 拿不到整屏矩形时退回工作区（等于「没有任务栏」，行为与旧版一致）
        full: if mr - ml > 0 && mb - mt > 0 {
            crate::Rect { x: ml, y: mt, w: mr - ml, h: mb - mt }
        } else {
            crate::Rect { x: l, y: t, w: r - l, h: b - t }
        },
        scale,
    })
}

/// 点所在的显示器（落点不在任何屏上也返回最近的一块）
pub fn monitor_at(x: i32, y: i32) -> Option<MonInfo> {
    mon_info(unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) })
}

/// 系统**主显示器**（显示设置里标「主显示器」的那块，坐标原点一定在它上面）
pub fn primary_monitor() -> Option<MonInfo> {
    mon_info(unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) })
}

/// 位置 (x, y) 所在显示器的缩放系数。
///
/// 盒子和菜单都是 `Progman` 的**子窗口**，而子窗口的 DPI 跟随父窗口（Explorer 是
/// system-DPI aware，永远按主屏 DPI）——`GetDpiForWindow` 因此对它们不可用：
/// 拖到另一块缩放不同的屏上，算出来的窗口物理尺寸还是主屏的，内容超出窗口后被
/// 圆角区域裁掉（用户看到的是「字体下半截不见了」）。这里改成按**位置**查显示器。
pub fn scale_at(x: i32, y: i32) -> f64 {
    monitor_at(x, y).map(|m| m.scale).unwrap_or(1.0)
}

/// 任务栏压在屏幕**下沿**时的厚度（物理像素）。
///
/// 工作区下边界和整屏下边界之间的差就是任务栏；任务栏在顶部/左右侧、或者自动隐藏
/// （工作区 = 整屏）时返回 0 —— 调用方据此判断「允许压在任务栏上」这条规则是否可用。
pub fn bottom_bar_at(x: i32, y: i32) -> i32 {
    match monitor_at(x, y) {
        Some(m) => (m.full.y + m.full.h - (m.work.y + m.work.h)).max(0),
        None => 0,
    }
}

/// 去掉标题栏/边框并把窗口钉到精确的物理像素尺寸。
/// Tauri 的 decorations(false) + inner_size 在此环境下没有完全生效（实测 96→136、客户区带标题栏），
/// 所以这里直接用 Win32 兜底，保证盒子和面板的尺寸、圆角、透明区域与设计一致。
///
/// `radius > 0` → 用窗口区域把矩形裁成圆角（盒子必须这样：它挂在 Progman 下面，
/// 子窗口没有逐像素 alpha，四角会露出 WebView 的白底）；
/// `radius <= 0` → 不裁，圆角完全交给 CSS 的 `border-radius`（顶层窗口的 DWM 合成是
/// 逐像素 alpha 的，Chromium 画出来的圆角自带抗锯齿，比 `CreateRoundRectRgn` 的 1 像素
/// 台阶细腻得多）。
/// 屏幕坐标 → 这个窗口 `SetWindowPos` 需要的坐标系。
///
/// 盒子被 `SetParent` 到 Progman 之后坐标就变成了**父窗口客户区**坐标
/// （实测：`SetWindowPos(2736,960)` 落在屏幕 (816,960)，因为 Progman 的客户区原点
/// 在 (-1920,0)），而项目里所有算位置的地方（配置、吸附、自动排序）用的都是屏幕坐标。
/// 主屏在原点时两者刚好重合，主屏不在原点（笔记本屏摆在主屏左边，x 是负数）就整块差
/// 一个屏宽 —— 表现就是「自动排序把盒子排到另一块屏」「贴边之后跳到另一块屏」。
///
/// 这里必须用 `GetAncestor(GA_PARENT)` 而不是 `GetParent()`：
/// `SetParent` 按 Win32 文档**不会**给窗口补 `WS_CHILD` 样式，于是 `GetParent()` 对盒子
/// 返回 0（看着像顶层窗口），换算被跳过，第一次定位就落到另一块屏上；
/// 后续「按落点所在显示器校正尺寸」又把那一块屏的 DPI 当成盒子的 DPI，尺寸也跟着错。
unsafe fn to_place_xy(hwnd: isize, x: i32, y: i32) -> (i32, i32) {
    let parent = GetAncestor(hwnd, GA_PARENT);
    // 顶层窗口的 GA_PARENT 是桌面窗口，坐标本来就是屏幕坐标
    if parent == 0 || parent == GetDesktopWindow() {
        return (x, y);
    }
    let mut pt = POINT { x: 0, y: 0 };
    if ClientToScreen(parent, &mut pt) == 0 {
        return (x, y);
    }
    (x - pt.x, y - pt.y)
}

/// 只把窗口挪到新位置，**不动尺寸/样式/区域**。
///
/// 专门给「胶囊团」整体拖动用：拖动途中每帧都要摆一次成员窗口，
/// `place_bar_box` / `strip_chrome` 那套（改样式、重设区域、调 DWM 属性）太重，
/// 60Hz 跑下来会闪。落位收尾时再走一次完整的那套。
/// 胶囊团跟随线程专用：坐标**已经**换算成父客户区坐标，异步投递，绝不阻塞。
///
/// 和 `move_box_window` 的区别只有两点，都是为「拖动途中由后台线程摆别人的窗口」准备的：
///  ① 不做 `to_place_xy`（那里面有 `ClientToScreen(parent)`，父窗口是 Explorer 的
///     Progman，跨线程调用要等对方；每帧两次，拖动时就是卡顿/死锁来源）——
///     偏移在拖动开始时算一次就够；
///  ② 带 `SWP_ASYNCWINDOWPOS`：窗口归主线程，主线程此刻正待在系统的窗口拖动模态循环里，
///     同步 `SendMessage` 过去有可能永远等不到派发，整条跟随线程就吊死在那一句上。
pub fn move_box_window_place(hwnd: isize, wx: i32, wy: i32) {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return;
    }
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            wx,
            wy,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        );
    }
}

/// 屏幕坐标 → 父客户区坐标 的固定偏移，**只算一次**。
///
/// 给跟随线程用：拖动开始时对每个成员调一次存起来，之后每帧只做一次加法
/// （`place = screen + off`），不再碰 `ClientToScreen` —— 那个调用要问父窗口
/// （Progman，Explorer 的线程），拖动途中每帧问两次就是卡顿的来源。
pub fn place_offset(hwnd: isize) -> (i32, i32) {
    unsafe { to_place_xy(hwnd, 0, 0) }
}

pub fn move_box_window(hwnd: isize, x: i32, y: i32) {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return;
    }
    unsafe {
        let (wx, wy) = to_place_xy(hwnd, x, y);
        SetWindowPos(
            hwnd,
            0,
            wx,
            wy,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 窗口在**屏幕坐标**下的矩形（物理像素）。
///
/// 盒子的位置一律从这里取：tao 的 `outer_position()` / `onMoved` 事件 payload 拿到的是
/// **WM_MOVE 的 lParam**，也就是父客户区坐标 —— 和屏幕坐标差一整块屏。
/// 之前「拖完松手位置被写进配置」走的就是那条路，配置里存进 2736 这种父坐标，
/// 下次启动再换算一次就又多偏移 1920。
pub fn window_rect(hwnd: isize) -> Option<crate::Rect> {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return None;
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) } == 0 {
        return None;
    }
    if r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    Some(crate::Rect {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    })
}

pub fn strip_chrome(hwnd: isize, x: i32, y: i32, w: i32, h: i32, radius: i32, show: bool) {
    if hwnd == 0 {
        return;
    }
    // w/h/radius 是**逻辑**像素，位置是物理像素
    let (lw, lh, lr) = (w, h, radius);
    // 按**目标位置**所在显示器的缩放换算：盒子是 Progman 的子窗口，
    // 它自己的 DPI 一直跟着父窗口（主屏），不能用来算尺寸。
    let sc = scale_at(x, y);
    let (mut pw, mut ph) = (px(lw, sc), px(lh, sc));
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let mut new_style = style
            & !(WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX);
        new_style |= WS_CLIPCHILDREN | WS_CLIPSIBLINGS;
        if show {
            new_style |= WS_VISIBLE;
        } else {
            new_style &= !WS_VISIBLE;
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
        // WS_EX_TOOLWINDOW：彻底从任务栏和 Alt+Tab 里消失（Tauri 的 skip_taskbar 会被后续样式变更失效）
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW);
        let mut flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED;
        if show {
            flags |= SWP_SHOWWINDOW;
        }
        // 传进来的是屏幕坐标；子窗口要换成父客户区坐标（见 to_place_xy）
        let (mut wx, mut wy) = to_place_xy(hwnd, x, y);
        SetWindowPos(hwnd, 0, wx, wy, pw, ph, flags);

        // 位置 + 尺寸自纠（最多两轮，正常情况第一轮就对）：
        //
        //  - 位置：子窗口的坐标换算一旦有偏差（父客户区原点被 DPI 虚拟化、桌面宿主
        //    换了、系统微调），窗口会整块偏移一块屏。用 GetWindowRect 实测差值再纠一次。
        //  - 尺寸：必须按窗口**真正落在的那块屏**算。以前这里只跟 `pw` 比，
        //    而 `pw` 是移动前那块屏的缩放，一旦位置没落对（上一条的偏差），
        //    尺寸就跟着按错屏的 DPI 写死，后面区域又按正确的屏重设 ——
        //    于是「窗口 100 物理像素、圆角区域只有 80」，右下角 20 像素被裁掉。
        let mut sc_final = sc;
        for _ in 0..2 {
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r) == 0 {
                break;
            }
            let sc2 = scale_at(r.left, r.top);
            sc_final = sc2;
            let (want_w, want_h) = (px(lw, sc2), px(lh, sc2));
            let (ddx, ddy) = (x - r.left, y - r.top);
            let size_off =
                (r.right - r.left - want_w).abs() > 1 || (r.bottom - r.top - want_h).abs() > 1;
            let pos_off = ddx.abs() > 2 || ddy.abs() > 2;
            if !size_off && !pos_off {
                pw = r.right - r.left;
                ph = r.bottom - r.top;
                break;
            }
            wx += ddx;
            wy += ddy;
            pw = want_w;
            ph = want_h;
            SetWindowPos(hwnd, 0, wx, wy, pw, ph, SWP_NOZORDER | SWP_NOACTIVATE);
        }

        // 圆角裁剪：挂到 Progman 之后 DWM 不再对这个窗口单独合成，
        // WebView 的白色背景和窗口标题会从 CSS 圆角的四个角里露出来（就是那层「重影」）。
        // 直接用窗口区域把矩形裁成圆角，四角在合成层就不存在了，视觉上也顺带获得干净边缘。
        //
        // 区域尺寸取**窗口真实大小**，不用我们以为的尺寸：区域比窗口小 = 内容被硬裁
        // （「名字下半截不见了」），区域比窗口大 = 圆角边上露白。两者都只能靠实测避免。
        let (fw, fh) = window_rect(hwnd)
            .map(|r| (r.w, r.h))
            .unwrap_or((pw, ph));
        set_region(hwnd, fw, fh, px(lr, sc_final));
        // 上面被 DWM 拒绝（子窗口没有独立合成）时这两句是无害的兜底
        let policy: u32 = DWMNCRP_DISABLED;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &policy as *const u32 as *const c_void,
            4,
        );
        let none = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 0, cyBottomHeight: 0 };
        DwmExtendFrameIntoClientArea(hwnd, &none);
    }
}

/// 按**物理**像素设置窗口区域；`radius <= 0` 表示不要区域（清成整块矩形）。
fn set_region(hwnd: isize, w: i32, h: i32, radius: i32) {
    unsafe {
        if radius <= 0 {
            SetWindowRgn(hwnd, 0, 1);
            return;
        }
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, radius * 2, radius * 2);
        if rgn != 0 {
            // 成功后区域由系统接管，不能再 DeleteObject
            SetWindowRgn(hwnd, rgn, 1);
        }
    }
}

/// 整窗透明度（0.35 ~ 1.0）。
///
/// **v0.7 起不再用于面板**：面板的「透明度」改成 CSS 背景 alpha ——
/// 这样图标和文字始终是实色，只有底板变透（而且这是毛玻璃能生效的前提：
/// `WS_EX_LAYERED` 窗口上 DWM 模糊无效）。函数留着给以后可能的「整窗淡出」用。
///
/// alpha >= 0.999 时取消 LAYERED：不留无用的图层，避免个别显卡驱动走软件合成。
pub fn set_alpha(hwnd: isize, alpha: f32) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if alpha >= 0.999 {
            if ex & WS_EX_LAYERED != 0 {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !WS_EX_LAYERED);
                SetWindowPos(
                    hwnd,
                    0,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
            return;
        }
        if ex & WS_EX_LAYERED == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED);
        }
        let a = (alpha.clamp(0.15, 1.0) * 255.0).round() as u8;
        SetLayeredWindowAttributes(hwnd, 0, a, LWA_ALPHA);
    }
}

/// 只重设圆角区域，不动样式、位置、Z 序。
///
/// 单独拆出来的原因：窗口刚建好的一小段时间里，tao 应用样式 / WebView2 初始化
/// 有可能把窗口区域改回矩形（表现为「圆角变直角」或圆角变小），
/// 那时再补一次区域即可，不需要把整个样式流程重跑一遍（那会闪一下）。
///
/// 参数是**逻辑**像素（内部乘 DPI 系数）；`radius <= 0` = 不要区域
/// （顶层窗口交给 CSS 圆角，见 strip_chrome 的说明）。
pub fn reclip(hwnd: isize, w: i32, h: i32, radius: i32) {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return;
    }
    // 尺寸按窗口**真实大小**取（w/h 只是拿不到真实尺寸时的兜底），
    // 缩放按窗口当前所在显示器算（不用 GetDpiForWindow：子窗口会拿到父窗口的 DPI）
    match window_rect(hwnd) {
        Some(r) => set_region(hwnd, r.w, r.h, px(radius, scale_at(r.x, r.y))),
        None => {
            let sc = scale_of(hwnd);
            set_region(hwnd, px(w, sc), px(h, sc), px(radius, sc));
        }
    }
}

/// 读窗口当前裁剪区域的外接矩形（物理像素）
unsafe fn region_box(hwnd: isize) -> Option<(i32, i32)> {
    let rgn = CreateRectRgn(0, 0, 0, 0);
    if rgn == 0 {
        return None;
    }
    let mut r = RECT::default();
    let got = if GetWindowRgn(hwnd, rgn) != 0 {
        let ok = GetRgnBox(rgn, &mut r) != 0;
        Some((r.right - r.left, r.bottom - r.top)).filter(|_| ok)
    } else {
        None
    };
    // 这个区域是我们自己建的临时副本，必须删掉（系统那份不归我们管）
    DeleteObject(rgn);
    got
}

/// 守护用：校核「窗口尺寸 / 圆角区域」是否和它所在显示器一致，不一致就地修。
///
/// 为什么需要：盒子挂到 Progman 之后 tao 仍然把它当顶层窗口，**换到缩放不同的屏上时
/// tao 会按自己的逻辑改一次窗口尺寸**（80 逻辑像素 × 新屏缩放），这一次改动不经过
/// 我们的 `strip_chrome`，圆角区域还是旧屏的尺寸 —— 于是区域把窗口裁掉一圈，
/// 视觉上就是「文字下半截不见了」。这里每轮守护顺手核对一次，谁改都补回来。
///
/// `w`/`h`/`radius` 都是**逻辑**像素。返回 true 表示修过。
pub fn sync_geometry(hwnd: isize, w: i32, h: i32, radius: i32) -> bool {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    // 用户正按着左键拖（或者程序自己刚摆好还没落定），别跟它抢
    if (unsafe { GetAsyncKeyState(0x01) } as u16) & 0x8000 != 0 {
        return false;
    }
    let r = match window_rect(hwnd) {
        Some(r) => r,
        None => return false,
    };
    let sc = scale_at(r.x + r.w / 2, r.y + r.h / 2);
    let (want_w, want_h) = (px(w, sc), px(h, sc));
    let size_off = (r.w - want_w).abs() > 1 || (r.h - want_h).abs() > 1;
    let rgn_off = unsafe { region_box(hwnd) }
        .map(|(rw, rh)| (rw - r.w).abs() > 1 || (rh - r.h).abs() > 1)
        // 读不到区域（被清空了）= 圆角丢了，同样要补
        .unwrap_or(true);
    if !size_off && !rgn_off {
        return false;
    }
    if size_off {
        unsafe {
            // 位置也要按父客户区坐标重算（尺寸变了，父坐标不变，但统一走同一条路更稳）
            let (wx, wy) = to_place_xy(hwnd, r.x, r.y);
            SetWindowPos(
                hwnd,
                0,
                wx,
                wy,
                want_w,
                want_h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    let (fw, fh) = if size_off {
        (want_w, want_h)
    } else {
        (r.w, r.h)
    };
    set_region(hwnd, fw, fh, px(radius, sc));
    true
}

/// Explorer 重启后 Progman 会被重建，box 的 owner 就断了 —— 后台守护线程负责检测，
/// 重设 owner 必须回到**创建窗口的那个线程**（主线程）执行，
/// 在子线程里跨线程改窗口层级是未定义行为，也是之前「程序整体卡死」的元凶之一。
///
/// 同一个循环顺手做几何守护（见 `sync_geometry`）：换屏 / 改缩放之后谁动了窗口尺寸，
/// 2.5 秒内补齐尺寸和圆角区域。`box_size` / `box_radius` 是盒子的**逻辑**像素尺寸。
pub fn start_watchdog(app: tauri::AppHandle, box_size: i32, box_radius: i32) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let host = desktop_host();
        let bars = bar_windows();
        if host == 0 && bars.is_empty() {
            continue;
        }
        let mut need: Vec<isize> = Vec::new();
        let mut geom: Vec<isize> = Vec::new();
        for hwnd in PINNED.lock().unwrap().clone() {
            if unsafe { IsWindow(hwnd) } == 0 {
                continue;
            }
            if unsafe { GetAncestor(hwnd, GA_PARENT) } != host {
                need.push(hwnd);
            } else {
                geom.push(hwnd);
            }
        }
        if need.is_empty() && geom.is_empty() && bars.is_empty() {
            continue;
        }
        let _ = app.run_on_main_thread(move || {
            for hwnd in need {
                repin(hwnd);
            }
            for hwnd in geom {
                sync_geometry(hwnd, box_size, box_size, box_radius);
            }
            // 任务栏里的盒子：任务栏重建（Explorer 重启）后要重新挂回去，
            // 任务栏变高变矮（改缩放 / 加高任务栏）后要重新按新厚度摆。
            for hwnd in bars {
                heal_bar_box(hwnd);
            }
        });
    });
}

/// Explorer 重启后 Progman 会被重建，父关系失效 —— 后台守护线程负责检测，
/// 真正的 SetParent 必须回到**创建窗口的那个线程**（主线程）执行，
/// 在子线程里跨线程改窗口父级是未定义行为，也是之前「程序整体卡死」的元凶之一。
pub fn repin(hwnd: isize) {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return;
    }
    let host = desktop_host();
    if host == 0 {
        return;
    }
    unsafe {
        SetParent(hwnd, host);
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 桌面目录（处理 OneDrive 重定向的情况）
pub fn desktop_dir() -> PathBuf {
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_DESKTOP, 0, 0, &mut p);
        if hr == 0 && !p.is_null() {
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(p, len);
            let s = String::from_utf16_lossy(slice);
            CoTaskMemFree(p as *mut c_void);
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
        PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Desktop")
    }
}

pub fn shell_open(path: &str) -> bool {
    unsafe {
        let op = w("open");
        let file = w(path);
        let r = ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        );
        r as isize > 32
    }
}

pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow() }
}

/// 当前前台窗口是不是本进程自己的窗口。
///
/// 用途：面板靠「失焦」自动收起，但点自己的收纳盒也会让面板失焦 ——
/// 那不是用户想收起面板，只是想在同一个盒子/另一个盒子上继续操作。
pub fn foreground_is_self() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == 0 {
            return false;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        pid != 0 && pid == GetCurrentProcessId()
    }
}

/// 告诉 shell 某个路径消失了（文件已被我们移走）。
/// 必须紧跟在移动成功之后调用：Explorer 的桌面视图靠文件监视刷新，
/// 不主动通知的话图标会在桌面上多留约 1 秒，拖拽手感就变成「卡一下」。
pub fn notify_gone(path: &Path) {
    let p = wide(&path.to_string_lossy());
    unsafe {
        SHChangeNotify(
            SHCNE_DELETE,
            SHCNF_PATHW,
            p.as_ptr() as *const c_void,
            std::ptr::null(),
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
            p.as_ptr() as *const c_void,
            std::ptr::null(),
        );
    }
}

/// 桌面图标视图（`SHELLDLL_DefView`）：给它发 `FCIDM_SHVIEW_REFRESH` 就等于在桌面上按 F5。
fn defview() -> isize {
    unsafe {
        let cls_progman = w("Progman");
        let cls_defview = w("SHELLDLL_DefView");
        let progman = FindWindowW(cls_progman.as_ptr(), std::ptr::null());
        if progman != 0 {
            let dv = FindWindowExW(progman, 0, cls_defview.as_ptr(), std::ptr::null());
            if dv != 0 {
                return dv;
            }
        }
        // 壁纸软件会把 DefView 挂到某个 WorkerW 下
        let cls_worker = w("WorkerW");
        let mut worker: isize = 0;
        loop {
            worker = FindWindowExW(0, worker, cls_worker.as_ptr(), std::ptr::null());
            if worker == 0 {
                return 0;
            }
            let dv = FindWindowExW(worker, 0, cls_defview.as_ptr(), std::ptr::null());
            if dv != 0 {
                return dv;
            }
        }
    }
}

/// 只广播「桌面目录内容变了」，轻量、不打扰用户视野。
pub fn notify_desktop_changed() {
    let dir = desktop_dir();
    let p = wide(&dir.to_string_lossy());
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEDIR,
            SHCNF_PATHW,
            p.as_ptr() as *const c_void,
            std::ptr::null(),
        );
    }
}

/// 强制刷新桌面图标（等价于用户手动按 F5）。
///
/// 为什么必须这么干：我们已经逐条发过 `SHCNE_DELETE`，但 Explorer 在某些情况下
/// （图标缓存未失效、路径来自网络/OneDrive、拖拽落点是 Progman 而不是 DefView）
/// 就是不处理，桌面上那个图标会一直留着，用户得自己按一次 F5。
/// 这里补两件事：让 DefView 自己走一遍刷新流程 + 广播目录变更。
///
/// 注意：跨进程发消息一定用 `SendMessageTimeoutW` + `SMTO_ABORTIFHUNG`，
/// 直接 SendMessage 会在 Explorer 繁忙时把本进程一起卡住。
pub fn refresh_desktop() {
    unsafe {
        let dv = defview();
        if dv != 0 {
            let mut out: usize = 0;
            SendMessageTimeoutW(
                dv,
                WM_COMMAND,
                FCIDM_SHVIEW_REFRESH,
                0,
                SMTO_ABORTIFHUNG,
                500,
                &mut out,
            );
        }
    }
    notify_desktop_changed();
}
