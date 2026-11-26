//! 应用状态与主循环。
//!
//! 一个隐藏的顶层窗口承载托盘图标和所有全局消息；真正可见的窗口
//! （长条、面板）各自是独立的顶层窗口。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, PostQuitMessage, RegisterClassW,
    SetTimer, TranslateMessage, TrackPopupMenuEx, MF_CHECKED, MF_SEPARATOR,
    MF_STRING, MSG, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_COMMAND, WM_DESTROY, WM_DISPLAYCHANGE, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSW,
};

use crate::config::Config;
use crate::tray::Tray;
use crate::{autostart, config, desktop, geom, log};

const CLASS_NAME: &str = "LightBoxHost";

/// 托盘菜单项 id
const IDM_SHOW_BAR: usize = 100;
const IDM_HIDE_BAR: usize = 101;
const IDM_AUTOSTART: usize = 102;
const IDM_DATA_DIR: usize = 103;
const IDM_QUIT: usize = 105;
const IDM_SETTINGS: usize = 106;
const IDM_CHECK_UPDATE: usize = 107;

pub struct App {
    pub cfg: Mutex<Config>,
    pub hwnd: Mutex<Option<HWND>>,
    pub tray: Mutex<Option<Tray>>,
    pub running: AtomicBool,
}

static APP: OnceLock<App> = OnceLock::new();

// 全部状态只在主线程里读写（低级鼠标钩子也在安装它的线程上回调），HWND 只是句柄。
unsafe impl Send for App {}
unsafe impl Sync for App {}

pub fn app() -> &'static App {
    APP.get_or_init(|| App {
        cfg: Mutex::new(Config::default()),
        hwnd: Mutex::new(None),
        tray: Mutex::new(None),
        running: AtomicBool::new(true),
    })
}

/// 隐藏窗口的消息处理
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        crate::tray::WM_TRAY => {
            // NOTIFYICON_VERSION_4：事件在 lParam 低 16 位，图标 id 在高 16 位
            let event = (lparam.0 & 0xffff) as u32;
            match event {
                // 托盘图标只负责「显示」：单击、双击都不会再把长条收起
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => crate::bar::show(),
                WM_RBUTTONUP => show_tray_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as usize;
            if id != 0 {
                on_menu(id);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            ensure_tray();
            // 顺序要紧：先让长条那边认领「显示器变了」这件事（进入过渡期），
            // 再检查任务栏有没有换窗口 —— 否则切屏那一秒里任务栏刚被重建，
            // 下面的 revive() 会立刻去 SetParent，正好撞在外壳最忙的时候。
            crate::bar::on_tick();
            check_taskbar_restart();
            LRESULT(0)
        }
        // 切屏 / 插拔显示器：这里只记一个标记，真正的活留给下一秒的 tick。
        // 在窗口过程里当场重排 / 重挂任务栏会和正忙的外壳线程互等（主界面卡死）。
        WM_DISPLAYCHANGE => {
            log::info(&format!(
                "显示器变化：wParam={}（每像素位数），lParam=({},{})",
                wparam.0,
                (lparam.0 & 0xffff) as u32,
                ((lparam.0 >> 16) & 0xffff) as u32
            ));
            crate::bar::on_display_change();
            LRESULT(0)
        }
        crate::updater::WM_UPDATE_DONE => {
            crate::updater::on_done(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 托盘图标补装。
///
/// 启动时任务栏可能还没准备好（比如资源管理器刚崩过、正被系统重启），
/// 这时 `Shell_NotifyIcon(NIM_ADD)` 会失败，程序就没有托盘图标了。
/// 每秒重试，半分钟内装上就停。
fn ensure_tray() {
    use std::sync::atomic::AtomicU32;
    static TRIES: AtomicU32 = AtomicU32::new(0);
    if app().tray.lock().unwrap().is_some() {
        TRIES.store(0, Ordering::Relaxed);
        return;
    }
    if TRIES.fetch_add(1, Ordering::Relaxed) >= 30 {
        return;
    }
    let Some(h) = *app().hwnd.lock().unwrap() else {
        return;
    };
    if let Ok(t) = Tray::install(h, "LightBox") {
        *app().tray.lock().unwrap() = Some(t);
        log::info("托盘图标已补装");
    }
}

/// 资源管理器重启（任务栏换成了另一个窗口）之后：补回托盘图标、把长条重挂回去。
///
/// 外壳崩溃时 Windows 会立刻把它重启，新任务栏是一个新窗口：托盘图标全部消失，
/// Win10 的长条（`Shell_TrayWnd` 的子窗口）还会被系统连带销毁。不补这一步，
/// 程序的界面就再也不回来，而进程仍占着单实例互斥体 —— 用户双击也没反应。
fn check_taskbar_restart() {
    use std::sync::atomic::{AtomicIsize, AtomicU32};
    static LAST_TB: AtomicIsize = AtomicIsize::new(0);
    // 托盘图标还没补上（新任务栏刚起来时不接受 NIM_ADD，要隔一秒再试）
    static TRAY_PENDING: AtomicBool = AtomicBool::new(false);
    // 已经重试了几次
    static TRAY_TRIES: AtomicU32 = AtomicU32::new(0);

    // 切屏过渡期：任务栏此刻可能正在重建，跳过这一轮（上一 tick 的 LAST_TB
    // 还没被覆盖，等过渡期结束那一 tick 自然还会发现「换过了」）。
    if crate::bar::settling() {
        return;
    }

    // 任务栏正在重启的那几秒拿不到句柄，跳过，等下一次 tick
    if let Some(tb) = crate::taskbar::taskbar() {
        let cur = tb.0 as isize;
        let prev = LAST_TB.swap(cur, Ordering::Relaxed);
        // prev == 0 只是第一次记录，不是「换过了」
        if prev != 0 && prev != cur {
            log::info("任务栏换了窗口（资源管理器重启过）：补托盘图标、重挂长条");
            TRAY_PENDING.store(true, Ordering::Relaxed);
            TRAY_TRIES.store(0, Ordering::Relaxed);
            crate::bar::revive();
        }
    }

    // 托盘图标补登记：失败就下一 tick 再试，最多 10 次
    if TRAY_PENDING.load(Ordering::Relaxed) {
        let tries = TRAY_TRIES.fetch_add(1, Ordering::Relaxed);
        if tries >= 10 {
            log::info("补托盘图标连续失败，放弃");
            TRAY_PENDING.store(false, Ordering::Relaxed);
            return;
        }
        let done = match app().tray.lock().unwrap().as_ref() {
            Some(t) => t.reinstall(),
            // 本来就没装上托盘图标（启动时就失败了），没什么可补的
            None => true,
        };
        if done {
            TRAY_PENDING.store(false, Ordering::Relaxed);
        }
    }
}

/// 建一个隐藏窗口，作为托盘和全局消息的宿主
fn create_host_window() -> Result<HWND, String> {    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let cls = crate::util::wide(CLASS_NAME);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: windows::core::PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| format!("创建主窗口失败：{e}"))
    }
}

fn on_menu(id: usize) {
    match id {
        IDM_SHOW_BAR => crate::bar::show(),
        IDM_HIDE_BAR => crate::bar::hide(),
        IDM_AUTOSTART => {
            let want = !autostart::enabled();
            match autostart::set(want) {
                Ok(()) => log::info(&format!("开机自启已{}", if want { "打开" } else { "关闭" })),
                Err(e) => log::info(&format!("开机自启设置失败：{e}")),
            }
        }
        IDM_SETTINGS => crate::settings::open(None),
        IDM_DATA_DIR => {
            let dir = config::root_dir();
            let _ = desktop::shell_open(&dir.to_string_lossy());
        }
        // 「检查更新…」不再弹结果框：统一跳到设置页「关于」，
        // 版本状态、更新说明和下载入口都在那一页；顺手后台刷新一次清单。
        IDM_CHECK_UPDATE => {
            crate::settings::open_about();
            crate::updater::check(false);
        }
        IDM_QUIT => {
            quit();
        }
        _ => {}
    }
}

/// 右键托盘图标 → 弹出菜单
fn show_tray_menu(hwnd: HWND) {
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(e) => {
                log::info(&format!("创建托盘菜单失败：{e}"));
                return;
            }
        };
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_SHOW_BAR,
            &windows::core::HSTRING::from("显示主界面"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_HIDE_BAR,
            &windows::core::HSTRING::from("隐藏主界面"),
        );
        // 「开机自启」在「设置」上面：开关类的项放一起，设置属于下面的操作区
        let mut flags = MF_STRING;
        if autostart::enabled() {
            flags |= MF_CHECKED;
        }
        let _ = AppendMenuW(
            menu,
            flags,
            IDM_AUTOSTART,
            &windows::core::HSTRING::from("开机自启"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_SETTINGS,
            &windows::core::HSTRING::from("设置…"),
        );
        // 位置从 0 数：显示 / 隐藏 / 开机自启 / 设置…
        crate::menu::attach_icon(menu, 3, crate::icons::MenuGlyph::Settings);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, &windows::core::HSTRING::from(""));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_CHECK_UPDATE,
            &windows::core::HSTRING::from("检查更新…"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_DATA_DIR,
            &windows::core::HSTRING::from("打开数据目录"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, &windows::core::HSTRING::from(""));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_QUIT,
            &windows::core::HSTRING::from("退出"),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 菜单弹出前必须把本窗口设为前台，否则点完菜单窗口失焦
        crate::menu::grab_foreground(hwnd);
        let cmd = TrackPopupMenuEx(
            menu,
            (TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        if cmd.0 != 0 {
            on_menu(cmd.0 as usize);
        }
    }
}

/// 启动：建窗口、装托盘、跑消息循环
pub fn run() {
    let hwnd = match create_host_window() {
        Ok(h) => h,
        Err(e) => {
            log::info(&format!("{e}"));
            return;
        }
    };
    *app().hwnd.lock().unwrap() = Some(hwnd);

    match Tray::install(hwnd, "LightBox") {
        Ok(t) => *app().tray.lock().unwrap() = Some(t),
        Err(e) => log::info(&format!("{e}")),
    }

    // 每秒一次的滴答：切屏检测、位置自愈都在这里做
    unsafe {
        SetTimer(Some(hwnd), 1, 1000, None);
    }

    log::info(&format!(
        "主循环启动，显示器 {} 块，数据目录 {}",
        geom::monitors().len(),
        config::root_dir().display()
    ));

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    log::info("主循环退出");
}

/// 退出时收拾干净（托盘图标、窗口）
pub fn shutdown() {
    app().running.store(false, Ordering::Relaxed);
    // 先删托盘图标：Tray 的 Drop 里会 Shell_NotifyIcon(NIM_DELETE)。
    // 它挂在 static OnceLock 里，进程退出不会自动 drop；而图标挂在宿主窗口上，
    // 窗口销毁后就没有收尾机会了 —— 所以必须在这里手动取出来释放。
    let _ = app().tray.lock().unwrap().take();
    // 锁先放开再销毁：DestroyWindow 会同步派发 WM_DESTROY，
    // 那边的处理要是再锁这把锁，就成自己等自己了。
    let h = { *app().hwnd.lock().unwrap() };
    unsafe {
        if let Some(h) = h {
            let _ = DestroyWindow(h);
        }
    }
}

/// 退出程序（菜单里的「退出」）
pub fn quit() {
    shutdown();
    unsafe {
        PostQuitMessage(0);
    }
}
