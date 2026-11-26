//! 自绘调色板（替代 `<input type="color">` 的系统对话框）+ 屏幕吸管。
//!
//! 为什么不用系统取色对话框：它弹的是**普通顶层窗口**，而设置窗口是置顶的 ——
//! 对话框直接被压在设置窗口底下，用户看到的就是「点了没反应」（换台机器、
//! 换一个 Windows 版本表现还不一样）。自己画就没有这些问题。
//!
//! 调色板是一个普通窗口（要和设置窗口同一套配色），里面是：
//! 常用色 → 饱和/明度方块 → 色相条 → hex 输入 + 吸管。
//! 吸管走后端开全屏取色，取到的颜色直接填进正在调的那个色块。

use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint,
    GetDC, GetPixel, InvalidateRect, ReleaseDC, SelectObject, UpdateWindow, BITMAPINFO,
    BITMAPINFOHEADER, CLR_INVALID, DIB_RGB_COLORS, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos,
    GetWindowRect, KillTimer, LoadCursorW, RegisterClassW, SetCursor, SetTimer,
    SetWindowsHookExW, SetWindowPos, ShowWindow, UnhookWindowsHookEx, WM_CHAR, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT,
    WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, HHOOK, HC_ACTION, IDC_ARROW, IDC_CROSS, IDC_HAND,
    SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_SHOWWINDOW, WH_MOUSE_LL,
};

use crate::app::app;
use crate::theme::{self, ColorTarget};
use crate::ui::{self, Align, Gfx};
use crate::util::wide;
use crate::log;

const CLASS_NAME: &str = "LightBoxPicker";
const MAG_CLASS: &str = "LightBoxMagnifier";
/// 光标闪烁的计时器编号
const CARET_TIMER: usize = 1;
/// 放大镜跟随鼠标的计时器编号
const MAG_TIMER: usize = 2;

/// 常用色（10 列 × 2 行）
const PRESETS: [&str; 20] = [
    "#FFFFFF", "#F5F6F8", "#E7E9ED", "#CFD3DA", "#9AA0AA", "#5A606B", "#2B2F36", "#1B1B1F",
    "#0E1116", "#000000", "#E8F0FE", "#BFD6FF", "#2F6FEB", "#0B3D91", "#DFF3E6", "#3FA96A",
    "#FFF4E0", "#F0A22E", "#FDE8E8", "#E05252",
];

// —— 布局（设计单位 = 96 DPI）——
const DW: f32 = 300.0;
const DPAD: f32 = 12.0;
const DTITLE: f32 = 34.0;
const DCHIP: f32 = 24.0;
const DGAP: f32 = 4.0;
const DSV: f32 = 150.0;
const DHUE: f32 = 16.0;
const DHEX: f32 = 28.0;

/// 取色器自己的底色：**固定浅灰**，不跟「弹出窗口」色走。
///
/// 取色器永远贴着设置页弹出来，两边用同一个颜色就分不出前后；
/// 固定成浅灰之后，设置页再把它加深，层次一眼就出来了。
/// 浅底上色卡都带深色描边，白色 / 浅灰色卡也不会和底色糊在一起。
const PICKER_BG: (u8, u8, u8) = (0xF3, 0xF4, 0xF6);

/// 吸管放大镜：取样窗口大小（物理像素）与放大倍数
const MAG_SAMPLE: i32 = 15;
const MAG_ZOOM: i32 = 8;

/// 吸管取色时鼠标停在哪
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Sv,
    Hue,
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Close,
    Preset(usize),
    Sv,
    Hue,
    Pipette,
    Done,
    /// hex 输入框（点一下把光标放进去）
    Hex,
}

struct Picker {
    hwnd: Option<HWND>,
    /// 放大镜窗口
    mag: Option<HWND>,
    /// 吸管期间的全局鼠标钩子
    hook: Option<HHOOK>,
    owner: Option<HWND>,
    target: ColorTarget,
    color: (u8, u8, u8),
    /// 正在调哪个（方块 / 色相条）
    drag: Option<Drag>,
    hover: Option<Hit>,
    /// hex 输入框里的文字（始终 "#" + 最多 6 位十六进制）
    hex: String,
    /// 光标位置：数字部分的下标（0 = 在第一位前面）
    caret: usize,
    /// 用户动过输入框没有。没动过时第一次输入 = 整串替换掉
    typed: bool,
    /// 光标闪烁的计时器
    blink: bool,
    /// 正在吸管取色
    picking: bool,
    /// 放大镜上一次落点（没挪动就跳过重绘，省得 15ms 一次空转）
    mag_pos: (i32, i32),
    /// 按下了吸管按钮（松手时还在按钮上才启动取色）
    armed: bool,
}

static STATE: OnceLock<Mutex<Picker>> = OnceLock::new();

// 全部状态只在主线程里读写（窗口消息都在主线程），HWND 只是句柄。
unsafe impl Send for Picker {}

fn state() -> &'static Mutex<Picker> {
    STATE.get_or_init(|| {
        Mutex::new(Picker {
            hwnd: None,
            mag: None,
            hook: None,
            owner: None,
            target: ColorTarget::Box,
            color: (0x5A, 0x60, 0x6B),
            drag: None,
            hover: None,
            hex: String::new(),
            caret: 0,
            typed: false,
            blink: true,
            picking: false,
            mag_pos: (i32::MIN, i32::MIN),
            armed: false,
        })
    })
}

/// 吸管图标位图缓存：(key, 不透明 BGRA)。光标闪烁会定期重绘，不必每次重算。
static PIPETTE_CACHE: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());

/// 打开调色板。`owner` 是设置窗口（调色板是它的子窗口层级的弹出层）。
pub fn open(target: ColorTarget, hex: &str, owner: HWND) {
    close();
    let rgb = crate::util::parse_hex(hex).unwrap_or((0x5A, 0x60, 0x6B));
    let scale = display_scale();
    let (h, _hits) = layout(scale);
    let hwnd = match create_window(h, scale, owner) {
        Ok(x) => x,
        Err(e) => {
            log::info(&format!("创建调色板失败：{e}"));
            return;
        }
    };
    {
        let mut s = state().lock().unwrap();
        s.hwnd = Some(hwnd);
        s.owner = Some(owner);
        s.target = target;
        s.color = rgb;
        s.hex = hex.to_uppercase();
        s.caret = digits_len(&s.hex);
        s.typed = false;
        s.drag = None;
        s.hover = None;
    }
    // 摆在设置窗口中间
    let mut orect = RECT::default();
    unsafe {
        let _ = GetWindowRect(owner, &mut orect);
    }
    let r = |v: f32| (v * scale).round() as i32;
    let w = r(DW);
    let x = orect.left + ((orect.right - orect.left - w) / 2).max(0);
    let y = orect.top + ((orect.bottom - orect.top - h) / 2).max(0);
    unsafe {
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_SHOWWINDOW);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetFocus(Some(hwnd));
        // 光标闪烁
        let _ = SetTimer(Some(hwnd), CARET_TIMER, 500, None);
    }
    // 设置页临时加深 —— 两个窗口底色一样就分不出层次，看不出谁在上面
    crate::settings::set_dimmed(true);
}

/// 关掉调色板
pub fn close() {
    let (hwnd, mag, hook) = {
        let mut s = state().lock().unwrap();
        let h = s.hwnd.take();
        let m = s.mag.take();
        let k = s.hook.take();
        s.owner = None;
        s.drag = None;
        s.hover = None;
        s.picking = false;
        (h, m, k)
    };
    unsafe {
        if let Some(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        if let Some(h) = hwnd {
            let _ = KillTimer(Some(h), CARET_TIMER);
            let _ = DestroyWindow(h);
        }
        if let Some(m) = mag {
            let _ = DestroyWindow(m);
        }
    }
    // 设置页恢复正常底色
    crate::settings::set_dimmed(false);
}

// —— hex 输入 ——

/// hex 文本里的数字部分（去掉开头的 #）
fn digits(hex: &str) -> &str {
    hex.trim_start_matches('#')
}

/// 数字部分的个数
fn digits_len(hex: &str) -> usize {
    digits(hex).chars().count()
}

/// 把输入框恢复到「跟着当前颜色走」的状态（外部改了颜色之后调用）
fn reset_edit() {
    let mut s = state().lock().unwrap();
    s.caret = digits_len(&s.hex);
    s.typed = false;
}

// —— 布局 ——

fn layout(scale: f32) -> (i32, Vec<(Hit, RECT)>) {
    let r = |v: f32| (v * scale).round() as i32;
    let w = r(DW);
    let pad = r(DPAD);
    let mut hits: Vec<(Hit, RECT)> = Vec::new();

    // 标题
    hits.push((
        Hit::Close,
        RECT {
            left: w - r(34.0),
            top: r(4.0),
            right: w - r(6.0),
            bottom: r(30.0),
        },
    ));

    // 常用色：10 列 2 行
    let chip = r(DCHIP);
    let gap = r(DGAP);
    let grid_w = 10 * chip + 9 * gap;
    let gx = pad + (w - pad * 2 - grid_w) / 2;
    let gy = r(DTITLE) + r(4.0);
    for i in 0..PRESETS.len() {
        let cx = gx + (i % 10) as i32 * (chip + gap);
        let cy = gy + (i / 10) as i32 * (chip + gap);
        hits.push((
            Hit::Preset(i),
            RECT {
                left: cx,
                top: cy,
                right: cx + chip,
                bottom: cy + chip,
            },
        ));
    }

    let sv_y = gy + chip * 2 + gap + r(8.0);
    hits.push((
        Hit::Sv,
        RECT {
            left: pad,
            top: sv_y,
            right: w - pad,
            bottom: sv_y + r(DSV),
        },
    ));
    let hue_y = sv_y + r(DSV) + r(8.0);
    hits.push((
        Hit::Hue,
        RECT {
            left: pad,
            top: hue_y,
            right: w - pad,
            bottom: hue_y + r(DHUE),
        },
    ));
    let hex_y = hue_y + r(DHUE) + r(10.0);

    // 底部一行：色块 + hex + 吸管 + 完成
    hits.push((
        Hit::Pipette,
        RECT {
            left: w - pad - r(96.0),
            top: hex_y,
            right: w - pad - r(60.0),
            bottom: hex_y + r(DHEX),
        },
    ));
    hits.push((
        Hit::Done,
        RECT {
            left: w - pad - r(52.0),
            top: hex_y,
            right: w - pad,
            bottom: hex_y + r(DHEX),
        },
    ));
    // hex 输入框（左边让出当前色的小方块）
    hits.push((
        Hit::Hex,
        RECT {
            left: pad + r(32.0),
            top: hex_y,
            right: w - pad - r(96.0),
            bottom: hex_y + r(DHEX),
        },
    ));

    (hex_y + r(DHEX) + pad, hits)
}

fn hit_at(x: i32, y: i32, scale: f32) -> Option<Hit> {
    let (_h, hits) = layout(scale);
    for (hit, r) in hits {
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
            return Some(hit);
        }
    }
    None
}

fn display_scale() -> f32 {
    let cfg = app().cfg.lock().unwrap().clone();
    let mon = crate::geom::work_area_at(cfg.bar.x, cfg.bar.y);
    mon.scale as f32 / 96.0
}

/// Ctrl 键按着吗
fn ctrl_down() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

// —— 窗口 ——

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_down(hwnd, x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_up(hwnd, x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            on_move(hwnd, x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_CHAR => {
            on_char(wp.0 as u32);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = (wp.0 & 0xffff) as u32;
            if vk == 0x1b {
                close();
                return LRESULT(0);
            }
            if vk == 0x08 {
                on_backspace();
                return LRESULT(0);
            }
            if vk == 0x2e {
                on_delete();
                return LRESULT(0);
            }
            if vk == 0x25 {
                move_caret(-1);
                return LRESULT(0);
            }
            if vk == 0x27 {
                move_caret(1);
                return LRESULT(0);
            }
            if vk == 0x24 {
                set_caret(0);
                return LRESULT(0);
            }
            if vk == 0x23 {
                let n = state().lock().unwrap().hex.chars().count();
                set_caret(n as i32 - 1);
                return LRESULT(0);
            }
            // Ctrl+A：全选 = 清空，等着重新输入
            if vk == 0x41 && ctrl_down() {
                {
                    let mut s = state().lock().unwrap();
                    s.hex = "#".to_string();
                    s.caret = 0;
                    s.typed = false;
                }
                repaint_self();
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_SETCURSOR => {
            let scale = display_scale();
            let h = hit_at(x_of(lp), y_of(lp), scale);
            unsafe {
                let _ = SetCursor(
                    LoadCursorW(
                        None,
                        if h.is_some() { IDC_HAND } else { IDC_ARROW },
                    )
                    .ok(),
                );
            }
            LRESULT(1)
        }
        WM_TIMER => {
            // 光标闪烁
            let mut s = state().lock().unwrap();
            s.blink = !s.blink;
            drop(s);
            repaint_self();
            LRESULT(0)
        }
        WM_DESTROY => {
            state().lock().unwrap().hwnd = None;
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

fn create_window(height: i32, scale: f32, owner: HWND) -> Result<HWND, String> {
    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let cls = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: windows::core::PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let w = (DW * scale).round() as i32;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            w,
            height,
            Some(owner),
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        round_corners(hwnd, w, height);
        Ok(hwnd)
    }
}

/// 圆角：跟「窗口圆角」开关走。
///
/// 两种系统都靠 `SetWindowRgn` 硬裁（原因见 `settings::round_corners`：
/// Win11 的 DWM 圆角只圆边框，客户区还是方的，四角会露白直角）。
fn round_corners(hwnd: HWND, w: i32, h: i32) {
    let radius = app().cfg.lock().unwrap().panel_radius;
    unsafe {
        let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE,
            &windows::Win32::Graphics::Dwm::DWMWCP_DONOTROUND as *const _
                as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
        // 先把上一次的区域清掉，再按开关决定要不要圆角
        let _ = windows::Win32::Graphics::Gdi::SetWindowRgn(hwnd, None, false);
        if radius {
            // 半径和自绘那圈描边必须一致（见 `settings::corner_radius`）
            let r = (8.0 * display_scale()).round().max(4.0) as i32;
            let region =
                windows::Win32::Graphics::Gdi::CreateRoundRectRgn(0, 0, w + 1, h + 1, r * 2, r * 2);
            let _ = windows::Win32::Graphics::Gdi::SetWindowRgn(hwnd, Some(region), true);
        }
    }
}

// —— 绘制 ——

fn paint(hwnd: HWND) {
    let scale = display_scale();
    let (_height, hits) = layout(scale);
    let s = state().lock().unwrap();
    let target = s.target;
    let color = s.color;
    let hex = s.hex.clone();
    let caret = s.caret;
    let blink = s.blink;
    let hover = s.hover;
    drop(s);

    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        // 双缓冲：先画在离屏位图上，画完一次性贴到窗口 —— 拖色块时不会闪
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let cw = (rc.right - rc.left).max(1);
        let ch = (rc.bottom - rc.top).max(1);
        let buf = ui::Buf::new(cw, ch);
        let dc = match &buf {
            Some(b) => b.hdc(),
            None => hdc,
        };
        let mut g = Gfx::new(dc);
        // 色卡 / 按钮的圆角走像素直写通道画抗锯齿（没有离屏位图就自动退回 GDI）
        if let Some(b) = &buf {
            g.attach(b.surface());
        }
        let r = |v: f32| (v * scale).round() as i32;
        let w = r(DW);
        let pad = r(DPAD);
        // 固定浅灰底：和设置页同色就分不出层次（设置页在下面临时加深）
        let bg = PICKER_BG;
        let sunken = theme::mix(bg, (0, 0, 0), 0.07);
        let fg = theme::text_on(bg);
        let accent = theme::accent_on(bg);
        // 色卡描边：所有色卡用同一条中性深边，浅色卡（白 / 浅灰）也有轮廓
        let edge = theme::mix(bg, (0, 0, 0), 0.42);

        g.fill_rect(0, 0, cw, ch, bg);
        // 窗口边缘描边：和设置页同一套 —— 圆角裁区是 1 像素台阶，
        // 靠这圈抗锯齿描边把它压住，看起来才是平滑的边。
        if app().cfg.lock().unwrap().panel_radius {
            g.stroke_round_aa(
                0,
                0,
                cw,
                ch,
                r(8.0),
                theme::mix(bg, fg, 0.32),
                1,
            );
        }
        g.font(r(13.0), true);
        g.text(
            pad,
            0,
            w - pad * 2,
            r(DTITLE),
            &format!("{}背景色", target.label()),
            fg,
            Align::Left,
        );
        let cr = hits
            .iter()
            .find(|(h, _)| *h == Hit::Close)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        if hover == Some(Hit::Close) {
            g.fill_round_aa(cr.left, cr.top, cr.right - cr.left, cr.bottom - cr.top, r(6.0), theme::mix(bg, fg, 0.14));
        }
        g.font(r(13.0), false);
        g.text(cr.left, cr.top, cr.right - cr.left, cr.bottom - cr.top, "✕", fg, Align::Center);
        g.hline(0, w, r(DTITLE) - 1, theme::mix(bg, fg, 0.18));

        // 常用色
        for (i, c) in PRESETS.iter().enumerate() {
            let Some((_, rect)) = hits.iter().find(|(h, _)| *h == Hit::Preset(i)) else {
                continue;
            };
            let rgb = crate::util::parse_hex(c).unwrap_or((0, 0, 0));
            let cw = rect.right - rect.left;
            let ch = rect.bottom - rect.top;
            g.fill_round_aa(rect.left, rect.top, cw, ch, r(5.0), rgb);
            g.stroke_round_aa(rect.left, rect.top, cw, ch, r(5.0), edge, 1);
            if hover == Some(Hit::Preset(i)) {
                g.stroke_round_aa(
                    rect.left - r(2.0),
                    rect.top - r(2.0),
                    cw + r(4.0),
                    ch + r(4.0),
                    r(7.0),
                    accent,
                    1,
                );
            }
        }

        // 饱和/明度方块
        let sv = hits
            .iter()
            .find(|(h, _)| *h == Hit::Sv)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        let sv_w = sv.right - sv.left;
        let sv_h = sv.bottom - sv.top;
        if sv_w > 0 && sv_h > 0 {
            let (h, s_, v) = theme::rgb_to_hsv(color);
            let px = sv_pixels(h, sv_w, sv_h);
            blit_bgra(dc, sv.left, sv.top, sv_w, sv_h, &px);
            // 色卡描边：SV 方块和灰色底之间要有一条分界
            g.stroke_round_aa(sv.left, sv.top, sv_w, sv_h, 0, edge, 1);
            // 光标圈
            let cx = sv.left + (sv_w as f32 * s_).round() as i32;
            let cy = sv.bottom - 1 - (sv_h as f32 * v).round() as i32;
            g.stroke_round_aa(cx - r(5.0), cy - r(5.0), r(10.0), r(10.0), r(5.0), (255, 255, 255), 2);
            g.stroke_round_aa(cx - r(6.0), cy - r(6.0), r(12.0), r(12.0), r(6.0), (0, 0, 0), 1);
        }

        // 色相条
        let hue = hits
            .iter()
            .find(|(h, _)| *h == Hit::Hue)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        let hue_w = hue.right - hue.left;
        if hue_w > 0 {
            let px = hue_pixels(hue_w, hue.bottom - hue.top);
            blit_bgra(dc, hue.left, hue.top, hue_w, hue.bottom - hue.top, &px);
            g.stroke_round_aa(hue.left, hue.top, hue_w, hue.bottom - hue.top, 0, edge, 1);
            let (h, _, _) = theme::rgb_to_hsv(color);
            let cx = hue.left + ((hue_w as f32 - 1.0) * h / 360.0).round() as i32;
            g.stroke_round_aa(cx - r(4.0), hue.top - r(2.0), r(8.0), hue.bottom - hue.top + r(4.0), r(4.0), (255, 255, 255), 2);
            g.stroke_round_aa(cx - r(5.0), hue.top - r(3.0), r(10.0), hue.bottom - hue.top + r(6.0), r(5.0), (0, 0, 0), 1);
        }

        // 底部：当前色 + hex + 吸管 + 完成
        let hex_y = hue.bottom + r(10.0);
        let hex_x = pad + r(32.0);
        let hex_w = w - pad - r(96.0) - hex_x;
        g.fill_round_aa(hex_x, hex_y, hex_w, r(DHEX), r(6.0), sunken);
        g.stroke_round_aa(hex_x, hex_y, hex_w, r(DHEX), r(6.0), theme::mix(bg, (0, 0, 0), 0.18), 1);
        g.font(r(13.0), false);
        let text_x = hex_x + r(8.0);
        g.text(text_x, hex_y, hex_w - r(16.0), r(DHEX), &hex, fg, Align::Left);
        // 光标：画在第几位前面。
        // 用 text_w_exact 而不是 text_w：后者量的比实际绘制宽，光标会离文字空半格。
        if blink {
            let prefix: String = std::iter::once('#')
                .chain(digits(&hex).chars().take(caret))
                .collect();
            let cx = text_x + g.text_w_exact(&prefix);
            g.fill_rect(cx, hex_y + r(5.0), 2, r(DHEX) - r(10.0), fg);
        }
        // 当前颜色的小方块（同样带一条深边，白 / 浅色也看得见）
        g.fill_round_aa(pad, hex_y + r(2.0), r(24.0), r(24.0), r(4.0), color);
        g.stroke_round_aa(pad, hex_y + r(2.0), r(24.0), r(24.0), r(4.0), edge, 1);

        let pip = hits
            .iter()
            .find(|(h, _)| *h == Hit::Pipette)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        let pip_hot = hover == Some(Hit::Pipette);
        if pip_hot {
            g.fill_round_aa(
                pip.left,
                pip.top,
                pip.right - pip.left,
                pip.bottom - pip.top,
                r(6.0),
                theme::mix(bg, fg, 0.12),
            );
        }
        // 图标走 4× 超采样位图（直接画会全是锯齿），底色要跟着悬停色走
        let icon_bg = if pip_hot { theme::mix(bg, fg, 0.12) } else { bg };
        let icon_size = (pip.bottom - pip.top - r(2.0)).max(r(18.0));
        if let Some(px) = pipette_bitmap(icon_size, icon_bg, fg) {
            let ix = (pip.left + pip.right) / 2 - icon_size / 2;
            let iy = (pip.top + pip.bottom) / 2 - icon_size / 2;
            blit_bgra(dc, ix, iy, icon_size, icon_size, &px);
        }

        let done = hits
            .iter()
            .find(|(h, _)| *h == Hit::Done)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        if hover == Some(Hit::Done) {
            g.fill_round_aa(done.left, done.top, done.right - done.left, done.bottom - done.top, r(6.0), accent);
        }
        g.font(r(12.0), false);
        // 悬停时按钮是蓝底，字得跟着蓝底走（不然深色字压在深蓝上看不清）
        let done_fg = if hover == Some(Hit::Done) {
            theme::text_on(accent)
        } else {
            fg
        };
        g.text(
            done.left,
            done.top,
            done.right - done.left,
            done.bottom - done.top,
            "完成",
            done_fg,
            Align::Center,
        );

        // 一次性贴到窗口：用户只看到画完的一帧
        drop(g);
        if let Some(b) = &buf {
            b.blit_to(hdc);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 吸管的小图标
///
/// 为什么不是竖着画：竖着（圆头 + 直杆）无论怎么调都像话筒 —— 话筒就是这个
/// 骨架。这版按图标的通行画法**斜 40° 摆**：右上捏的胶头 → 略宽的箍 →
/// 细长越走越细的针 → 尖嘴，尖嘴外悬着一滴颜色。有「头、细管、尖、一滴」
/// 这四段，配上左边 hex 输入框的语境，一眼能认出是取色用的吸管。
///
/// 小尺寸直接画会全是锯齿（画笔只有 1px），所以先按 4 倍画再降采样，
/// 见 `pipette_bitmap`。
fn draw_pipette(g: &Gfx, cx: f32, cy: f32, s: f32, fg: (u8, u8, u8)) {
    let f = |v: f32| v * s;
    let (ux, uy) = (-0.766f32, 0.643f32); // 轴向：左下
    let (nx, ny) = (-uy, ux); // 法向
    let len = f(21.0);
    let bx = cx - ux * len / 2.0;
    let by = cy - uy * len / 2.0;

    let xy = |d: f32, off: f32| {
        (
            (bx + ux * f(d) + nx * off).round() as i32,
            (by + uy * f(d) + ny * off).round() as i32,
        )
    };
    // 沿轴的一段梯形：(起点距离, 终点距离, 起点半宽, 终点半宽)
    let band = |d0: f32, d1: f32, hw0: f32, hw1: f32| {
        let a1 = xy(d0, f(hw0));
        let a2 = xy(d1, f(hw1));
        let a3 = xy(d1, -f(hw1));
        let a4 = xy(d0, -f(hw0));
        g.fill_poly(&[a1, a2, a3, a4], fg);
    };
    // 沿轴的胶囊：矩形段 + 两端圆
    let capsule = |d0: f32, d1: f32, hw: f32| {
        band(d0, d1, hw, hw);
        for d in [d0, d1] {
            let (px, py) = xy(d, 0.0);
            let rr = f(hw).round() as i32;
            g.fill_round_aa(px - rr, py - rr, rr * 2, rr * 2, rr, fg);
        }
    };

    // 胶头：短胖的胶囊，明显比管身粗
    capsule(0.0, 2.4, 3.4);
    // 箍：胶头下面那道略宽的收口，有它才不会连成一根棉签
    band(2.4, 4.2, 2.6, 2.1);
    // 针管：细长，越走越细
    band(4.2, 15.8, 1.5, 0.65);
    // 尖嘴：收成针尖
    band(15.8, 18.4, 0.65, 0.15);

    // 尖嘴外悬着一滴：说明这是「吸」颜色的
    let (qx, qy) = xy(18.4, 0.0);
    let (dx, dy) = (qx as f32 + ux * f(1.7), qy as f32 + uy * f(1.7));
    let rr = f(1.4).round() as i32;
    g.fill_round_aa(dx as i32 - rr, dy as i32 - rr, rr * 2, rr * 2, rr, fg);
}

/// 吸管位图：4× 超采样后按块平均，得到不透明 BGRA（已合成到底色上）
///
/// 结果按 (尺寸, 底色) 缓存：光标闪烁会 500ms 重绘一次，没必要每次都重算。
fn pipette_bitmap(size: i32, bg: (u8, u8, u8), fg: (u8, u8, u8)) -> Option<Vec<u8>> {
    const SS: i32 = 4;
    if size <= 0 {
        return None;
    }
    let key: u64 = ((size as u64) << 24)
        | ((bg.0 as u64) << 16)
        | ((bg.1 as u64) << 8)
        | bg.2 as u64;
    if let Some((_, px)) = PIPETTE_CACHE
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| *k == key)
    {
        return Some(px.clone());
    }

    let big = size * SS;
    let px = unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: big,
                biHeight: -big,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            return None;
        }
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, HGDIOBJ(hbmp.0));
        let g = Gfx::new(dc);
        g.fill_rect(0, 0, big, big, bg);
        // 图形按设计高度 24.2 单位算：占画布 85%，左右留白（竖直的一支，本来就不宽）
        draw_pipette(
            &g,
            big as f32 / 2.0,
            big as f32 / 2.0,
            big as f32 * 0.85 / 24.2,
            fg,
        );
        drop(g);

        let src = bits as *const u8;
        let mut out = vec![255u8; (size * size * 4) as usize];
        let n = (SS * SS) as u32;
        for y in 0..size {
            for x in 0..size {
                let (mut sb, mut sg, mut sr) = (0u32, 0u32, 0u32);
                for dy in 0..SS {
                    for dx in 0..SS {
                        let i = (((y * SS + dy) * big + (x * SS + dx)) * 4) as isize;
                        let p = src.offset(i);
                        sb += *p as u32;
                        sg += *p.add(1) as u32;
                        sr += *p.add(2) as u32;
                    }
                }
                let o = ((y * size + x) * 4) as usize;
                out[o] = (sb / n) as u8;
                out[o + 1] = (sg / n) as u8;
                out[o + 2] = (sr / n) as u8;
            }
        }
        let _ = SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(dc);
        out
    };

    let mut cache = PIPETTE_CACHE.lock().unwrap();
    if cache.len() >= 8 {
        cache.clear();
    }
    cache.push((key, px.clone()));
    Some(px)
}

/// 饱和/明度方块的像素（BGRA）
///
/// 顶边是纯色相，往下混白（饱和度低），再叠一层黑（明度低）。
fn sv_pixels(hue: f32, w: i32, h: i32) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    for x in 0..w {
        let s = if w > 1 { x as f32 / (w - 1) as f32 } else { 0.0 };
        let base = theme::hsv_to_rgb(hue, 1.0, 1.0);
        let top = theme::mix(base, (255, 255, 255), 1.0 - s);
        for y in 0..h {
            let v = if h > 1 { 1.0 - y as f32 / (h - 1) as f32 } else { 1.0 };
            let c = theme::mix(top, (0, 0, 0), 1.0 - v);
            let i = ((y * w + x) * 4) as usize;
            px[i] = c.2;
            px[i + 1] = c.1;
            px[i + 2] = c.0;
            px[i + 3] = 255;
        }
    }
    px
}

/// 色相条的像素（BGRA）
fn hue_pixels(w: i32, h: i32) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    for x in 0..w {
        let hue = if w > 1 { x as f32 / (w - 1) as f32 * 359.0 } else { 0.0 };
        let c = theme::hsv_to_rgb(hue, 1.0, 1.0);
        for y in 0..h {
            let i = ((y * w + x) * 4) as usize;
            px[i] = c.2;
            px[i + 1] = c.1;
            px[i + 2] = c.0;
            px[i + 3] = 255;
        }
    }
    px
}

/// 把一段 BGRA 像素按 1:1 画到窗口上
fn blit_bgra(hdc: windows::Win32::Graphics::Gdi::HDC, x: i32, y: i32, w: i32, h: i32, px: &[u8]) {
    if w <= 0 || h <= 0 {
        return;
    }
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };
        let hbmp = match CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) => b,
            Err(_) => {
                let _ = ReleaseDC(None, screen);
                return;
            }
        };
        let old = SelectObject(mem, HGDIOBJ(hbmp.0));
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
        let _ = windows::Win32::Graphics::Gdi::BitBlt(
            hdc,
            x,
            y,
            w,
            h,
            Some(mem),
            0,
            0,
            SRCCOPY,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, screen);
    }
}

// —— 交互 ——

fn on_down(hwnd: HWND, x: i32, y: i32) {
    let scale = display_scale();
    let Some(hit) = hit_at(x, y, scale) else { return };
    match hit {
        Hit::Close => close(),
        Hit::Preset(i) => {
            let c = PRESETS[i];
            if let Some(rgb) = crate::util::parse_hex(c) {
                set_color(rgb, Some(c.to_string()));
                reset_edit();
            }
        }
        Hit::Sv => {
            state().lock().unwrap().drag = Some(Drag::Sv);
            apply_sv(x, y, scale);
        }
        Hit::Hue => {
            state().lock().unwrap().drag = Some(Drag::Hue);
            apply_hue(x, scale);
        }
        Hit::Pipette => {
            // 只记下「按在吸管上」，真正启动放到松手那一步（见 on_up）
            state().lock().unwrap().armed = true;
        }
        Hit::Done => {
            commit_hex();
            close();
        }
        Hit::Hex => {
            // 点一下输入框：按点击位置放光标
            let caret = caret_at(x, scale);
            set_caret(caret);
        }
    }
    let _ = hwnd;
}

/// 松手：吸管在**松手**这一刻才启动。
///
/// 之前是「按下就启动」：鼠标钩子刚装好，这次点击的「松手」就到了，
/// 钩子立刻把刚起来的吸管又收掉 —— 表现就是「点了吸管，什么都没发生」。
/// 改到松手再启动，那次松手已经在钩子装好之前发生掉了，不会再被吃掉。
fn on_up(hwnd: HWND, x: i32, y: i32) {
    let scale = display_scale();
    let hit = hit_at(x, y, scale);
    let armed = {
        let mut s = state().lock().unwrap();
        s.drag = None;
        std::mem::replace(&mut s.armed, false)
    };
    if armed && hit == Some(Hit::Pipette) {
        begin_pick();
    }
    let _ = hwnd;
}

/// 点在输入框的哪个位置 → 光标放哪
///
/// 以前用「每个字符约 13px」估位置，字号 / 字体一变就对不上（点在第 3 位，
/// 光标跑到第 4 位）。现在按真实进位宽度算每个卡位的 x，取离得最近的那个。
fn caret_at(x: i32, scale: f32) -> i32 {
    let rect = {
        let (_h, hits) = layout(scale);
        hits.iter()
            .find(|(h, _)| *h == Hit::Hex)
            .map(|(_, r)| *r)
            .unwrap_or_default()
    };
    let r = |v: f32| (v * scale).round() as i32;
    let text_x = rect.left + r(8.0);
    let hex = state().lock().unwrap().hex.clone();
    let chars: Vec<char> = digits(&hex).chars().collect();
    let Some(buf) = ui::Buf::new(8, 8) else {
        return 0;
    };
    let mut g = Gfx::new(buf.hdc());
    g.font(r(13.0), false);
    let mut best = 0;
    let mut best_d = i32::MAX;
    for i in 0..=chars.len() {
        let prefix: String = std::iter::once('#').chain(chars[..i].iter().copied()).collect();
        let cx = text_x + g.text_w_exact(&prefix);
        let d = (x - cx).abs();
        if d < best_d {
            best_d = d;
            best = i as i32;
        }
    }
    best
}

/// 输入框里的 hex 是完整的就按它改一次颜色（点「完成」时用）
fn commit_hex() {
    let hex = state().lock().unwrap().hex.clone();
    if let Some(rgb) = crate::util::parse_hex(&hex) {
        set_color(rgb, Some(hex));
    }
}

fn on_move(hwnd: HWND, x: i32, y: i32) {
    let scale = display_scale();
    let drag = state().lock().unwrap().drag;
    match drag {
        Some(Drag::Sv) => apply_sv(x, y, scale),
        Some(Drag::Hue) => apply_hue(x, scale),
        None => {
            let hit = hit_at(x, y, scale);
            let changed = {
                let mut s = state().lock().unwrap();
                let c = s.hover != hit;
                s.hover = hit;
                c
            };
            if changed {
                repaint_self();
            }
        }
    }
    let _ = hwnd;
}

/// 方块里的位置 → 饱和/明度
fn apply_sv(x: i32, y: i32, scale: f32) {
    let rect = {
        let (_h, hits) = layout(scale);
        hits
            .iter()
            .find(|(h, _)| *h == Hit::Sv)
            .map(|(_, r)| *r)
            .unwrap_or_default()
    };
    let w = (rect.right - rect.left).max(1);
    let h = (rect.bottom - rect.top).max(1);
    let s = (((x - rect.left) as f32 / (w - 1) as f32).clamp(0.0, 1.0) * 100.0).round() / 100.0;
    let v = (1.0 - ((y - rect.top) as f32 / (h - 1) as f32)).clamp(0.0, 1.0);
    let (hue, _, _) = {
        let st = state().lock().unwrap();
        theme::rgb_to_hsv(st.color)
    };
    set_color(theme::hsv_to_rgb(hue, s, v), None);
    reset_edit();
}

/// 色相条上的位置 → 色相
fn apply_hue(x: i32, scale: f32) {
    let rect = {
        let (_h, hits) = layout(scale);
        hits
            .iter()
            .find(|(h, _)| *h == Hit::Hue)
            .map(|(_, r)| *r)
            .unwrap_or_default()
    };
    let w = (rect.right - rect.left).max(1);
    let hue = (((x - rect.left) as f32 / (w - 1) as f32).clamp(0.0, 1.0) * 360.0) % 360.0;
    let (_old_hue, s, v) = {
        let st = state().lock().unwrap();
        theme::rgb_to_hsv(st.color)
    };
    set_color(theme::hsv_to_rgb(hue, s, v), None);
    reset_edit();
}

/// 改颜色：写配置 + 刷新界面
///
/// `hex` 为 None 时由 rgb 推出来。这里**不动**光标和 typed 标志 ——
/// 调用方（拖色块、点预设、吸管）改完颜色后自己调 `reset_edit()`。
fn set_color(rgb: (u8, u8, u8), hex: Option<String>) {
    let target = {
        let mut s = state().lock().unwrap();
        s.color = rgb;
        s.hex = hex
            .unwrap_or_else(|| crate::util::hex(rgb.0, rgb.1, rgb.2))
            .to_uppercase();
        s.target
    };
    crate::settings::apply_color(target, rgb);
    repaint_self();
}

/// hex 输入：只收十六进制数字
///
/// 规则和常见取色器一致：
/// - 没动过输入框时，第一次输入 = 整串替换（直接敲「5A606B」就行）；
/// - 动过之后就在光标位置插入，敲满 6 位为止；
/// - 敲满 6 位就实时生效。
fn on_char(code: u32) {
    let Some(ch) = char::from_u32(code) else { return };
    if !ch.is_ascii_hexdigit() {
        return;
    }
    let ch = ch.to_ascii_uppercase();
    let (text, complete) = {
        let mut s = state().lock().unwrap();
        let mut d: Vec<char> = digits(&s.hex).chars().collect();
        if !s.typed {
            // 第一次输入 = 整串替换掉
            d.clear();
            s.typed = true;
            s.caret = 0;
        }
        let mut caret = s.caret.min(d.len());
        if d.len() < 6 {
            d.insert(caret, ch);
        } else if caret < 6 {
            d[caret] = ch;
        } else {
            // 满了且光标在末尾：覆盖最后一位
            d[5] = ch;
            caret = 5;
        }
        caret = (caret + 1).min(d.len());
        let text: String = d.iter().collect();
        s.hex = format!("#{text}");
        s.caret = caret;
        (text, d.len() == 6)
    };
    if complete {
        if let Some(rgb) = crate::util::parse_hex(&text) {
            set_color(rgb, Some(format!("#{text}")));
            return;
        }
    }
    repaint_self();
}

/// 退格：删掉光标前面那一位
fn on_backspace() {
    let (text, complete) = {
        let mut s = state().lock().unwrap();
        let mut d: Vec<char> = digits(&s.hex).chars().collect();
        if !s.typed {
            // 还没动过：第一次退格 = 进入编辑模式并删掉最后一位
            s.typed = true;
        }
        let caret = s.caret.min(d.len());
        if caret > 0 {
            d.remove(caret - 1);
            s.caret = caret - 1;
        }
        let text: String = d.iter().collect();
        s.hex = format!("#{text}");
        (text, d.len() == 6)
    };
    if complete {
        if let Some(rgb) = crate::util::parse_hex(&text) {
            set_color(rgb, Some(format!("#{text}")));
            return;
        }
    }
    repaint_self();
}

/// Delete：删掉光标后面那一位
fn on_delete() {
    let (text, complete) = {
        let mut s = state().lock().unwrap();
        let mut d: Vec<char> = digits(&s.hex).chars().collect();
        s.typed = true;
        let caret = s.caret.min(d.len());
        if caret < d.len() {
            d.remove(caret);
        }
        let text: String = d.iter().collect();
        s.hex = format!("#{text}");
        (text, d.len() == 6)
    };
    if complete {
        if let Some(rgb) = crate::util::parse_hex(&text) {
            set_color(rgb, Some(format!("#{text}")));
            return;
        }
    }
    repaint_self();
}

/// 左右方向键：移动光标
fn move_caret(delta: i32) {
    // 先把值取出来再调用：`state().lock()` 的临时守卫会活到整条语句结束，
    // 直接写 `set_caret(state().lock().unwrap().caret ...)` 的话，
    // 守卫还握着锁，set_caret 里再锁一次就是自己等自己 —— 当场卡死。
    let caret = state().lock().unwrap().caret;
    set_caret(caret as i32 + delta);
}

/// 把光标放到第 n 位（数字部分的下标）
fn set_caret(n: i32) {
    {
        let mut s = state().lock().unwrap();
        let max = digits_len(&s.hex) as i32;
        s.caret = n.clamp(0, max) as usize;
        s.typed = true;
    }
    repaint_self();
}

/// 让调色板重画一遍
///
/// 只标脏、**不**用 `UpdateWindow` 当场同步重画：这个函数是从按键 / 鼠标
/// 消息里调的，同步重画等于在消息处理里再跑一遍绘制，绘制里再碰同一把锁
/// 就是死锁（按键一按就卡死）。标脏之后消息循环下一轮就会画，肉眼看不出延迟。
fn repaint_self() {
    let hwnd = state().lock().unwrap().hwnd;
    if let Some(h) = hwnd {
        unsafe {
            let _ = InvalidateRect(Some(h), None, false);
        }
    }
}

// —— 吸管 ——

/// 开始吸管取色：把设置窗口和调色板收起来，弹出放大镜跟着鼠标走
///
/// 鼠标事件用**全局钩子**收，不用放大镜窗口自己的消息 ——
/// 放大镜要跟着鼠标走，它本身不该挡着光标；而且窗口一隐藏就收不到焦点，
/// 靠窗口消息的话「按下 / 松开」经常收不到，表现就是「吸管没反应」。
fn begin_pick() {
    let mag = match create_magnifier() {
        Ok(m) => m,
        Err(e) => {
            log::info(&format!("创建放大镜失败：{e}"));
            return;
        }
    };
    {
        let mut s = state().lock().unwrap();
        s.mag = Some(mag);
        s.picking = true;
        // 新的一轮：清掉上次的落点，第一次 update 必定摆一次位置
        s.mag_pos = (i32::MIN, i32::MIN);
    }
    unsafe {
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(pick_hook), None, 0).ok();
        if hook.is_none() {
            log::info("吸管：安装鼠标钩子失败");
        }
        state().lock().unwrap().hook = hook;
        // 自己的界面先让开，别把用户吸到自己的颜色
        // （锁先取出句柄再 ShowWindow：ShowWindow 会同步派发消息）
        let self_hwnd = { state().lock().unwrap().hwnd };
        if let Some(h) = self_hwnd {
            let _ = ShowWindow(h, SW_HIDE);
        }
        let _ = crate::settings::hide();
        let _ = ShowWindow(mag, SW_SHOW);
        let _ = SetCapture(mag);
        let _ = SetCursor(LoadCursorW(None, IDC_CROSS).ok());
    }
    update_magnifier();
}

/// 结束吸管：取色 / 取消
fn end_pick(commit: Option<(u8, u8, u8)>) {
    let (mag, hook) = {
        let mut s = state().lock().unwrap();
        s.picking = false;
        (s.mag.take(), s.hook.take())
    };
    unsafe {
        if let Some(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        let _ = ReleaseCapture();
        if let Some(m) = mag {
            let _ = ShowWindow(m, SW_HIDE);
            let _ = DestroyWindow(m);
        }
        // 把自己的界面放回来：先设置窗口（它是调色板的宿主），再调色板
        let _ = crate::settings::show();
        let self_hwnd = { state().lock().unwrap().hwnd };
        if let Some(h) = self_hwnd {
            let _ = ShowWindow(h, SW_SHOW);
        }
    }
    if let Some(rgb) = commit {
        set_color(rgb, None);
        reset_edit();
    }
    repaint_self();
}

/// 吸管期间的全局鼠标钩子
unsafe extern "system" fn pick_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        match wp.0 as u32 {
            WM_MOUSEMOVE => {
                // 十字光标：钩子跑在目标窗口处理消息之前，这时设才留得住。
                // 放大镜的**位置**不在这里更新 —— 这个回调必须尽快返回，
                // 超时一次系统就会摘掉整个钩子（连点选都失效）；移动交给
                // 放大镜自己的 15ms 计时器。
                let _ = SetCursor(LoadCursorW(None, IDC_CROSS).ok());
            }
            WM_LBUTTONUP => {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let rgb = pixel_at(pt.x, pt.y);
                end_pick(rgb);
            }
            WM_RBUTTONUP => end_pick(None),
            _ => {}
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// 放大镜跟着鼠标走，并刷新显示
///
/// 由放大镜自己的 15ms 计时器驱动（`MAG_TIMER`）。光标没挪动就直接返回 ——
/// 不然吸管待机时每秒 60 多次取屏重绘，白烧 CPU。
fn update_magnifier() {
    let mag = {
        let mut s = state().lock().unwrap();
        let Some(m) = s.mag else { return };
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
        }
        let size = (MAG_SAMPLE * MAG_ZOOM + 12) as i32;
        // 放在光标的右下；贴到屏幕边就翻到另一边
        let mut x = pt.x + 24;
        let mut y = pt.y + 24;
        let virt = crate::geom::virtual_rect();
        if x + size > virt.right {
            x = pt.x - size - 24;
        }
        if y + size > virt.bottom {
            y = pt.y - size - 24;
        }
        if s.mag_pos == (x, y) {
            return;
        }
        s.mag_pos = (x, y);
        (m, x, y, size)
    };
    let (m, x, y, size) = mag;
    unsafe {
        let _ = SetWindowPos(m, None, x, y, size, size, SWP_NOACTIVATE);
        let _ = InvalidateRect(Some(m), None, false);
        let _ = UpdateWindow(m);
    }
}

fn create_magnifier() -> Result<HWND, String> {
    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let cls = wide(MAG_CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(mag_proc),
            hInstance: hinstance.into(),
            lpszClassName: windows::core::PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let size = (MAG_SAMPLE * MAG_ZOOM + 12) as i32;
        // WS_EX_TRANSPARENT：鼠标能穿过去，吸到的是放大镜**底下**那个像素
        // WS_EX_NOACTIVATE：不吃焦点，不打断用户正在做的事
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            size,
            size,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        // 跟随鼠标靠**自己的计时器**，不靠钩子转发的移动事件：
        // 鼠标钩子的回调有超时限制（`LowLevelHooksTimeout`，默认几百毫秒），
        // 一旦回调里做了取屏 + 重绘这种重活、超时一次，系统会**悄悄摘掉钩子**，
        // 表现就是「放大镜钉在原地不动了」（Win10 上遇到过，Win11 因为更快没露）。
        // 钩子现在只负责按键，移动交给这个 15ms 的计时器。
        let _ = SetTimer(Some(hwnd), MAG_TIMER, 15, None);
        Ok(hwnd)
    }
}

unsafe extern "system" fn mag_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER => {
            // 15ms 挪一次：放大镜跟着光标，顺带把底下那圈像素重取一遍
            update_magnifier();
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(Some(hwnd), MAG_TIMER);
            LRESULT(0)
        }
        WM_PAINT => {
            paint_magnifier(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = (wp.0 & 0xffff) as u32;
            if vk == 0x1b {
                end_pick(None);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 读屏幕上一个物理像素的颜色
fn pixel_at(x: i32, y: i32) -> Option<(u8, u8, u8)> {
    unsafe {
        let dc = GetDC(None);
        let c = GetPixel(dc, x, y);
        let _ = ReleaseDC(None, dc);
        if c.0 == CLR_INVALID {
            return None;
        }
        Some((
            (c.0 & 0xff) as u8,
            ((c.0 >> 8) & 0xff) as u8,
            ((c.0 >> 16) & 0xff) as u8,
        ))
    }
}

/// 截取光标周围一小块屏幕
fn sample_region(cx: i32, cy: i32, n: i32) -> Option<Vec<u8>> {
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                biHeight: -n,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };
        let hbmp = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(mem, HGDIOBJ(hbmp.0));
        let ok = BitBlt(
            mem,
            0,
            0,
            n,
            n,
            Some(screen),
            cx - n / 2,
            cy - n / 2,
            SRCCOPY,
        )
        .is_ok();
        let len = (n * n * 4) as usize;
        let mut px = vec![0u8; len];
        if ok {
            std::ptr::copy_nonoverlapping(bits as *const u8, px.as_mut_ptr(), len);
        }
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, screen);
        if ok {
            Some(px)
        } else {
            None
        }
    }
}

/// 画放大镜：放大后的像素 + 中心格的框
fn paint_magnifier(hwnd: HWND) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let n = MAG_SAMPLE;
    let zoom = MAG_ZOOM;
    let size = n * zoom + 12;
    let region = sample_region(pt.x, pt.y, n);

    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let g = Gfx::new(hdc);
        g.fill_rect(0, 0, size, size, (0x1E, 0x20, 0x26));
        if let Some(px) = region {
            // 放大：每个源像素画成一个 zoom×zoom 的块
            for sy in 0..n {
                for sx in 0..n {
                    let i = ((sy * n + sx) * 4) as usize;
                    let (b, gr, r) = (px[i], px[i + 1], px[i + 2]);
                    g.fill_rect(
                        6 + sx * zoom,
                        6 + sy * zoom,
                        zoom,
                        zoom,
                        (r, gr, b),
                    );
                }
            }
            // 中心格描边（白框 + 黑框，看得清选中的是哪一格）
            let cx = 6 + (n / 2) * zoom;
            let cy = 6 + (n / 2) * zoom;
            g.stroke_round_aa(cx - 1, cy - 1, zoom + 2, zoom + 2, 0, (255, 255, 255), 2);
            g.stroke_round_aa(cx - 2, cy - 2, zoom + 4, zoom + 4, 0, (0, 0, 0), 1);
        }
        g.stroke_round_aa(0, 0, size, size, 6, (0x60, 0x66, 0x72), 1);
        let _ = EndPaint(hwnd, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Buf;

    /// 手动跑：`cargo test --release 导出吸管图标预览 -- --ignored --nocapture`
    /// 把吸管图标放大 10 倍存到临时目录，肉眼确认造型（别再画成话筒）。
    #[test]
    #[ignore]
    fn 导出吸管图标预览() {
        let (size, zoom) = (26i32, 12u32);
        let bg = (0xF3, 0xF4, 0xF6);
        let fg = (0x28, 0x2C, 0x34);
        let px = pipette_bitmap(size, bg, fg).unwrap();
        let mut img = image::RgbaImage::new((size as u32) * zoom, (size as u32) * zoom);
        for y in 0..size {
            for x in 0..size {
                let i = ((y * size + x) * 4) as usize;
                let c = image::Rgba([px[i + 2], px[i + 1], px[i], 255]);
                for dy in 0..zoom {
                    for dx in 0..zoom {
                        img.put_pixel((x as u32) * zoom + dx, (y as u32) * zoom + dy, c);
                    }
                }
            }
        }
        let p = std::env::temp_dir().join("lightbox-pipette-preview.png");
        img.save(&p).unwrap();
        println!("{}", p.display());
    }

    /// hex 文字用「逐字进位」量宽度：和 `GetTextExtentPoint32W` 的估值差一格以内
    #[test]
    fn 十六进制宽度用逐字进位量() {
        let buf = Buf::new(8, 8).unwrap();
        let mut g = Gfx::new(buf.hdc());
        g.font(16, false);
        let exact = g.text_w_exact("#00393B");
        let rough = g.text_w("#00393B");
        // 实测：exact=65、rough=73（就是原来光标空出小半格的原因）
        assert!(
            exact < rough,
            "逐字进位 {exact} 应该小于 Point32 的 {rough}"
        );
        assert!(exact >= 55 && exact <= 70, "宽度看着不对：{exact}");
        // 单字符与累加一致（'#' 比数字宽，但都必须是正数）
        assert!(g.text_w_exact("#") > 0);
        assert_eq!(g.text_w_exact(""), 0);
    }
}
