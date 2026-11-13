//! 改名小窗：点弹出面板头部的盒子名，就在这里改名字。
//!
//! 为什么不在分层面板上就地画一个输入框？
//! 分层窗口（`UpdateLayeredWindow`）不能当子控件的父窗口 —— 挂下去的 EDIT
//! 一个像素都看不见（设置窗口开头就记了这个坑）。而且盒子名要支持中文，
//! 系统输入法只认真控件，自己用键盘钩子拼字符拼不出中文。
//! 所以这里开一个**普通小窗**：底色跟弹出窗口一致，里面一个真正的 EDIT。
//! 它是弹出面板的附属窗口（owner = 面板），所以始终压在面板上面。

use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, HBRUSH, HDC, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::Ime::{
    ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPSTR, GCS_RESULTSTR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow,
    GetParent, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, LoadCursorW, PostMessageW, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWLP_WNDPROC,
    WINDOW_EX_STYLE, WM_ACTIVATE, WM_CTLCOLOREDIT, WM_DESTROY, WM_ERASEBKGND,
    WM_IME_COMPOSITION, WM_KEYDOWN, WM_SETFONT, WM_SETTEXT, WNDCLASSW, WS_CHILD,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP, WS_VISIBLE, IDC_ARROW, SW_SHOW,
    SWP_SHOWWINDOW, WA_INACTIVE, WNDPROC,
};

use crate::app::app;
use crate::config;
use crate::theme;
use crate::util::wide;
use crate::log;

const CLASS_NAME: &str = "LightBoxRename";

/// 输入框四周的留白
const D_PAD: f32 = 5.0;
/// 输入框的高
const D_EDIT_H: f32 = 26.0;
/// 字号（跟面板标题一致）
const D_FONT: f32 = 13.0;
/// 输入框宽度上限
const D_W_MAX: f32 = 260.0;

/// EM_SETSEL：全选输入框里的文字
const EM_SETSEL: u32 = 0x00B1;

/// 输入框里按了回车（延后到消息队列里再处理，见 edit_proc）
const WM_COMMIT_NAME: u32 = 0x8101;

struct RenameState {
    hwnd: Option<HWND>,
    edit: Option<HWND>,
    /// 正在改名的盒子
    box_id: String,
    font: Option<HFONT>,
    brush: Option<HBRUSH>,
    /// 正在提交：防止「提交 → 销毁 → 失焦 → 再提交」递归
    committing: bool,
    /// 刚刚用输入法选了一个词
    ///
    /// 选词那一下的回车是「把词送进输入框」，不是「提交名字」——
    /// 不记这一下的话，中文名只能写到第一个词就打住。
    ime_word: bool,
}

static STATE: OnceLock<Mutex<RenameState>> = OnceLock::new();

// 状态只在主线程里读写（窗口消息都在主线程），HWND 只是句柄。
unsafe impl Send for RenameState {}

fn state() -> &'static Mutex<RenameState> {
    STATE.get_or_init(|| {
        Mutex::new(RenameState {
            hwnd: None,
            edit: None,
            box_id: String::new(),
            font: None,
            brush: None,
            committing: false,
            ime_word: false,
        })
    })
}

/// 输入框的子类过程：原来的窗口过程（拦回车 / Esc 用）
static EDIT_ORIG: OnceLock<WNDPROC> = OnceLock::new();

/// 改名窗开着吗
///
/// 面板的鼠标钩子、键盘钩子都要靠它**让路**：改名的时候 Esc / Ctrl+A 归输入框，
/// 点输入框也不算「点了面板外面」。
pub fn is_open() -> bool {
    state().lock().unwrap().hwnd.is_some()
}

/// 打开改名窗。
///
/// `at` 是输入框要占的位置（屏幕坐标，一般就是面板标题那块），
/// `name` 是当前的名字，`owner` 是弹出面板的窗口句柄。
pub fn open(box_id: &str, name: &str, at: RECT, scale: f32, owner: HWND) {
    close();
    let r = |v: f32| (v * scale).round() as i32;
    let pad = r(D_PAD);
    // 改名框也跟着「外观 → 弹窗字体」走：字体名 + 字号倍率都取当前配置，
    // 窗口高度同步放大，否则字号调大后 EDIT 会把字裁掉。
    let (family, font_scale) = {
        let cfg = crate::app::app().cfg.lock().unwrap();
        (
            cfg.panel_font.clone(),
            (cfg.panel_font_size.clamp(
                crate::config::FONT_SIZE_MIN,
                crate::config::FONT_SIZE_MAX,
            ) / crate::config::FONT_SIZE_BASE)
                .max(0.5),
        )
    };
    let edit_h = (D_EDIT_H * scale * font_scale).round() as i32;
    let h = edit_h + pad * 2;
    // 宽度：跟着名字长度走（至少 120），**不**按面板标题区宽度封顶 ——
    // 面板窄的时候按标题区宽度算，输入框会被压成一条缝，没法改字
    let text_w = crate::render::text(name, r(D_FONT), true)
        .map(|t| t.w)
        .unwrap_or(0);
    let w = (text_w + r(24.0)).clamp(r(120.0), r(D_W_MAX));    // 垂直居中在标题那一行里
    let x = at.left;
    let y = at.top + ((at.bottom - at.top - h) / 2).max(0);

    let hwnd = match create_window(x, y, w, h, owner) {
        Ok(x) => x,
        Err(e) => {
            log::info(&format!("创建改名窗失败：{e}"));
            return;
        }
    };
    let edit = create_edit(hwnd, w, h, pad, scale, name, &family, font_scale);
    {
        let mut s = state().lock().unwrap();
        s.hwnd = Some(hwnd);
        s.edit = Some(edit);
        s.box_id = box_id.to_string();
        s.committing = false;
    }
    unsafe {
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_SHOWWINDOW);
        let _ = ShowWindow(hwnd, SW_SHOW);
        // 焦点给输入框，并全选：直接打字就是改名
        focus_edit(hwnd, edit);
        let _ = SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
    // 标题旁边的数字牌让给改名窗
    crate::panel::refresh_head();
}

/// 把键盘焦点抢到输入框上
///
/// 面板是 WS_EX_NOACTIVATE 的，点它不会让本进程变成前台窗口 ——
/// 键盘输入根本到不了输入框。把本线程临时挂到前台线程上就能正常打字，
/// 而且不会像「按一下假 ALT」那样惊动用户正在用的程序。
unsafe fn focus_edit(hwnd: HWND, edit: HWND) {
    let fg = GetForegroundWindow();
    let fg_thread = GetWindowThreadProcessId(fg, None);
    let my_thread = GetCurrentThreadId();
    let attached = fg_thread != 0
        && fg_thread != my_thread
        && AttachThreadInput(my_thread, fg_thread, true).as_bool();
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(Some(edit));
    if attached {
        let _ = AttachThreadInput(my_thread, fg_thread, false);
    }
}

/// 关掉改名窗（不提交）
pub fn close() {
    let (hwnd, font, brush) = {
        let mut s = state().lock().unwrap();
        let h = s.hwnd.take();
        s.edit = None;
        s.box_id.clear();
        s.committing = false;
        let f = s.font.take();
        let b = s.brush.take();
        (h, f, b)
    };
    unsafe {
        if let Some(h) = hwnd {
            let _ = DestroyWindow(h);
        }
        if let Some(f) = font {
            let _ = DeleteObject(f.into());
        }
        if let Some(b) = brush {
            let _ = DeleteObject(b.into());
        }
    }
    // 数字牌回来
    crate::panel::refresh_head();
}

/// 提交名字（面板里点别处收起编辑时也要用，所以放开给同 crate 调用）
pub(crate) fn commit() {
    let (edit, id) = {
        let mut s = state().lock().unwrap();
        if s.committing {
            return;
        }
        s.committing = true;
        (s.edit, s.box_id.clone())
    };
    if id.is_empty() {
        close();
        return;
    }
    let Some(h) = edit else {
        close();
        return;
    };
    let raw = unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; (n as usize) + 1];
        let _ = GetWindowTextW(h, &mut buf);
        crate::util::from_wide(&buf)
    };
    let v = raw.trim();
    let v = if v.is_empty() { "收纳盒" } else { v };
    let old = {
        let cfg = app().cfg.lock().unwrap().clone();
        cfg.boxes
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.name.clone())
            .unwrap_or_default()
    };
    if v != old {
        {
            let mut g = app().cfg.lock().unwrap();
            if let Some(b) = g.boxes.iter_mut().find(|b| b.id == id) {
                b.name = v.to_string();
            }
            config::save(&g);
        }
        crate::bar::refresh();
        crate::panel::reload();
        log::info(&format!("收纳盒改名：{old} → {v}"));
    }
    close();
}

// —— 窗口 ——

/// 底色跟弹出窗口一致
fn bg_color() -> (u8, u8, u8) {
    let cfg = app().cfg.lock().unwrap().clone();
    crate::util::parse_hex(&cfg.panel_bg).unwrap_or((0x5A, 0x60, 0x6B))
}

/// 输入框的凹槽底色
fn sunken_color(bg: (u8, u8, u8)) -> (u8, u8, u8) {
    theme::mix(bg, (0, 0, 0), 0.3)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_CTLCOLOREDIT => {
            let bg = bg_color();
            let sunken = sunken_color(bg);
            let fg = theme::text_on(sunken);
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            let _ = windows::Win32::Graphics::Gdi::SetTextColor(hdc, crate::ui::colorref(fg.0, fg.1, fg.2));
            let _ = windows::Win32::Graphics::Gdi::SetBkColor(hdc, crate::ui::colorref(sunken.0, sunken.1, sunken.2));
            let brush = state().lock().unwrap().brush;
            match brush {
                Some(b) => LRESULT(b.0 as isize),
                None => LRESULT(0),
            }
        }
        WM_DESTROY => {
            state().lock().unwrap().hwnd = None;
            LRESULT(0)
        }
        WM_COMMIT_NAME => {
            // 输入法还在组字 → 这一下回车是「选词」，不是「提交」
            let edit = state().lock().unwrap().edit;
            if let Some(e) = edit {
                if ime_composing(e) {
                    return LRESULT(0);
                }
            }
            commit();
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // 失去焦点 = 用户点到别处去了 → 就此提交
            if (wp.0 & 0xffff) as u32 == WA_INACTIVE as u32 {
                commit();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 输入框里是不是正在用输入法组字
///
/// 组字过程中回车是「选词」，不能当成「提交名字」—— 否则中文名字根本打不出来。
fn ime_composing(hwnd: HWND) -> bool {
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.0.is_null() {
            return false;
        }
        // 组字串的长度（字节数）：非空就是正在拼
        let n = ImmGetCompositionStringW(himc, GCS_COMPSTR, None, 0);
        let _ = ImmReleaseContext(hwnd, himc);
        n > 0
    }
}

fn create_window(x: i32, y: i32, w: i32, h: i32, owner: HWND) -> Result<HWND, String> {
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
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            Some(owner),
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        round_corners(hwnd, w, h);
        // 输入框的底刷：文字背景要跟凹槽色一致
        let sunken = sunken_color(bg_color());
        let brush = CreateSolidBrush(crate::ui::colorref(sunken.0, sunken.1, sunken.2));
        state().lock().unwrap().brush = Some(brush);
        Ok(hwnd)
    }
}

/// 圆角：跟「圆角窗口」开关走
fn round_corners(hwnd: HWND, w: i32, h: i32) {
    let cfg = app().cfg.lock().unwrap().clone();
    unsafe {
        if crate::taskbar::is_win11() {
            let pref = if cfg.panel_radius {
                DWMWCP_ROUND
            } else {
                DWMWCP_DONOTROUND
            };
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
        } else {
            let _ = windows::Win32::Graphics::Gdi::SetWindowRgn(hwnd, None, false);
            if cfg.panel_radius {
                let region =
                    windows::Win32::Graphics::Gdi::CreateRoundRectRgn(0, 0, w + 1, h + 1, 16, 16);
                let _ = windows::Win32::Graphics::Gdi::SetWindowRgn(hwnd, Some(region), true);
            }
        }
    }
}

/// 名称输入框
fn create_edit(
    parent: HWND,
    w: i32,
    h: i32,
    pad: i32,
    scale: f32,
    name: &str,
    family: &str,
    font_scale: f32,
) -> HWND {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap();
        let cls = wide("EDIT");
        let font = crate::ui::create_font_named(
            (D_FONT * scale * font_scale).round() as i32,
            true,
            family,
        );
        // 单行 EDIT 文字顶对齐：控件高度压到行高附近并垂直居中，名字才不会靠着上沿
        let inner_h = h - pad * 2;
        let fh = crate::ui::font_height(font);
        let edit_h = if fh > 0 {
            (fh + (4.0 * scale).round() as i32).min(inner_h)
        } else {
            inner_h
        };
        let edit_y = pad + (inner_h - edit_h) / 2;
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            pad,
            edit_y,
            w - pad * 2,
            edit_h,
            Some(parent),
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(x) => x,
            Err(e) => {
                log::info(&format!("创建改名输入框失败：{e}"));
                let _ = DeleteObject(font.into());
                return HWND::default();
            }
        };
        let text = wide(name);
        let _ = SendMessageW(
            hwnd,
            WM_SETTEXT,
            Some(WPARAM(0)),
            Some(LPARAM(text.as_ptr() as isize)),
        );
        let _ = SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        state().lock().unwrap().font = Some(font);
        // 子类化：拦回车（提交）和 Esc（撤销）
        let orig = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
        let _ = EDIT_ORIG.set(std::mem::transmute::<isize, WNDPROC>(orig));
        let proc: WNDPROC = Some(edit_proc);
        SetWindowLongPtrW(
            hwnd,
            GWLP_WNDPROC,
            proc.map_or(0, |f| f as *const () as isize),
        );
        hwnd
    }
}

/// 输入框的子类过程
unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_IME_COMPOSITION && (lp.0 & (GCS_RESULTSTR.0 as isize)) != 0 {
        // 输入法选好了一个词，词已经送进输入框了
        state().lock().unwrap().ime_word = true;
    }
    if msg == WM_KEYDOWN {
        let vk = (wp.0 & 0xffff) as u32;
        if vk == VK_RETURN.0 as u32 {
            // 刚选过一个词：这一下回车是输入法收尾，不是提交
            let just_word = {
                let mut s = state().lock().unwrap();
                let w = s.ime_word;
                s.ime_word = false;
                w
            };
            if !just_word {
                // 不在这里直接提交，扔进消息队列里稍后再办：
                // 输入法组字过程中的回车是「选词」，延后一步就不会把词吃掉
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(Some(parent), WM_COMMIT_NAME, WPARAM(0), LPARAM(0));
                }
            }
            return LRESULT(0);
        }
        if vk == VK_ESCAPE.0 as u32 {
            close();
            return LRESULT(0);
        }
        // 别的键 = 用户接着往下打字，之前那次选词不算数了
        state().lock().unwrap().ime_word = false;
    }
    match EDIT_ORIG.get().copied().flatten() {
        Some(f) => CallWindowProcW(Some(f), hwnd, msg, wp, lp),
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
