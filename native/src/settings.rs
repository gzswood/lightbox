//! 设置窗口：左侧导航（收纳盒 / 外观 / 功能 / 关于）+ 右侧内容。
//!
//! 所有改动先写进 `draft`，**点「应用」或「确定」才落盘**：
//!  · 应用 = 保存并留在窗口里；
//!  · 确定 = 保存并关闭；
//!  · 取消 / 右上角 ✕ / Esc = 丢弃这次的全部改动。
//! 只有「弹出窗口底色 / 圆角」这类能立刻看见的项在 draft 里做即时预览，
//! 长条和面板本身要等应用后才变。
//!
//! 为什么是普通窗口而不是分层窗口：分层窗口不能当子控件的父窗口
//! （`UpdateLayeredWindow` 会把整个窗口内容换成位图，挂下面的 EDIT 一个像素
//! 都看不见，实测过）。盒子名要支持中文输入就必须用真正的 EDIT，所以走
//! 普通窗口：圆角靠 Win11 的 DWM `DWMWA_WINDOW_CORNER_PREFERENCE`、
//! Win10 靠 `SetWindowRgn` 裁圆角区域，阴影 DWM 自带。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, EnumFontFamiliesExW, GetDC, ReleaseDC, SelectObject,
    SetBkColor, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, DEFAULT_CHARSET, DIB_RGB_COLORS,
    HBRUSH, HDC, HGDIOBJ, HFONT, InvalidateRect, LOGFONTW, PAINTSTRUCT, SRCCOPY, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, KillTimer, LoadCursorW,
    MessageBoxW, RegisterClassW, SendMessageW, SetCursor, SetTimer,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WINDOW_STYLE, GWLP_WNDPROC, IDC_ARROW, IDC_HAND,
    MB_ICONWARNING,
    MB_OK, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW,
    WINDOW_EX_STYLE, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETFONT, WM_SETTEXT,
    WM_TIMER, WNDCLASSW, WNDPROC, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_TABSTOP, WS_VSCROLL,
};

use crate::app::app;
use crate::config;
use crate::desktop;
use crate::geom;
use crate::render::IconBmp;
use crate::theme::{self, ColorTarget};
use crate::ui::{self, Align, Gfx};
use crate::util::wide;
use crate::{autostart, icons, log};

const CLASS_NAME: &str = "LightBoxSettings";

/// `EM_SETSEL`：输入框里全选，别处没引这个常量（要开 Win32_UI_Controls 特性）
const EM_SETSEL: u32 = 0x00B1;

/// 「已复制」提示的定时器 id（1.4 秒后把提示抹掉）
const TIMER_COPIED: usize = 1;
const COPIED_MS: u32 = 1400;

/// 关于页三行信息：标签 + 值。三行**都可以点一下复制**（开源链接另有一个「打开」）
const ABOUT_ROWS: [(&str, &str); 3] = [
    ("作者", "Gathon"),
    ("微信", "gzswood"),
    ("开源链接", "https://gitee.com/gathon/lightbox"),
];

// —— ComboBox 消息 & 样式 ——
const CB_ADDSTRING: u32 = 0x0143;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;
const CB_GETLBTEXTW: u32 = 0x0148;
const CB_GETLBTEXTLEN: u32 = 0x0149;
const CB_SHOWDROPDOWN: u32 = 0x014F;
const CB_FINDSTRINGEXACT: u32 = 0x0158;
const CB_SETDROPPEDWIDTH: u32 = 0x0160;
const CBN_SELCHANGE: u32 = 1;
const CBN_CLOSEUP: u32 = 8;

/// 取色器弹出时设置页要临时加深 —— 两个窗口底色一样就分不出层次
static DIMMED: AtomicBool = AtomicBool::new(false);

/// 设置页现在是不是被加深了
fn dimmed() -> bool {
    DIMMED.load(Ordering::Relaxed)
}

// —— 尺寸（设计单位 = 96 DPI 下的逻辑像素）——
const DW: f32 = 680.0;
const DH: f32 = 560.0;
const DTITLE: f32 = 52.0;
const DSIDEBAR: f32 = 160.0;
const DPAD: f32 = 22.0;
const DFOOT: f32 = 60.0;
const DNAV: f32 = 42.0;
const DROW_H: f32 = 46.0;
const DBTN_W: f32 = 84.0;
const DBTN_H: f32 = 34.0;
/// 收纳盒列表下面要留给「隐藏名称开关 + 主界面模式」的空间
const ROW_AREA_BOTTOM: f32 = 140.0;
/// 关于页「更新信息」文本框（只有查到新版本时才画），在检查更新按钮下方
const DNOTES_TOP: f32 = 332.0;
const DNOTES_BOTTOM: f32 = 490.0;
const DNOTES_LINE: f32 = 18.0;

/// 左侧导航页
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Boxes,
    Look,
    Features,
    About,
}

const TABS: [Tab; 4] = [Tab::Boxes, Tab::Look, Tab::Features, Tab::About];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Boxes => "收纳盒设置",
            Tab::Look => "外观",
            Tab::Features => "功能设置",
            Tab::About => "关于",
        }
    }

    fn title(self) -> &'static str {
        self.label()
    }

    fn subtitle(self) -> &'static str {
        match self {
            Tab::Boxes => "调整顺序、名称、图标与主界面形态",
            Tab::Look => "透明度、背景色、圆角与弹窗字体",
            Tab::Features => "开机自启、置顶、诊断与存储位置",
            Tab::About => "版本信息与反馈渠道",
        }
    }
}

/// 开关项
#[derive(Clone, Copy, PartialEq, Debug)]
enum Setting {
    Acrylic,
    Radius,
    HideLabel,
    Topmost,
    Autostart,
    /// 「打印诊断信息」：写 lightbox.log（默认关）
    Diagnostics,
}

/// 可以点的区域
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Close,
    Nav(Tab),
    BoxRow(usize),
    BoxIcon(usize),
    BoxName(usize),
    BoxUp(usize),
    BoxDown(usize),
    Slider,
    Swatch(ColorTarget),
    ResetColor,
    Switch(Setting),
    Mode(bool),
    /// 外观页：弹窗字体名输入框
    FontName,
    /// 外观页：文字大小 − / +
    FontMinus,
    FontPlus,
    OpenDir,
    CheckUpdate,
    Download,
    /// 关于页三行信息（作者 / 微信 / 开源链接）：点一下复制
    CopyInfo(usize),
    /// 关于页「开源链接」右边的「打开」按钮（用系统浏览器打开 Gitee 仓库）
    Link,
    Apply,
    Cancel,
    Ok,
}

/// 布局结果：命中区域 + 收纳盒列表每一行的矩形（绘制和点击共用）
struct Layout {
    hits: Vec<(Hit, RECT)>,
    rows: Vec<RECT>,
}

fn hit_rect(lay: &Layout, hit: Hit) -> Option<RECT> {
    lay.hits.iter().find(|(h, _)| *h == hit).map(|(_, r)| *r)
}

fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 绘制上下文：缩放 + 各列边界
#[derive(Clone, Copy)]
struct Ctx {
    scale: f32,
    h: i32,
    sx: i32,
    cx: i32,
    cr: i32,
}

impl Ctx {
    fn r(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }
}

/// 一整套配色（全部由底色推出来）
#[derive(Clone, Copy)]
struct Colors {
    bg: (u8, u8, u8),
    fg: (u8, u8, u8),
    muted: (u8, u8, u8),
    accent: (u8, u8, u8),
    line: (u8, u8, u8),
    sunken: (u8, u8, u8),
}

struct Settings {
    hwnd: Option<HWND>,
    tab: Tab,
    /// 正在编辑的配置（应用前都只改这一份）
    draft: config::Config,
    /// 打开窗口时的那一份，用来判断「有没有改动」
    orig: config::Config,
    autostart: bool,
    autostart_orig: bool,
    /// 选中的盒子在 `draft.boxes` 里的下标
    selected: usize,
    /// 列表滚动到第几个盒子
    scroll: usize,
    /// 关于页「更新信息」文本框滚到第几行
    notes_scroll: usize,
    hover: Option<Hit>,
    /// 正在拖透明度滑块
    dragging: bool,
    /// 正在拖整个窗口时的「鼠标相对窗口左上角」偏移
    drag_win: Option<POINT>,
    /// 名字输入框（只有一个，反复挪到正在改的那一行）
    edit: Option<HWND>,
    /// 字体选择下拉框（常驻，默认隐藏；点「弹窗字体」时展开）
    font_combo: Option<HWND>,
    /// 枚举到的系统字体列表（顺序与 ComboBox 里的项严格一致）
    fonts: Vec<String>,
    /// 鼠标按下时命中了「弹窗字体」，等抬起再展开下拉框。
    /// 在 WM_LBUTTONDOWN 里直接 CB_SHOWDROPDOWN 会被紧接着的 mouse-up 收掉，
    /// 看起来就是「点不开」。
    pending_font_combo: bool,
    /// 正在改名字的盒子 id
    editing: Option<String>,
    /// 关于页刚复制了第几行（显示「已复制」提示，1.4 秒后自动消掉）
    copied: Option<usize>,
    brush: Option<HBRUSH>,
    font: Option<HFONT>,
}

static STATE: OnceLock<Mutex<Settings>> = OnceLock::new();

// 全部状态只在主线程里读写（窗口消息都在主线程），HWND 只是句柄。
unsafe impl Send for Settings {}

fn state() -> &'static Mutex<Settings> {
    STATE.get_or_init(|| {
        Mutex::new(Settings {
            hwnd: None,
            tab: Tab::Boxes,
            draft: config::Config::default(),
            orig: config::Config::default(),
            autostart: false,
            autostart_orig: false,
            selected: 0,
            scroll: 0,
            notes_scroll: 0,
            hover: None,
            dragging: false,
            drag_win: None,
            edit: None,
            font_combo: None,
            fonts: Vec::new(),
            pending_font_combo: false,
            editing: None,
            copied: None,
            brush: None,
            font: None,
        })
    })
}

/// 名字输入框的子类化：要在里面拦回车（提交）和 Esc（撤销）
static EDIT_ORIG: OnceLock<WNDPROC> = OnceLock::new();

/// 盒子图标的解码缓存（key = id + 图标数据哈希）
static ICON_CACHE: OnceLock<Mutex<HashMap<String, Option<IconBmp>>>> = OnceLock::new();

// —— 对外接口 ——

/// 打开设置窗口。`box_id` 为空 = 全局设置（托盘「设置…」进来的）；
/// 给了盒子 id = 打开并选中那个盒子。
pub fn open(box_id: Option<&str>) {
    open_inner(box_id, Tab::Boxes);
}

/// 打开（或唤起）设置窗口并停在「关于」页。
/// 面板标题栏的「有更新」小牌和托盘「检查更新…」都走这里：
/// 版本状态、更新说明、下载入口集中在一处，不再弹结果框。
pub fn open_about() {
    open_inner(None, Tab::About);
}

fn open_inner(box_id: Option<&str>, want_tab: Tab) {
    if is_open() {
        // 切页前先把输入框里没提交的内容收掉，别让 EDIT 留在别的页上
        commit_inline_edit();
        // 托盘「设置…」的「再点一次就收起」语义只留给原来的 open(None)
        if box_id.is_none() && want_tab == Tab::Boxes {
            close();
            return;
        }
        // 已经开着就只切页，别把用户改了一半的草稿丢掉
        let mut s = state().lock().unwrap();
        if let Some(id) = box_id {
            if let Some(i) = s.draft.boxes.iter().position(|b| b.id == id) {
                s.tab = Tab::Boxes;
                s.selected = i;
            }
        } else {
            s.tab = want_tab;
            s.notes_scroll = 0;
        }
        drop(s);
        hide_font_combo();
        state().lock().unwrap().pending_font_combo = false;
        repaint();
        let hwnd = state().lock().unwrap().hwnd;
        if let Some(h) = hwnd {
            unsafe {
                let _ = ShowWindow(h, SW_SHOW);
                let _ = SetFocus(Some(h));
            }
        }
        return;
    }
    let cfg = app().cfg.lock().unwrap().clone();
    let auto = autostart::enabled();
    let id = box_id.unwrap_or("").to_string();
    let scale = display_scale();
    let selected = cfg.boxes.iter().position(|b| b.id == id).unwrap_or(0);
    let visible = layout(
        scale,
        Tab::Boxes,
        cfg.boxes.len(),
        0,
        crate::updater::has_update(),
    )
    .rows
    .len()
    .max(1);
    let scroll = selected.saturating_sub(visible.saturating_sub(1));

    {
        let mut s = state().lock().unwrap();
        s.tab = want_tab;
        s.notes_scroll = 0;
        s.draft = cfg.clone();
        s.orig = cfg;
        s.autostart = auto;
        s.autostart_orig = auto;
        s.selected = selected;
        s.scroll = scroll;
        s.hover = None;
        s.dragging = false;
        s.drag_win = None;
        s.editing = None;
    }

    let hwnd = match create_window(scale) {
        Ok(x) => x,
        Err(e) => {
            log::info(&format!("创建设置窗口失败：{e}"));
            return;
        }
    };
    state().lock().unwrap().hwnd = Some(hwnd);
    if let Some(cache) = ICON_CACHE.get() {
        cache.lock().unwrap().clear();
    }
    refresh_edit_brush();
    let edit = create_edit(hwnd, scale);
    state().lock().unwrap().edit = Some(edit);
    // 字体下拉框（常驻，默认隐藏）
    let font_combo = create_font_combo(hwnd, scale);
    state().lock().unwrap().font_combo = Some(font_combo);
    refresh_round();
    position(hwnd, scale);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetFocus(Some(hwnd));
    }
}

/// 关掉设置窗口（未应用的改动直接丢弃）
pub fn close() {
    // 调色板开着的话先收掉（它会把「加深」状态一起恢复）
    crate::picker::close();
    hide_font_combo();
    let (hwnd, brush, font, font_combo) = {
        let mut s = state().lock().unwrap();
        let h = s.hwnd.take();
        let cb = s.font_combo.take();
        s.edit = None;
        s.editing = None;
        s.pending_font_combo = false;
        s.hover = None;
        s.dragging = false;
        s.drag_win = None;
        let b = s.brush.take();
        let f = s.font.take();
        (h, b, f, cb)
    };
    unsafe {
        if let Some(h) = hwnd {
            let _ = DestroyWindow(h);
        }
        // ComboBox 是 hwnd 的子控件，DestroyWindow(hwnd) 会一起销毁；
        // 但如果设置窗口已销毁，手动再销毁一次也不会出错。
        if let Some(cb) = font_combo {
            let _ = DestroyWindow(cb);
        }
        if let Some(b) = brush {
            let _ = DeleteObject(HGDIOBJ(b.0));
        }
        if let Some(f) = font {
            let _ = DeleteObject(HGDIOBJ(f.0));
        }
    }
    DIMMED.store(false, Ordering::Relaxed);
}

/// 设置窗口开着吗
pub fn is_open() -> bool {
    state().lock().unwrap().hwnd.is_some()
}

/// 临时加深 / 恢复设置页底色（取色器弹出时用）
pub fn set_dimmed(on: bool) {
    if DIMMED.swap(on, Ordering::Relaxed) == on {
        return;
    }
    refresh_edit_brush();
    repaint();
}

/// 临时藏起设置窗口（吸管取色时用 —— 别让用户吸到自己窗口的颜色）
pub fn hide() {
    let hwnd = state().lock().unwrap().hwnd;
    if let Some(h) = hwnd {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// 把设置窗口放回来（吸管结束）
pub fn show() {
    let hwnd = state().lock().unwrap().hwnd;
    if let Some(h) = hwnd {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
        }
    }
}

/// 取色器改了一个底色：只改 draft（预览），应用时才落盘
pub fn apply_color(target: ColorTarget, rgb: (u8, u8, u8)) {
    let hex = crate::util::hex(rgb.0, rgb.1, rgb.2);
    {
        let mut s = state().lock().unwrap();
        match target {
            ColorTarget::Box => s.draft.box_bg = hex,
            ColorTarget::Panel => s.draft.panel_bg = hex,
        }
    }
    // 设置页自己的底色跟着「弹出窗口」色走，输入框的底色要一起换
    refresh_edit_brush();
    repaint();
}

/// 设置窗口重画一遍（外观变了要立刻看到）
///
/// 只标脏、不 `UpdateWindow` 同步重画：这个函数会在别人的消息处理里被调
/// （取色器改色 → 回写设置页），同步重画会让绘制嵌套进当前消息，
/// 绘制里再拿同一把锁就死锁。标脏后下一轮消息循环立刻画，看不出来。
pub fn repaint() {
    let hwnd = state().lock().unwrap().hwnd;
    if let Some(h) = hwnd {
        unsafe {
            let _ = InvalidateRect(Some(h), None, false);
        }
    }
}

// —— 配色 ——

/// 设置窗口底色：固定浅灰，不跟「弹出窗口」的自定义底色走。
///
/// 设置页里有大量输入框、开关和按钮，如果跟着用户把弹窗底色调成深色/彩色，
/// 按钮和文字就只能靠对比度算法硬救，边缘状态（禁用、选中、悬停）容易撞色。
/// 固定浅灰之后，文字永远用深色、强调色永远用深蓝，层次稳定可预期。
fn bg_color() -> (u8, u8, u8) {
    const LIGHT: (u8, u8, u8) = (0xF3, 0xF4, 0xF6);
    if dimmed() {
        theme::mix(LIGHT, (0, 0, 0), 0.28)
    } else {
        LIGHT
    }
}

/// 输入框 / 凹槽的底色：浅色背景上用近白，深色背景上用更深的凹槽
fn sunken_color(bg: (u8, u8, u8)) -> (u8, u8, u8) {
    if theme::rel_lum(bg) > 0.45 {
        theme::mix(bg, theme::SNOW, 0.88)
    } else {
        theme::mix(bg, (0, 0, 0), 0.3)
    }
}

/// 分隔线：比底色略微明显一点
fn line_color(bg: (u8, u8, u8)) -> (u8, u8, u8) {
    theme::mix(bg, theme::text_on(bg), 0.18)
}

/// 重建输入框的背景刷（底色变了要跟着换）
fn refresh_edit_brush() {
    let sunken = sunken_color(bg_color());
    let old = state().lock().unwrap().brush.take();
    unsafe {
        if let Some(b) = old {
            let _ = DeleteObject(HGDIOBJ(b.0));
        }
        let brush = CreateSolidBrush(ui::colorref(sunken.0, sunken.1, sunken.2));
        state().lock().unwrap().brush = Some(brush);
    }
}

/// 有没有未应用的改动
fn dirty() -> bool {
    let s = state().lock().unwrap();
    s.draft != s.orig || s.autostart != s.autostart_orig
}

// —— 布局 ——

/// 算出所有可点区域和列表行矩形。绘制和命中测试共用这一份，避免两处对不上。
fn layout(scale: f32, tab: Tab, box_count: usize, scroll: usize, has_update: bool) -> Layout {
    let r = |v: f32| (v * scale).round() as i32;
    let w = r(DW);
    let h = r(DH);
    let sx = r(DSIDEBAR);
    let pad = r(DPAD);
    let cx = sx + pad;
    let cr = w - pad;
    let mut hits: Vec<(Hit, RECT)> = Vec::new();
    let mut rows: Vec<RECT> = Vec::new();

    // 关闭按钮
    hits.push((
        Hit::Close,
        RECT {
            left: w - r(46.0),
            top: r(6.0),
            right: w - r(6.0),
            bottom: r(42.0),
        },
    ));

    // 左侧导航
    let mut ny = r(DTITLE) + r(10.0);
    for t in TABS {
        hits.push((
            Hit::Nav(t),
            RECT {
                left: r(10.0),
                top: ny,
                right: sx - r(10.0),
                bottom: ny + r(DNAV) - r(6.0),
            },
        ));
        ny += r(DNAV);
    }

    // 页脚按钮：从右往左 确定 / 取消 / 应用
    let by = h - r(DFOOT) + (r(DFOOT) - r(DBTN_H)) / 2;
    let ok = RECT {
        left: cr - r(DBTN_W),
        top: by,
        right: cr,
        bottom: by + r(DBTN_H),
    };
    let cancel = RECT {
        left: ok.left - r(8.0) - r(DBTN_W),
        top: by,
        right: ok.left - r(8.0),
        bottom: by + r(DBTN_H),
    };
    let apply = RECT {
        left: cancel.left - r(8.0) - r(DBTN_W),
        top: by,
        right: cancel.left - r(8.0),
        bottom: by + r(DBTN_H),
    };
    hits.push((Hit::Ok, ok));
    hits.push((Hit::Cancel, cancel));
    hits.push((Hit::Apply, apply));

    match tab {
        Tab::Boxes => {
            let top = r(DTITLE) + r(64.0);
            let bottom = h - r(DFOOT) - r(ROW_AREA_BOTTOM);
            let row_h = r(DROW_H);
            let visible = (((bottom - top) / row_h).max(1)) as usize;
            for i in scroll..(scroll + visible).min(box_count) {
                let y = top + ((i - scroll) as i32) * row_h;
                let row = RECT {
                    left: cx - r(8.0),
                    top: y,
                    right: cr + r(8.0),
                    bottom: y + row_h - r(4.0),
                };
                // 先放整行，再放行内的小块：命中测试从后往前找，
                // 这样图标 / 名字 / 箭头会盖过「整行」的点击区
                hits.push((Hit::BoxRow(i), row));
                rows.push(row);
                hits.push((
                    Hit::BoxIcon(i),
                    RECT {
                        left: row.left + r(7.0),
                        top: y + r(5.0),
                        right: row.left + r(39.0),
                        bottom: y + r(37.0),
                    },
                ));
                hits.push((
                    Hit::BoxName(i),
                    RECT {
                        left: row.left + r(47.0),
                        top: y + r(5.0),
                        right: row.right - r(64.0),
                        bottom: y + r(37.0),
                    },
                ));
                hits.push((
                    Hit::BoxUp(i),
                    RECT {
                        left: row.right - r(58.0),
                        top: y + r(9.0),
                        right: row.right - r(34.0),
                        bottom: y + r(33.0),
                    },
                ));
                hits.push((
                    Hit::BoxDown(i),
                    RECT {
                        left: row.right - r(30.0),
                        top: y + r(9.0),
                        right: row.right - r(6.0),
                        bottom: y + r(33.0),
                    },
                ));
            }

            // 隐藏名称
            let ly = bottom + r(8.0);
            hits.push((
                Hit::Switch(Setting::HideLabel),
                RECT {
                    left: cr - r(42.0),
                    top: ly + r(6.0),
                    right: cr,
                    bottom: ly + r(28.0),
                },
            ));

            // 主界面：任务栏模式 / 桌面模式
            let my = bottom + r(70.0);
            let half = (cr - cx - r(8.0)) / 2;
            hits.push((
                Hit::Mode(false),
                RECT {
                    left: cx,
                    top: my,
                    right: cx + half,
                    bottom: my + r(32.0),
                },
            ));
            hits.push((
                Hit::Mode(true),
                RECT {
                    left: cx + half + r(8.0),
                    top: my,
                    right: cr,
                    bottom: my + r(32.0),
                },
            ));
        }
        Tab::Look => {
            hits.push((
                Hit::Slider,
                RECT {
                    left: cx,
                    top: r(140.0),
                    right: cr - r(58.0),
                    bottom: r(164.0),
                },
            ));
            hits.push((
                Hit::ResetColor,
                RECT {
                    left: cr - r(84.0),
                    top: r(192.0),
                    right: cr,
                    bottom: r(214.0),
                },
            ));
            // 「收纳盒」「弹出窗口」两块色卡并排放，一行看完
            let half = (cr - cx - r(8.0)) / 2;
            hits.push((
                Hit::Swatch(ColorTarget::Box),
                RECT {
                    left: cx,
                    top: r(218.0),
                    right: cx + half,
                    bottom: r(264.0),
                },
            ));
            hits.push((
                Hit::Swatch(ColorTarget::Panel),
                RECT {
                    left: cx + half + r(8.0),
                    top: r(218.0),
                    right: cr,
                    bottom: r(264.0),
                },
            ));
            for (s, top) in [(Setting::Acrylic, 300.0), (Setting::Radius, 356.0)] {
                hits.push((
                    Hit::Switch(s),
                    RECT {
                        left: cr - r(42.0),
                        top: r(top),
                        right: cr,
                        bottom: r(top) + r(22.0),
                    },
                ));
            }

            // 弹窗字体：字体名输入框 + 文字大小 − / +
            hits.push((
                Hit::FontName,
                RECT {
                    left: cx + r(104.0),
                    top: r(392.0),
                    right: cr,
                    bottom: r(420.0),
                },
            ));
            hits.push((
                Hit::FontMinus,
                RECT {
                    left: cr - r(140.0),
                    top: r(440.0),
                    right: cr - r(112.0),
                    bottom: r(468.0),
                },
            ));
            hits.push((
                Hit::FontPlus,
                RECT {
                    left: cr - r(28.0),
                    top: r(440.0),
                    right: cr,
                    bottom: r(468.0),
                },
            ));
        }
        Tab::Features => {
            for (s, top) in [
                (Setting::Autostart, 128.0),
                (Setting::Topmost, 184.0),
                (Setting::Diagnostics, 240.0),
            ] {
                hits.push((
                    Hit::Switch(s),
                    RECT {
                        left: cr - r(42.0),
                        top: r(top),
                        right: cr,
                        bottom: r(top) + r(22.0),
                    },
                ));
            }
            hits.push((
                Hit::OpenDir,
                RECT {
                    left: cr - r(80.0),
                    top: r(326.0),
                    right: cr,
                    bottom: r(356.0),
                },
            ));
        }
        Tab::About => {
            // 三行信息都可以点一下复制：整行都是命中区
            for i in 0..ABOUT_ROWS.len() {
                let y = r(166.0) + i as i32 * r(34.0);
                hits.push((
                    Hit::CopyInfo(i),
                    RECT {
                        left: cx,
                        top: y,
                        right: cr,
                        bottom: y + r(26.0),
                    },
                ));
            }
            // 「打开」压在第 3 行右侧：命中测试从后往前找，这一小块盖过整行
            hits.push((
                Hit::Link,
                RECT {
                    left: cr - r(62.0),
                    top: r(231.0),
                    right: cr,
                    bottom: r(257.0),
                },
            ));
            hits.push((
                Hit::CheckUpdate,
                RECT {
                    left: cx,
                    top: r(286.0),
                    right: cx + r(112.0),
                    bottom: r(320.0),
                },
            ));
            if has_update {
                hits.push((
                    Hit::Download,
                    RECT {
                        left: cx + r(120.0),
                        top: r(286.0),
                        right: cx + r(232.0),
                        bottom: r(320.0),
                    },
                ));
            }
        }
    }

    Layout { hits, rows }
}

/// 鼠标在某点落在哪个可点区域（从后往前找 = 小块优先于整行）
fn hit_at(x: i32, y: i32, scale: f32) -> Option<Hit> {
    let (tab, count, scroll) = {
        let s = state().lock().unwrap();
        (s.tab, s.draft.boxes.len(), s.scroll)
    };
    let lay = layout(scale, tab, count, scroll, crate::updater::has_update());
    lay.hits
        .iter()
        .rev()
        .find(|(_, r)| contains(r, x, y))
        .map(|(h, _)| *h)
}

/// 这个命中区能不能点（边界上的 ↑ / ↓、没改动时的「应用」是灰的）
fn clickable(h: Hit) -> bool {
    let (is_dirty, count) = {
        let s = state().lock().unwrap();
        (
            s.draft != s.orig || s.autostart != s.autostart_orig,
            s.draft.boxes.len(),
        )
    };
    match h {
        Hit::Apply => is_dirty,
        Hit::BoxUp(i) => i > 0,
        Hit::BoxDown(i) => i + 1 < count,
        _ => true,
    }
}

/// 当前显示器的缩放系数（96 DPI = 1.0）
fn display_scale() -> f32 {
    let cfg = app().cfg.lock().unwrap().clone();
    let mon = geom::work_area_at(cfg.bar.x, cfg.bar.y);
    mon.scale as f32 / 96.0
}

// —— 窗口 ——

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            // 输入框：文字颜色跟底色走，背景色用凹槽色
            let sunken = sunken_color(bg_color());
            let fg = theme::text_on(sunken);
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            let _ = SetTextColor(hdc, ui::colorref(fg.0, fg.1, fg.2));
            let _ = SetBkColor(hdc, ui::colorref(sunken.0, sunken.1, sunken.2));
            let brush = state().lock().unwrap().brush;
            match brush {
                Some(b) => LRESULT(b.0 as isize),
                None => LRESULT(0),
            }
        }
        WM_COMMAND => {
            // HIWORD(wp) = 通知码（EN_KILLFOCUS / CBN_SELCHANGE）
            // LOWORD(lp) = 发消息的控件句柄
            let notify_code = ((wp.0 >> 16) & 0xffff) as u32;
            let ctrl_hwnd = HWND(lp.0 as *mut core::ffi::c_void);
            // ComboBox 选择变化：提交字体名并隐藏下拉框
            let is_font_combo = state().lock().unwrap().font_combo == Some(ctrl_hwnd);
            if is_font_combo && notify_code == CBN_SELCHANGE {
                commit_font_from_combo();
                return LRESULT(0);
            }
            // 下拉被收起但没改选择（点别处 / Esc）：把浮出来的控件藏回去，
            // 否则它会一直盖在自绘的字体框上
            if is_font_combo && notify_code == CBN_CLOSEUP {
                // 选中提交时 SELCHANGE 已经把新值写进 draft，这里只收尾
                state().lock().unwrap().pending_font_combo = false;
                hide_font_combo();
                repaint();
                return LRESULT(0);
            }
            // 输入框失焦 = 提交名字
            if notify_code == 0x0200 {
                commit_inline_edit();
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_mouse_down(hwnd, x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_mouse_up();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            on_wheel(((wp.0 >> 16) & 0xffff) as i16);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = (wp.0 & 0xffff) as u32;
            if vk == VK_ESCAPE.0 as u32 {
                close();
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_TIMER => {
            // 「已复制」提示到点就抹掉
            if wp.0 == TIMER_COPIED {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_COPIED);
                }
                state().lock().unwrap().copied = None;
                repaint();
            }
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

fn create_window(scale: f32) -> Result<HWND, String> {
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
        let owner = app().hwnd.lock().unwrap().unwrap_or_default();
        let w = (DW * scale).round() as i32;
        let h = (DH * scale).round() as i32;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            // WS_CLIPCHILDREN：别让父窗口的绘制盖住子控件（名字输入框）——
            // 双缓冲贴整张位图时会把 EDIT 一起擦掉，输入框就会一闪一闪
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            w,
            h,
            Some(owner),
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        round_corners(hwnd, w, h);
        Ok(hwnd)
    }
}

/// 圆角：跟 draft 里的「圆角窗口」开关走。
/// Win11 用 DWM 的系统圆角，Win10 用 `SetWindowRgn` 裁一个圆角区域。
fn round_corners(hwnd: HWND, w: i32, h: i32) {
    let radius = state().lock().unwrap().draft.panel_radius;
    unsafe {
        if crate::taskbar::is_win11() {
            let pref = if radius {
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
            // 先把上一次的区域清掉，再按开关决定要不要圆角
            let _ = windows::Win32::Graphics::Gdi::SetWindowRgn(hwnd, None, false);
            if radius {
                let r = 10;
                let region = windows::Win32::Graphics::Gdi::CreateRoundRectRgn(
                    0,
                    0,
                    w + 1,
                    h + 1,
                    r * 2,
                    r * 2,
                );
                let _ = windows::Win32::Graphics::Gdi::SetWindowRgn(hwnd, Some(region), true);
            }
        }
    }
}

/// 自己的圆角跟着 draft 走（预览）
fn refresh_round() {
    let hwnd = state().lock().unwrap().hwnd;
    let Some(hwnd) = hwnd else { return };
    let scale = display_scale();
    let w = (DW * scale).round() as i32;
    let h = (DH * scale).round() as i32;
    round_corners(hwnd, w, h);
}

/// 名字输入框（建好先藏着，点哪一行就挪到哪一行）
fn create_edit(parent: HWND, scale: f32) -> HWND {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap();
        let cls = wide("EDIT");
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_CHILD | WS_TABSTOP,
            0,
            0,
            10,
            10,
            Some(parent),
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                log::info(&format!("创建名称输入框失败：{e}"));
                return HWND::default();
            }
        };
        let font = ui::create_font((14.0 * scale).round() as i32, false);
        let _ = SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
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
    if msg == WM_KEYDOWN {
        let vk = (wp.0 & 0xffff) as u32;
        if vk == VK_RETURN.0 as u32 {
            commit_inline_edit();
            return LRESULT(0);
        }
        if vk == VK_ESCAPE.0 as u32 {
            cancel_edit();
            return LRESULT(0);
        }
    }
    match EDIT_ORIG.get().copied().flatten() {
        Some(f) => CallWindowProcW(Some(f), hwnd, msg, wp, lp),
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// —— 字体下拉框 ——

/// 枚举系统已安装的字体（EnumFontFamiliesExW 回调）
///
/// windows 0.62 的回调签名是 `(*const LOGFONTW, *const TEXTMETRICW, FONTTYPE, LPARAM) -> i32`，
/// 直接从 LOGFONTW::lfFaceName 读字体族名即可。
unsafe extern "system" fn font_enum_proc(
    lplf: *const LOGFONTW,
    _lpntme: *const TEXTMETRICW,
    _font_type: u32,
    l_param: LPARAM,
) -> i32 {
    if lplf.is_null() {
        return 1; // 继续
    }
    let fonts = &mut *(l_param.0 as *mut Vec<String>);
    let face = (*lplf).lfFaceName;
    let name = crate::util::from_wide(&face);
    let name = name.trim_end_matches('\0').to_string();
    if !name.is_empty() && !fonts.contains(&name) {
        fonts.push(name);
    }
    1
}

/// 枚举系统所有已安装的字体名
fn enumerate_system_fonts() -> Vec<String> {
    unsafe {
        let dc = GetDC(None);
        let mut fonts: Vec<String> = Vec::new();
        let mut lf = LOGFONTW {
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        let ptr = &mut fonts as *mut Vec<String> as isize;
        let _ = EnumFontFamiliesExW(
            dc,
            &mut lf,
            Some(font_enum_proc),
            LPARAM(ptr),
            0,
        );
        let _ = ReleaseDC(None, dc);
        fonts
    }
}

/// 创建常驻的字体下拉框（默认隐藏；点「弹窗字体」时展开选择）
fn create_font_combo(parent: HWND, scale: f32) -> HWND {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap();
        let cls = wide("COMBOBOX");
        // DROPDOWNLIST | HASSTRINGS。**不要加 CBS_SORT**：排序后 ComboBox 里的
        // 顺序和 state.fonts 不一致，按索引取名字会写错字体。这里自己排序后填充。
        let combo_style = WINDOW_STYLE(0x0003u32 | 0x0200u32);
        // COMBOBOX 的下拉列表高度由「创建时的窗口高度 − 编辑行高」决定。
        // 早先这里给了 10px，下拉只有几像素高，点开等于没点开。
        let create_h = (300.0 * scale).round() as i32;
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_CHILD | WS_VSCROLL | combo_style,
            0,
            0,
            10,
            create_h,
            Some(parent),
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                log::info(&format!("创建字体下拉框失败：{e}"));
                return HWND::default();
            }
        };
        let font = ui::create_font((12.0 * scale).round() as i32, false);
        let _ = SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );

        // 枚举系统字体并填充：自己排序，保证「列表下标 → 字体名」一一对应
        let mut fonts = enumerate_system_fonts();
        fonts.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        fonts.dedup();
        // 用户当前用的字体可能不在枚举结果里（比如刚手工填的名字），补进去
        let cur_font = state().lock().unwrap().draft.panel_font.clone();
        if !cur_font.trim().is_empty() && !fonts.contains(&cur_font) {
            fonts.push(cur_font.clone());
        }
        let default_font = config::default_panel_font();

        {
            let mut s = state().lock().unwrap();
            s.fonts = fonts.clone();
        }

        for name in &fonts {
            let wname = wide(name);
            let _ = SendMessageW(
                hwnd,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(wname.as_ptr() as isize)),
            );
        }

        // 选中当前草稿里的字体（找不到就用默认，再找不到就用第一个）
        let idx = fonts
            .iter()
            .position(|f| f == &cur_font)
            .or_else(|| fonts.iter().position(|f| f == &default_font))
            .unwrap_or(0);
        let _ = SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(idx)), None);

        let _ = ShowWindow(hwnd, SW_HIDE);
        hwnd
    }
}

/// 收起下拉框时防止重入：CBN_SELCHANGE 里调它，CB_SHOWDROPDOWN(0) 会
/// 再发一次 CBN_CLOSEUP，那个分支又会调回来。
static HIDING_COMBO: AtomicBool = AtomicBool::new(false);

/// 收起并隐藏字体下拉框（选完了 / 点别处 / 切页 / 关窗口）
fn hide_font_combo() {
    if HIDING_COMBO.swap(true, Ordering::SeqCst) {
        return;
    }
    let combo = state().lock().unwrap().font_combo;
    if let Some(combo) = combo {
        unsafe {
            let _ = SendMessageW(combo, CB_SHOWDROPDOWN, Some(WPARAM(0)), None);
            let _ = ShowWindow(combo, SW_HIDE);
        }
    }
    HIDING_COMBO.store(false, Ordering::SeqCst);
}

/// 把字体下拉框挪到「外观 → 弹窗字体」位置并展开，让用户选择。
/// 只在鼠标抬起后调用：在 WM_LBUTTONDOWN 里展开会被紧接着的 mouse-up 收掉。
fn show_font_combo() {
    let scale = display_scale();
    let (hwnd, font_combo, cur_font) = {
        let s = state().lock().unwrap();
        (s.hwnd, s.font_combo, s.draft.panel_font.clone())
    };
    let (Some(parent), Some(combo)) = (hwnd, font_combo) else {
        return;
    };
    let lay = layout(scale, Tab::Look, 0, 0, crate::updater::has_update());
    let Some(fr) = hit_rect(&lay, Hit::FontName) else {
        return;
    };
    unsafe {
        let r = |v: f32| (v * scale).round() as i32;
        let row_h = fr.bottom - fr.top;
        let drop_h = r(220.0).max(row_h * 4);
        // 控件总高 = 露出的一行 + 下拉列表；只给行高的话下拉只有几像素
        let _ = SetWindowPos(
            combo,
            None,
            fr.left,
            fr.top,
            fr.right - fr.left,
            row_h + drop_h,
            SWP_NOZORDER,
        );
        // 字体名可能很长，让下拉比输入行宽一些
        let drop_w = (fr.right - fr.left).max(r(240.0));
        let _ = SendMessageW(
            combo,
            CB_SETDROPPEDWIDTH,
            Some(WPARAM(drop_w as usize)),
            None,
        );
        // 精确按名字定位当前项，不依赖自己记的索引（列表没用 CBS_SORT，
        // 但保留这个写法更稳，改回排序也不会错位）
        let cur = wide(&cur_font);
        let found = SendMessageW(
            combo,
            CB_FINDSTRINGEXACT,
            Some(WPARAM((-1i32) as usize)),
            Some(LPARAM(cur.as_ptr() as isize)),
        )
        .0 as i32;
        let _ = SendMessageW(
            combo,
            CB_SETCURSEL,
            Some(WPARAM(if found >= 0 { found as usize } else { 0 })),
            None,
        );

        let _ = ShowWindow(combo, SW_SHOW);
        let _ = SetFocus(Some(parent)); // 先让设置窗口成为前台，否则下拉会被立刻收起
        let _ = SetFocus(Some(combo));
        let _ = SendMessageW(combo, CB_SHOWDROPDOWN, Some(WPARAM(1)), None);
    }
}

/// 从 ComboBox 读取当前选中的字体名，写入 draft
fn commit_font_from_combo() {
    let font_combo = state().lock().unwrap().font_combo;
    let Some(combo) = font_combo else { return };
    unsafe {
        let idx = SendMessageW(combo, CB_GETCURSEL, None, None).0 as i32;
        if idx < 0 {
            hide_font_combo();
            return;
        }
        let len = SendMessageW(
            combo,
            CB_GETLBTEXTLEN,
            Some(WPARAM(idx as usize)),
            None,
        )
        .0 as i32;
        let mut name = String::new();
        if len > 0 {
            let mut buf = vec![0u16; (len as usize) + 1];
            let _ = SendMessageW(
                combo,
                CB_GETLBTEXTW,
                Some(WPARAM(idx as usize)),
                Some(LPARAM(buf.as_mut_ptr() as isize)),
            );
            name = crate::util::from_wide(&buf);
        }
        // 兜底：万一 CB_GETLBTEXT 取不到，用自己记的那份列表
        if name.trim().is_empty() {
            name = state()
                .lock()
                .unwrap()
                .fonts
                .get(idx as usize)
                .cloned()
                .unwrap_or_default();
        }
        if !name.trim().is_empty() {
            let mut s = state().lock().unwrap();
            s.draft.panel_font = name.trim().to_string();
            s.pending_font_combo = false;
        }
        hide_font_combo();
        let hwnd = state().lock().unwrap().hwnd;
        if let Some(h) = hwnd {
            let _ = SetFocus(Some(h));
        }
    }
    repaint();
}

/// 把窗口摆在鼠标附近（居中，再夹进工作区）
fn position(hwnd: HWND, scale: f32) {
    let r = |v: f32| (v * scale).round() as i32;
    let w = r(DW);
    let h = r(DH);
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let mon = geom::work_area_at(pt.x, pt.y);
    let min_x = mon.work.left + r(12.0);
    let min_y = mon.work.top + r(12.0);
    let max_x = (mon.work.right - w - r(12.0)).max(min_x);
    let max_y = (mon.work.bottom - h - r(12.0)).max(min_y);
    let x = (pt.x - w / 2).clamp(min_x, max_x);
    let y = (pt.y - h / 2).clamp(min_y, max_y);
    unsafe {
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

// —— 绘制 ——

fn paint(hwnd: HWND) {
    let scale = display_scale();
    let (
        tab,
        scroll,
        hover,
        selected,
        editing,
        draft,
        orig,
        auto_on,
        auto_orig,
        notes_scroll,
    ) = {
        let s = state().lock().unwrap();
        (
            s.tab,
            s.scroll,
            s.hover,
            s.selected,
            s.editing.clone(),
            s.draft.clone(),
            s.orig.clone(),
            s.autostart,
            s.autostart_orig,
            s.notes_scroll,
        )
    };
    let is_dirty = draft != orig || auto_on != auto_orig;
    let has_update = crate::updater::has_update();
    let lay = layout(scale, tab, draft.boxes.len(), scroll, has_update);

    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        // 双缓冲：先画在离屏位图上，画完一次性贴到窗口。
        // 直接往窗口上画是「先擦背景、再逐块画上去」，鼠标一动就重画一遍，
        // 用户能看见中间态 —— 那就是闪烁。
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

        let r = |v: f32| (v * scale).round() as i32;
        let c = Ctx {
            scale,
            h: ch,
            sx: r(DSIDEBAR),
            cx: r(DSIDEBAR) + r(DPAD),
            cr: cw - r(DPAD),
        };
        let bg = bg_color();
        let col = Colors {
            bg,
            fg: theme::text_on(bg),
            // 次要文字比通用 muted 再深一点：浅灰底上细字本来就显得软，
            // 按 0.62 收会偏虚，0.70 更接近系统设置的观感
            muted: theme::mix(bg, theme::text_on(bg), 0.70),
            accent: theme::accent_on(bg),
            line: line_color(bg),
            sunken: sunken_color(bg),
        };

        // —— 底色 + 左侧导航 ——
        g.fill_rect(0, 0, cw, ch, bg);
        let sidebar = theme::mix(bg, col.fg, 0.045);
        g.fill_rect(0, 0, c.sx, ch, sidebar);
        g.font(r(14.0), true);
        g.text(
            r(20.0),
            r(10.0),
            c.sx - r(40.0),
            r(30.0),
            "LightBox",
            col.fg,
            Align::Left,
        );
        for t in TABS {
            let Some(nr) = hit_rect(&lay, Hit::Nav(t)) else { continue };
            let active = t == tab;
            let hot = hover == Some(Hit::Nav(t));
            if active {
                g.fill_round(
                    nr.left,
                    nr.top,
                    nr.right - nr.left,
                    nr.bottom - nr.top,
                    r(7.0),
                    theme::mix(bg, col.accent, 0.16),
                );
            } else if hot {
                g.fill_round(
                    nr.left,
                    nr.top,
                    nr.right - nr.left,
                    nr.bottom - nr.top,
                    r(7.0),
                    theme::mix(bg, col.fg, 0.08),
                );
            }
            g.font(r(13.0), active);
            g.text(
                nr.left + r(12.0),
                nr.top,
                nr.right - nr.left - r(20.0),
                nr.bottom - nr.top,
                t.label(),
                if active { col.accent } else { col.fg },
                Align::Left,
            );
        }
        g.fill_rect(c.sx - 1, 0, 1, ch, col.line);

        // —— 页头 ——
        let title_w = cw - c.cx - r(50.0);
        g.font(r(17.0), true);
        g.text(c.cx, r(8.0), title_w, r(28.0), tab.title(), col.fg, Align::Left);
        g.font(r(11.0), false);
        g.text(
            c.cx,
            r(36.0),
            title_w,
            r(18.0),
            tab.subtitle(),
            col.muted,
            Align::Left,
        );

        // —— 关闭按钮 ——
        if let Some(cr) = hit_rect(&lay, Hit::Close) {
            if hover == Some(Hit::Close) {
                g.fill_round(
                    cr.left,
                    cr.top,
                    cr.right - cr.left,
                    cr.bottom - cr.top,
                    r(6.0),
                    theme::mix(bg, col.fg, 0.14),
                );
            }
            g.font(r(14.0), false);
            g.text(
                cr.left,
                cr.top,
                cr.right - cr.left,
                cr.bottom - cr.top,
                "✕",
                col.fg,
                Align::Center,
            );
        }

        // —— 当前页 ——
        match tab {
            Tab::Boxes => paint_boxes(
                &mut g, &c, &lay, &col, &draft, hover, selected, scroll, &editing,
            ),
            Tab::Look => paint_look(&mut g, &c, &lay, &col, &draft, hover),
            Tab::Features => paint_features(&mut g, &c, &lay, &col, &draft, hover, auto_on),
            Tab::About => paint_about(&mut g, &c, &lay, &col, hover, has_update, notes_scroll),
        }

        // 字体下拉框不能在重绘里隐藏：鼠标移动 / hover 变化都会 repaint()，
        // 刚展开的 ComboBox 会被立刻藏回去，表现就是「字体选择框点不开」。
        // 收起统一交给 hide_font_combo()（选完 / CBN_CLOSEUP / 切页 / 关窗口）。

        // —— 页脚 ——
        let fy = ch - r(DFOOT);
        g.hline(c.sx, cw, fy, col.line);
        if is_dirty {
            g.font(r(11.0), false);
            g.text(
                c.cx,
                fy,
                r(220.0),
                r(DFOOT),
                "有未应用的修改",
                col.accent,
                Align::Left,
            );
        }
        for (hit, label, kind) in [
            (Hit::Apply, "应用", if is_dirty { 1 } else { 0 }),
            (Hit::Cancel, "取消", 1),
            (Hit::Ok, "确定", 2),
        ] {
            if let Some(rect) = hit_rect(&lay, hit) {
                draw_button(&mut g, &c, &col, rect, label, kind, hover == Some(hit));
            }
        }

        // 防撞色描边：设置页和弹窗底色相同，叠在一起分不清前后。
        // 1px 对比色（由底色推导）贴着圆角内侧描一圈，谁是上面的窗口一眼可见。
        let edge_rad = if draft.panel_radius {
            if crate::taskbar::is_win11() {
                8
            } else {
                10
            }
        } else {
            0
        };
        g.stroke_round(
            1,
            1,
            cw - 2,
            ch - 2,
            edge_rad,
            theme::mix(bg, col.fg, 0.32),
            1,
        );

        // 一次性贴到窗口：用户只看到画完的一帧，中间态不会露出来
        drop(g);
        if let Some(b) = &buf {
            b.blit_to(hdc);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

fn paint_boxes(
    g: &mut Gfx,
    c: &Ctx,
    lay: &Layout,
    col: &Colors,
    draft: &config::Config,
    hover: Option<Hit>,
    selected: usize,
    scroll: usize,
    editing: &Option<String>,
) {
    let r = |v: f32| c.r(v);
    let top = r(DTITLE) + r(64.0);
    let bottom = c.h - r(DFOOT) - r(ROW_AREA_BOTTOM);

    if draft.boxes.is_empty() {
        g.font(r(13.0), false);
        g.text(
            c.cx,
            top + r(36.0),
            c.cr - c.cx,
            r(24.0),
            "还没有收纳盒。右键长条，选「新建收纳盒」。",
            col.muted,
            Align::Left,
        );
    }

    for (k, row) in lay.rows.iter().enumerate() {
        let i = scroll + k;
        let Some(b) = draft.boxes.get(i) else { continue };
        let hot = matches!(
            hover,
            Some(Hit::BoxRow(j)) | Some(Hit::BoxName(j)) | Some(Hit::BoxIcon(j)) | Some(Hit::BoxUp(j)) | Some(Hit::BoxDown(j)) if j == i
        );
        let row_bg = if i == selected {
            theme::mix(col.bg, col.accent, 0.14)
        } else if hot {
            theme::mix(col.bg, col.fg, 0.08)
        } else {
            col.bg
        };
        if i == selected || hot {
            g.fill_round(
                row.left,
                row.top,
                row.right - row.left,
                row.bottom - row.top,
                r(8.0),
                row_bg,
            );
        }
        if i == selected {
            g.fill_round(
                row.left + r(2.0),
                row.top + r(10.0),
                r(3.0),
                row.bottom - row.top - r(20.0),
                r(1.5),
                col.accent,
            );
        }

        // 图标
        if let Some(ir) = hit_rect(lay, Hit::BoxIcon(i)) {
            draw_box_icon(
                g,
                ir,
                b,
                row_bg,
                col.accent,
                hover == Some(Hit::BoxIcon(i)),
                c.scale,
            );
        }

        // 名字（正在编辑的那一行由 EDIT 自己显示）
        if editing.as_deref() != Some(b.id.as_str()) {
            if let Some(nr) = hit_rect(lay, Hit::BoxName(i)) {
                if hover == Some(Hit::BoxName(i)) {
                    g.fill_round(
                        nr.left,
                        nr.top,
                        nr.right - nr.left,
                        nr.bottom - nr.top,
                        r(6.0),
                        theme::mix(row_bg, col.fg, 0.06),
                    );
                }
                g.font(r(13.0), false);
                let tw = (nr.right - nr.left - r(12.0)).max(r(20.0));
                let shown = ui::ellipsize(&b.name, tw, |s| g.text_w(s));
                g.text(
                    nr.left + r(6.0),
                    nr.top,
                    tw,
                    nr.bottom - nr.top,
                    &shown,
                    col.fg,
                    Align::Left,
                );
            }
        }

        // 上移 / 下移：标准 ⬆ / ⬇ 比例的蓝色实心箭头（见 ui::Gfx::arrow）。
        // 普通态 = 完整强调蓝（浅底 #2F6FEB / 深底 #6CA8FF），
        // 悬停态 = 往文字色靠一点做加深反馈，禁用态 = 灰。
        // 之前用 mix(bg, accent, 0.45) 得到的是发灰的淡蓝，加上画刷被换空，
        // 实际渲染成黑色空心轮廓 —— 这里直接用强调色。
        let last = draft.boxes.len().saturating_sub(1);
        for (hit, up, on) in [
            (Hit::BoxUp(i), true, i > 0),
            (Hit::BoxDown(i), false, i < last),
        ] {
            let Some(ar) = hit_rect(lay, hit) else { continue };
            let ah = hover == Some(hit);
            let hover_bg = theme::mix(col.bg, col.accent, 0.2);
            if ah && on {
                g.fill_round(
                    ar.left,
                    ar.top,
                    ar.right - ar.left,
                    ar.bottom - ar.top,
                    r(6.0),
                    hover_bg,
                );
            }
            let bg_here = if ah && on { hover_bg } else { col.bg };
            let color = if !on {
                theme::muted_on(bg_here)
            } else if ah {
                // 浅底往黑靠 = 深蓝，深底往白靠 = 亮蓝，两种主题都有「按下去」的反馈
                theme::mix(col.accent, theme::text_on(bg_here), 0.20)
            } else {
                col.accent
            };
            g.arrow(
                (ar.left + ar.right) / 2,
                (ar.top + ar.bottom) / 2,
                // 外接框 16×20 → 12.8×16（缩小 20%）：原来贴着格子边显得太满，
                // 缩一号后留白更匀，蓝色实心也不至于糊成一块。
                r(12.8),
                r(16.0),
                up,
                color,
                r(5.6), // 箭杆整宽：约为头宽的 44%，跟 ⬆ / ⬇ 一致
            );
        }
    }

    // 隐藏名称
    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::HideLabel),
        "嵌入任务栏时隐藏盒子名称",
        "打开后任务栏里只显示图标，不显示名字",
        draft.bar_hide_label,
    );

    // 主界面形态
    let my = bottom + r(70.0);
    g.font(r(13.0), false);
    g.text(
        c.cx,
        bottom + r(46.0),
        c.cr - c.cx,
        r(20.0),
        "主界面",
        col.fg,
        Align::Left,
    );
    for (val, label) in [(false, "任务栏模式"), (true, "桌面模式")] {
        let Some(mr) = hit_rect(lay, Hit::Mode(val)) else { continue };
        let active = draft.bar.desktop == val;
        let hot = hover == Some(Hit::Mode(val));
        let fill = if active {
            col.accent
        } else if hot {
            theme::mix(col.bg, col.fg, 0.14)
        } else {
            theme::mix(col.bg, col.fg, 0.07)
        };
        g.fill_round(
            mr.left,
            mr.top,
            mr.right - mr.left,
            mr.bottom - mr.top,
            r(7.0),
            fill,
        );
        g.font(r(12.0), active);
        let tc = if active {
            theme::text_on(col.accent)
        } else {
            col.fg
        };
        g.text(
            mr.left,
            mr.top,
            mr.right - mr.left,
            mr.bottom - mr.top,
            label,
            tc,
            Align::Center,
        );
    }
    g.font(r(11.0), false);
    g.text(
        c.cx,
        my + r(38.0),
        c.cr - c.cx,
        r(18.0),
        "任务栏模式嵌在任务栏里；桌面模式浮在桌面上，可拖到任意位置",
        col.muted,
        Align::Left,
    );
}

fn paint_look(
    g: &mut Gfx,
    c: &Ctx,
    lay: &Layout,
    col: &Colors,
    draft: &config::Config,
    hover: Option<Hit>,
) {
    let r = |v: f32| c.r(v);

    // 透明度
    let t = (draft.panel_transparency.clamp(0.0, 50.0) / 50.0) as f32;
    g.font(r(13.0), false);
    g.text(c.cx, r(110.0), r(200.0), r(22.0), "透明度", col.fg, Align::Left);
    g.text(
        c.cr - r(56.0),
        r(110.0),
        r(56.0),
        r(22.0),
        &format!("{:.0}%", draft.panel_transparency.clamp(0.0, 50.0).round()),
        col.fg,
        Align::Right,
    );
    if let Some(sr) = hit_rect(lay, Hit::Slider) {
        let ty = (sr.top + sr.bottom) / 2;
        let tw = sr.right - sr.left;
        g.fill_round(
            sr.left,
            ty - r(2.0),
            tw,
            r(4.0),
            r(2.0),
            theme::mix(col.bg, col.fg, 0.22),
        );
        let kx = sr.left + (tw as f32 * t).round() as i32;
        g.fill_round(
            kx - r(7.0),
            ty - r(7.0),
            r(14.0),
            r(14.0),
            r(7.0),
            col.accent,
        );
    }
    g.font(r(11.0), false);
    g.text(
        c.cx,
        r(166.0),
        c.cr - c.cx,
        r(18.0),
        "0% 完全不透明，50% 最透",
        col.muted,
        Align::Left,
    );

    // 颜色
    g.font(r(13.0), false);
    g.text(c.cx, r(192.0), r(200.0), r(22.0), "颜色", col.fg, Align::Left);
    if let Some(rr) = hit_rect(lay, Hit::ResetColor) {
        let hot = hover == Some(Hit::ResetColor);
        g.font(r(11.0), false);
        g.text(
            rr.left,
            rr.top,
            rr.right - rr.left,
            rr.bottom - rr.top,
            "恢复默认",
            if hot { col.accent } else { col.muted },
            Align::Right,
        );
    }
    for (target, label) in [(ColorTarget::Box, "收纳盒"), (ColorTarget::Panel, "弹出窗口")] {
        let Some(rect) = hit_rect(lay, Hit::Swatch(target)) else { continue };
        let hex = match target {
            ColorTarget::Box => &draft.box_bg,
            ColorTarget::Panel => &draft.panel_bg,
        };
        let rgb = crate::util::parse_hex(hex).unwrap_or((0x5A, 0x60, 0x6B));
        let hot = hover == Some(Hit::Swatch(target));
        let sh = rect.bottom - rect.top;
        g.fill_round(
            rect.left,
            rect.top,
            rect.right - rect.left,
            sh,
            r(7.0),
            if hot {
                theme::mix(col.bg, col.fg, 0.14)
            } else {
                theme::mix(col.bg, col.fg, 0.07)
            },
        );
        let sw = r(26.0);
        g.swatch(rect.left + r(8.0), rect.top + (sh - sw) / 2, sw, sw, rgb);
        g.font(r(12.0), false);
        g.text(
            rect.left + r(46.0),
            rect.top,
            r(110.0),
            sh,
            label,
            col.fg,
            Align::Left,
        );
        g.font(r(11.0), false);
        g.text(
            rect.left + r(46.0),
            rect.top,
            rect.right - rect.left - r(56.0),
            sh,
            &hex.to_uppercase(),
            col.muted,
            Align::Right,
        );
    }

    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::Acrylic),
        "毛玻璃",
        "弹出窗口背后做模糊，像磨砂玻璃",
        draft.panel_acrylic,
    );
    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::Radius),
        "圆角窗口",
        "弹出窗口和本设置页四角为圆角；关掉是直角",
        draft.panel_radius,
    );

    // —— 弹窗字体 ——
    // 设置只作用于点击收纳盒弹出的那个面板（标题、图标名、提示、改名框），
    // 长条胶囊和色块首字尺寸固定，不跟着放大，免得字撑出格子。
    g.hline(c.cx, c.cr, r(388.0), col.line);
    g.font(r(13.0), false);
    g.text(
        c.cx,
        r(392.0),
        r(100.0),
        r(28.0),
        "弹窗字体",
        col.fg,
        Align::Left,
    );
    if let Some(fr) = hit_rect(lay, Hit::FontName) {
        let fw = fr.right - fr.left;
        let fh = fr.bottom - fr.top;
        g.fill_round(fr.left, fr.top, fw, fh, r(6.0), theme::mix(col.bg, col.fg, 0.05));
        g.stroke_round(
            fr.left,
            fr.top,
            fw,
            fh,
            r(6.0),
            theme::mix(col.bg, col.fg, 0.24),
            1,
        );
        // 下拉箭头 ▼（右边预留 20px 给箭头）
        let arrow_area = r(20.0);
        let arrow_left = fr.right - arrow_area;
        g.font(r(10.0), true);
        // 用实心小三角作为下拉指示（用 GDI 画比 unicode 字符更稳）
        // arrow_area 已经是像素，别再乘一次缩放
        let ax = arrow_left + arrow_area / 2;
        let ay = (fr.top + fr.bottom) / 2;
        g.arrow(
            ax,
            ay,
            r(12.0),
            r(8.0),
            false, // 下箭头
            col.muted,
            0, // 只要一个等腰三角，不要箭杆
        );
        // 字体名（文字区域减去箭头空间）
        g.font(r(12.0), false);
        let shown = ui::ellipsize(&draft.panel_font, fw - arrow_area - r(16.0), |s| g.text_w(s));
        g.text(
            fr.left + r(8.0),
            fr.top,
            fw - arrow_area - r(8.0),
            fh,
            &shown,
            col.fg,
            Align::Left,
        );
    }
    g.font(r(11.0), false);
    g.text(
        c.cx,
        r(421.0),
        c.cr - c.cx,
        r(18.0),
        "点上方选择框，从系统已安装字体中挑选；点「应用」后生效",
        col.muted,
        Align::Left,
    );

    // 文字大小
    g.font(r(13.0), false);
    g.text(
        c.cx,
        r(440.0),
        r(120.0),
        r(28.0),
        "文字大小",
        col.fg,
        Align::Left,
    );
    let size = draft
        .panel_font_size
        .clamp(config::FONT_SIZE_MIN, config::FONT_SIZE_MAX);
    let minus = hit_rect(lay, Hit::FontMinus);
    let plus = hit_rect(lay, Hit::FontPlus);
    if let (Some(mr), Some(pr)) = (minus, plus) {
        let can_minus = size > config::FONT_SIZE_MIN;
        let can_plus = size < config::FONT_SIZE_MAX;
        draw_button(
            g,
            c,
            col,
            mr,
            "−",
            if can_minus { 1 } else { 0 },
            hover == Some(Hit::FontMinus) && can_minus,
        );
        draw_button(
            g,
            c,
            col,
            pr,
            "+",
            if can_plus { 1 } else { 0 },
            hover == Some(Hit::FontPlus) && can_plus,
        );
        g.font(r(12.0), false);
        g.text(
            mr.right,
            mr.top,
            pr.left - mr.right,
            mr.bottom - mr.top,
            &format!("{} px", size.round() as i32),
            col.fg,
            Align::Center,
        );
    }

    // 预览：用当前草稿的字体和字号，画在弹窗底色上，改之前就能看出样子
    let py = r(472.0);
    let ph = r(24.0);
    let bg = crate::util::parse_hex(&draft.panel_bg).unwrap_or((0x5A, 0x60, 0x6B));
    g.fill_round(c.cx, py, c.cr - c.cx, ph, r(6.0), bg);
    g.font_named(r(size), false, &draft.panel_font);
    g.text(
        c.cx + r(10.0),
        py,
        c.cr - c.cx - r(20.0),
        ph,
        "LightBox  收纳盒  ABC 123",
        theme::text_on(bg),
        Align::Left,
    );
}

fn paint_features(
    g: &mut Gfx,
    c: &Ctx,
    lay: &Layout,
    col: &Colors,
    draft: &config::Config,
    hover: Option<Hit>,
    auto_on: bool,
) {
    let r = |v: f32| c.r(v);

    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::Autostart),
        "开机自启",
        "登录 Windows 后自动运行 LightBox",
        auto_on,
    );
    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::Topmost),
        "总是置顶",
        "长条浮在普通窗口之上（任务栏模式自动置顶）",
        draft.bar.topmost,
    );
    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::Diagnostics),
        "打印诊断信息",
        "把排障日志写到 lightbox.log；平时关着，出问题再打开",
        draft.diagnostics,
    );

    // 存储位置
    g.font(r(13.0), false);
    g.text(
        c.cx,
        r(300.0),
        r(200.0),
        r(22.0),
        "存储位置",
        col.fg,
        Align::Left,
    );
    let btn = hit_rect(lay, Hit::OpenDir).unwrap_or_default();
    let pw = (btn.left - c.cx - r(10.0)).max(r(60.0));
    let dir = config::root_dir().display().to_string();
    g.font(r(11.0), false);
    let shown = ui::ellipsize(&dir, pw, |s| g.text_w(s));
    g.text(c.cx, r(330.0), pw, r(22.0), &shown, col.muted, Align::Left);
    if let Some(rect) = hit_rect(lay, Hit::OpenDir) {
        draw_button(g, c, col, rect, "打开目录", 1, hover == Some(Hit::OpenDir));
    }
    g.font(r(11.0), false);
    g.text(
        c.cx,
        r(362.0),
        c.cr - c.cx,
        r(18.0),
        "配置和收纳盒里的快捷方式都存在这个目录，可整个拷走备份",
        col.muted,
        Align::Left,
    );
}

fn paint_about(
    g: &mut Gfx,
    c: &Ctx,
    lay: &Layout,
    col: &Colors,
    hover: Option<Hit>,
    has_update: bool,
    notes_scroll: usize,
) {
    let r = |v: f32| c.r(v);

    g.font(r(26.0), true);
    g.text(
        c.cx,
        r(72.0),
        c.cr - c.cx,
        r(36.0),
        "LightBox",
        col.fg,
        Align::Left,
    );
    g.font(r(12.0), false);
    g.text(
        c.cx,
        r(112.0),
        c.cr - c.cx,
        r(20.0),
        &format!("版本 v{}", env!("CARGO_PKG_VERSION")),
        col.muted,
        Align::Left,
    );
    g.hline(c.cx, c.cr, r(146.0), col.line);

    // 三行信息都可以点一下复制（作者 / 微信 / 开源链接），
    // 悬停变强调色、复制后右侧冒出「已复制」。开源链接另有「打开」按钮。
    let copied = state().lock().unwrap().copied;
    for (i, (label, value)) in ABOUT_ROWS.iter().enumerate() {
        let y = r(166.0) + i as i32 * r(34.0);
        let is_link = i == 2;
        let hot = hover == Some(Hit::CopyInfo(i));
        g.font(r(12.0), false);
        g.text(c.cx, y, r(90.0), r(24.0), label, col.muted, Align::Left);

        // 第 3 行右侧留给「打开」按钮，文字宽度要收一收
        let right = if is_link { c.cr - r(70.0) } else { c.cr };
        let vw = (right - (c.cx + r(96.0))).max(r(40.0));
        g.font(r(12.0), is_link);
        let color = if hot {
            theme::mix(col.accent, col.fg, 0.2)
        } else if is_link {
            col.accent
        } else {
            col.fg
        };
        g.text(c.cx + r(96.0), y, vw, r(24.0), value, color, Align::Left);

        // 「已复制」提示紧跟文字右侧，1.4 秒后自动消失
        if copied == Some(i) {
            let tw = g.text_w(value).min(vw);
            g.font(r(11.0), false);
            g.text(
                c.cx + r(96.0) + tw + r(8.0),
                y,
                r(64.0),
                r(24.0),
                "已复制",
                col.accent,
                Align::Left,
            );
        }
    }
    if let Some(rect) = hit_rect(lay, Hit::Link) {
        draw_button(g, c, col, rect, "打开", 1, hover == Some(Hit::Link));
    }
    // 可复制这件事得说出来，不然没人会想到去点文字
    g.font(r(11.0), false);
    g.text(
        c.cx,
        r(264.0),
        c.cr - c.cx,
        r(18.0),
        "点一下作者 / 微信 / 链接即可复制到剪贴板",
        col.muted,
        Align::Left,
    );

    if let Some(rect) = hit_rect(lay, Hit::CheckUpdate) {
        draw_button(
            g,
            c,
            col,
            rect,
            "检查更新",
            1,
            hover == Some(Hit::CheckUpdate),
        );
    }
    if has_update {
        if let Some(rect) = hit_rect(lay, Hit::Download) {
            draw_button(g, c, col, rect, "下载新版", 2, hover == Some(Hit::Download));
        }
    }
    let (status, accent) = update_status();
    let stx = if has_update {
        c.cx + r(244.0)
    } else {
        c.cx + r(120.0)
    };
    let sw = (c.cr - stx).max(r(40.0));
    let shown = ui::ellipsize(&status, sw, |s| g.text_w(s));
    g.font(r(11.0), false);
    g.text(
        stx,
        r(286.0),
        sw,
        r(34.0),
        &shown,
        if accent { col.accent } else { col.muted },
        Align::Left,
    );

    // 有更新时在按钮下方展示清单里的完整信息（滚轮可翻）
    if has_update {
        paint_update_box(g, c, col, notes_scroll);
    }
}

/// 关于页「更新信息」文本框的内容：第一行版本 + 日期，之后一行一条说明。
fn update_notes_lines() -> Vec<String> {
    let Some(m) = crate::updater::latest_manifest() else {
        return Vec::new();
    };
    let head = if m.date.is_empty() {
        format!("发现新版本 v{}", m.latest)
    } else {
        format!("发现新版本 v{}（{}）", m.latest, m.date)
    };
    let mut out = vec![head];
    for n in &m.notes {
        out.push(format!("· {n}"));
    }
    if out.len() == 1 {
        out.push("（版本清单里没有更新说明）".into());
    }
    out
}

/// 文本框里一屏能放几行说明（标题行另算）
fn notes_visible(scale: f32) -> usize {
    let r = |v: f32| (v * scale).round() as i32;
    let first = r(DNOTES_TOP) + r(8.0) + r(26.0);
    let bottom = r(DNOTES_BOTTOM) - r(8.0);
    let lh = r(DNOTES_LINE).max(1);
    ((bottom - first) / lh).max(1) as usize
}

/// 画「更新信息」文本框。
/// 用自绘而不是真 EDIT：这里只是展示，不需要输入/复制，也省掉分层窗口挂控件的坑。
fn paint_update_box(g: &mut Gfx, c: &Ctx, col: &Colors, scroll: usize) {
    let r = |v: f32| c.r(v);
    let x = c.cx;
    let y = r(DNOTES_TOP);
    let w = c.cr - c.cx;
    let h = r(DNOTES_BOTTOM) - y;
    if w <= 0 || h <= 0 {
        return;
    }
    g.fill_round(x, y, w, h, r(8.0), col.sunken);
    g.stroke_round(x, y, w, h, r(8.0), col.line, 1);

    let lines = update_notes_lines();
    if lines.is_empty() {
        return;
    }
    let pad = r(12.0);
    let inner_w = w - pad * 2;
    let body = &lines[1..];
    let visible = notes_visible(c.scale);
    let start = scroll.min(body.len().saturating_sub(visible));
    let more = start + visible < body.len();

    // 标题行（右侧留一块给「滚轮查看更多」提示）
    let hint_w = r(96.0);
    let head_w = (inner_w - hint_w).max(r(40.0));
    g.font(r(12.0), true);
    let head = ui::ellipsize(&lines[0], head_w, |s| g.text_w(s));
    g.text(
        x + pad,
        y + r(8.0),
        head_w,
        r(20.0),
        &head,
        col.accent,
        Align::Left,
    );
    if more {
        g.font(r(10.0), false);
        g.text(
            x + w - pad - hint_w,
            y + r(9.0),
            hint_w,
            r(18.0),
            "滚轮查看更多",
            col.muted,
            Align::Right,
        );
    }

    let first = y + r(8.0) + r(26.0);
    let lh = r(DNOTES_LINE);
    g.font(r(11.0), false);
    for (i, line) in body.iter().skip(start).take(visible).enumerate() {
        let shown = ui::ellipsize(line, inner_w, |s| g.text_w(s));
        g.text(
            x + pad,
            first + (i as i32) * lh,
            inner_w,
            lh,
            &shown,
            col.fg,
            Align::Left,
        );
    }
}

/// 检查更新的状态文字（页面内显示，不弹模态框）
fn update_status() -> (String, bool) {
    if crate::updater::is_busy() {
        return ("正在检查…".into(), false);
    }
    if !crate::updater::last_error().is_empty() {
        return ("检查失败（网络或地址问题）".into(), false);
    }
    if crate::updater::has_update() {
        let v = crate::updater::latest_manifest()
            .map(|m| m.latest)
            .unwrap_or_default();
        return (format!("发现新版本 v{v}"), true);
    }
    if crate::updater::latest_manifest().is_some() {
        return ("已是最新版本".into(), false);
    }
    ("点「检查更新」联网获取最新版本".into(), false)
}

/// 一行开关：左边标题 + 说明，右边开关
fn draw_switch_row(
    g: &mut Gfx,
    c: &Ctx,
    lay: &Layout,
    col: &Colors,
    hit: Hit,
    title: &str,
    desc: &str,
    on: bool,
) {
    let Some(rect) = hit_rect(lay, hit) else { return };
    let text_w = (rect.left - c.cx - c.r(16.0)).max(c.r(60.0));
    g.font(c.r(13.0), false);
    g.text(
        c.cx,
        rect.top - c.r(14.0),
        text_w,
        c.r(20.0),
        title,
        col.fg,
        Align::Left,
    );
    g.font(c.r(11.0), false);
    let shown = ui::ellipsize(desc, text_w, |s| g.text_w(s));
    g.text(
        c.cx,
        rect.top + c.r(7.0),
        text_w,
        c.r(18.0),
        &shown,
        col.muted,
        Align::Left,
    );
    g.switch(
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        on,
    );
}

/// 按钮：0 = 禁用，1 = 次要，2 = 主要（强调色）
fn draw_button(
    g: &mut Gfx,
    c: &Ctx,
    col: &Colors,
    rect: RECT,
    label: &str,
    kind: i32,
    hot: bool,
) {
    let (x, y) = (rect.left, rect.top);
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    // 浅色底上：次要按钮用近白底 + 描边，和灰色页面明确分层；
    // 禁用态只用中灰文字，不再靠「把强调色变淡」区分（浅底上那样最容易糊在一起）
    let (fill, text, border) = match kind {
        2 => (
            if hot {
                theme::mix(col.accent, theme::SNOW, 0.12)
            } else {
                col.accent
            },
            theme::text_on(col.accent),
            None,
        ),
        1 => (
            if hot {
                theme::mix(col.sunken, col.accent, 0.08)
            } else {
                col.sunken
            },
            col.fg,
            Some(theme::mix(col.bg, col.fg, 0.24)),
        ),
        _ => (
            theme::mix(col.bg, col.fg, 0.05),
            theme::muted_on(col.bg),
            None,
        ),
    };
    g.fill_round(x, y, w, h, c.r(7.0), fill);
    if let Some(bd) = border {
        g.stroke_round(x, y, w, h, c.r(7.0), bd, 1);
    }
    g.font(c.r(12.0), false);
    g.text(x, y, w, h, label, text, Align::Center);
}

// —— 图标 ——

fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// 盒子的自定义图标（解码后缓存）
fn box_icon_bitmap(b: &config::BoxConfig) -> Option<IconBmp> {
    let data = b.icon.as_ref()?;
    let key = format!("{}:{:x}", b.id, hash_str(data));
    {
        let cache = ICON_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
    }
    let value = icons::decode_png_data_url(data);
    ICON_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(key, value.clone());
    value
}

fn draw_box_icon(
    g: &mut Gfx,
    rect: RECT,
    b: &config::BoxConfig,
    bg: (u8, u8, u8),
    accent: (u8, u8, u8),
    hot: bool,
    scale: f32,
) {
    let r = |v: f32| (v * scale).round() as i32;
    let side = rect.right - rect.left;
    if hot {
        g.fill_round(
            rect.left - r(2.0),
            rect.top - r(2.0),
            side + r(4.0),
            side + r(4.0),
            r(8.0),
            theme::mix(bg, accent, 0.25),
        );
    }
    match box_icon_bitmap(b) {
        Some(src) => {
            let tmp = scale_fit(&src, side);
            blit_bgra(
                g.hdc,
                rect.left + (side - tmp.w) / 2,
                rect.top + (side - tmp.h) / 2,
                tmp.w,
                tmp.h,
                bg,
                &tmp.px,
            );
        }
        None => {
            // 没有自定义图标 → 内置「收纳盒」图标（和任务栏胶囊、v0.9.33 一致）
            let glyph = icons::box_glyph(side);
            blit_bgra(
                g.hdc,
                rect.left + (side - glyph.w) / 2,
                rect.top + (side - glyph.h) / 2,
                glyph.w,
                glyph.h,
                bg,
                &glyph.px,
            );
        }
    }
}

/// 等比缩放到不超过 size（允许轻微放大，小图标也别留空）
fn scale_fit(src: &IconBmp, size: i32) -> IconBmp {
    if src.w <= 0 || src.h <= 0 || size <= 0 {
        return IconBmp::empty(0, 0);
    }
    let k = size as f32 / src.w.max(src.h) as f32;
    let dw = ((src.w as f32) * k).round().max(1.0) as i32;
    let dh = ((src.h as f32) * k).round().max(1.0) as i32;
    if dw == src.w && dh == src.h {
        src.clone()
    } else {
        scale_bilinear(src, dw, dh)
    }
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

/// 把一张非预乘 BGRA 图标贴到 GDI 上。
///
/// GDI 的 `BitBlt` 不做 alpha 混合，所以先在一张 DIB 里把图标和底色逐像素合成
/// （合完 alpha 全是 255），再一次性贴过去。
fn blit_bgra(dst: HDC, x: i32, y: i32, w: i32, h: i32, bg: (u8, u8, u8), px: &[u8]) {
    if w <= 0 || h <= 0 || px.len() < (w * h * 4) as usize {
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
        match CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(hbmp) => {
                if !bits.is_null() {
                    let out = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
                    for i in 0..(w * h) as usize {
                        let si = i * 4;
                        let a = px[si + 3] as u32;
                        let inv = 255 - a;
                        out[si] = ((px[si] as u32 * a + bg.2 as u32 * inv) / 255) as u8;
                        out[si + 1] = ((px[si + 1] as u32 * a + bg.1 as u32 * inv) / 255) as u8;
                        out[si + 2] = ((px[si + 2] as u32 * a + bg.0 as u32 * inv) / 255) as u8;
                        out[si + 3] = 255;
                    }
                    let old = SelectObject(mem, HGDIOBJ(hbmp.0));
                    let _ = BitBlt(dst, x, y, w, h, Some(mem), 0, 0, SRCCOPY);
                    SelectObject(mem, old);
                }
                let _ = DeleteObject(HGDIOBJ(hbmp.0));
                let _ = DeleteDC(mem);
            }
            Err(_) => {
                let _ = DeleteDC(mem);
            }
        }
        let _ = ReleaseDC(None, screen);
    }
}

// —— 交互 ——

fn on_mouse_down(hwnd: HWND, x: i32, y: i32) {
    let scale = display_scale();
    // 点别处 = 结束改名 / 字体输入（没在编辑时是空操作）
    commit_inline_edit();
    let Some(hit) = hit_at(x, y, scale) else {
        // 顶栏 / 左栏空白：拖窗口
        if y < (DTITLE * scale).round() as i32 || x < (DSIDEBAR * scale).round() as i32 {
            start_window_drag(hwnd);
        }
        return;
    };
    if hit == Hit::Close {
        close();
        return;
    }
    if !clickable(hit) {
        return;
    }
    match hit {
        Hit::Nav(t) => {
            {
                let mut s = state().lock().unwrap();
                s.tab = t;
                s.hover = None;
                s.scroll = 0;
                s.pending_font_combo = false;
            }
            hide_font_combo();
            repaint();
        }
        Hit::BoxRow(i) => select_box(i),
        Hit::BoxName(i) => {
            select_box(i);
            start_edit(i);
        }
        Hit::BoxIcon(i) => pick_icon(i),
        Hit::BoxUp(i) => move_box(i, -1),
        Hit::BoxDown(i) => move_box(i, 1),
        Hit::Slider => {
            state().lock().unwrap().dragging = true;
            unsafe {
                let _ = SetCapture(hwnd);
            }
            set_transparency_from_x(x, scale);
        }
        Hit::Swatch(target) => {
            let hex = {
                let s = state().lock().unwrap();
                match target {
                    ColorTarget::Box => s.draft.box_bg.clone(),
                    ColorTarget::Panel => s.draft.panel_bg.clone(),
                }
            };
            crate::picker::open(target, &hex, hwnd);
        }
        Hit::ResetColor => {
            {
                let mut s = state().lock().unwrap();
                s.draft.box_bg = config::default_box_bg();
                s.draft.panel_bg = config::default_panel_bg();
            }
            refresh_edit_brush();
            repaint();
        }
        Hit::FontName => {
            // 记下来，等 WM_LBUTTONUP 再展开下拉框。
            // 在 mouse-down 里直接展开，会被紧接着的 mouse-up 收掉，
            // 表现就是「字体选择框点不开」。
            state().lock().unwrap().pending_font_combo = true;
        }
        Hit::FontMinus | Hit::FontPlus => {
            let delta = if hit == Hit::FontMinus { -1.0 } else { 1.0 };
            {
                let mut s = state().lock().unwrap();
                s.draft.panel_font_size = (s.draft.panel_font_size + delta).clamp(
                    config::FONT_SIZE_MIN,
                    config::FONT_SIZE_MAX,
                );
            }
            repaint();
        }
        Hit::Switch(setting) => toggle_switch(setting),
        Hit::Mode(desktop_mode) => {
            state().lock().unwrap().draft.bar.desktop = desktop_mode;
            repaint();
        }
        Hit::OpenDir => {
            let dir = config::root_dir();
            let _ = desktop::shell_open(&dir.to_string_lossy());
        }
        Hit::CheckUpdate => crate::updater::check(false),
        Hit::Download => open_download_page(),
        Hit::CopyInfo(i) => {
            // 点一下就把这一行的值放进剪贴板，顺手在右侧亮 1.4 秒「已复制」
            if let Some((_, value)) = ABOUT_ROWS.get(i) {
                if crate::util::copy_text(value) {
                    state().lock().unwrap().copied = Some(i);
                    repaint();
                    unsafe {
                        let _ = SetTimer(Some(hwnd), TIMER_COPIED, COPIED_MS, None);
                    }
                }
            }
        }
        Hit::Link => {
            let _ = desktop::shell_open("https://gitee.com/gathon/lightbox");
        }
        Hit::Apply => {
            if dirty() {
                apply_changes();
            }
        }
        Hit::Cancel => close(),
        Hit::Ok => {
            apply_changes();
            close();
        }
        Hit::Close => {}
    }
}

fn on_mouse_up() {
    let (was_dragging, open_font) = {
        let mut s = state().lock().unwrap();
        let d = s.dragging || s.drag_win.is_some();
        let f = std::mem::take(&mut s.pending_font_combo);
        s.dragging = false;
        s.drag_win = None;
        (d, f)
    };
    unsafe {
        let _ = ReleaseCapture();
    }
    if was_dragging {
        repaint();
    }
    if open_font {
        show_font_combo();
    }
}

fn on_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let scale = display_scale();

    if state().lock().unwrap().dragging {
        set_transparency_from_x(x, scale);
        return;
    }

    if let Some(off) = state().lock().unwrap().drag_win {
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = SetWindowPos(
                hwnd,
                None,
                pt.x - off.x,
                pt.y - off.y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        return;
    }

    let hit = hit_at(x, y, scale);
    let changed = {
        let mut s = state().lock().unwrap();
        if s.hover != hit {
            s.hover = hit;
            true
        } else {
            false
        }
    };
    if changed {
        let hand = hit.map_or(false, clickable);
        unsafe {
            let cur = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).ok();
            let _ = SetCursor(cur);
        }
        repaint();
    }
}

fn on_wheel(delta: i16) {
    let scale = display_scale();
    let (tab, count, scroll, notes_scroll) = {
        let s = state().lock().unwrap();
        (s.tab, s.draft.boxes.len(), s.scroll, s.notes_scroll)
    };
    // 关于页：滚「更新信息」文本框。清单说明常有十条，一屏放不下。
    if tab == Tab::About {
        if !crate::updater::has_update() {
            return;
        }
        let lines = update_notes_lines().len().saturating_sub(1);
        let max_scroll = lines.saturating_sub(notes_visible(scale));
        let next = if delta > 0 {
            notes_scroll.saturating_sub(1)
        } else {
            (notes_scroll + 1).min(max_scroll)
        };
        if next != notes_scroll {
            state().lock().unwrap().notes_scroll = next;
            repaint();
        }
        return;
    }
    if tab != Tab::Boxes || count == 0 {
        return;
    }
    let visible = layout(scale, Tab::Boxes, count, 0, crate::updater::has_update())
        .rows
        .len()
        .max(1);
    let max_scroll = count.saturating_sub(visible);
    let next = if delta > 0 {
        scroll.saturating_sub(1)
    } else {
        (scroll + 1).min(max_scroll)
    };
    if next != scroll {
        state().lock().unwrap().scroll = next;
        repaint();
    }
}

fn start_window_drag(hwnd: HWND) {
    let mut wr = RECT::default();
    let mut pt = POINT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut wr);
        let _ = GetCursorPos(&mut pt);
        let _ = SetCapture(hwnd);
    }
    state().lock().unwrap().drag_win = Some(POINT {
        x: pt.x - wr.left,
        y: pt.y - wr.top,
    });
}

fn select_box(i: usize) {
    state().lock().unwrap().selected = i;
    repaint();
}

/// 上移 / 下移一个盒子（只改 draft）
fn move_box(i: usize, delta: i32) {
    let scale = display_scale();
    let (count, visible) = {
        let s = state().lock().unwrap();
        (
            s.draft.boxes.len(),
            layout(
                scale,
                Tab::Boxes,
                s.draft.boxes.len(),
                0,
                crate::updater::has_update(),
            )
            .rows
            .len()
            .max(1),
        )
    };
    let j = i as i32 + delta;
    if j < 0 || j as usize >= count {
        return;
    }
    let j = j as usize;
    {
        let mut s = state().lock().unwrap();
        s.draft.boxes.swap(i, j);
        s.selected = j;
        if j < s.scroll {
            s.scroll = j;
        } else if j >= s.scroll + visible {
            s.scroll = j - visible + 1;
        }
    }
    repaint();
}

/// 换盒子图标：系统文件对话框里选一个 .ico，存成 PNG data URL
fn pick_icon(i: usize) {
    let hwnd = match state().lock().unwrap().hwnd {
        Some(h) => h,
        None => return,
    };
    let Some(path) = crate::iconpick::pick_ico(hwnd, "选择收纳盒图标") else {
        return;
    };
    if !crate::iconpick::is_ico(&path) {
        warn(hwnd, "只支持 .ico 图标文件。");
        return;
    }
    let Some(url) = icons::ico_file_data_url(&path, 64) else {
        warn(hwnd, "这个图标文件读不出来，换一个试试。");
        return;
    };
    {
        let mut s = state().lock().unwrap();
        let Some(b) = s.draft.boxes.get_mut(i) else { return };
        b.icon = Some(url);
    }
    repaint();
}

fn open_download_page() {
    let Some(m) = crate::updater::latest_manifest() else { return };
    let url = if m.download.is_empty() {
        m.page
    } else {
        m.download
    };
    if !url.is_empty() {
        let _ = desktop::shell_open(&url);
    }
}

fn toggle_switch(s: Setting) {
    {
        let mut st = state().lock().unwrap();
        match s {
            Setting::Acrylic => st.draft.panel_acrylic = !st.draft.panel_acrylic,
            Setting::Radius => st.draft.panel_radius = !st.draft.panel_radius,
            Setting::HideLabel => st.draft.bar_hide_label = !st.draft.bar_hide_label,
            Setting::Topmost => st.draft.bar.topmost = !st.draft.bar.topmost,
            Setting::Autostart => st.autostart = !st.autostart,
            Setting::Diagnostics => st.draft.diagnostics = !st.draft.diagnostics,
        }
    }
    if s == Setting::Radius {
        refresh_round();
    }
    repaint();
}

/// 按滑块位置算透明度（draft）
fn set_transparency_from_x(x: i32, scale: f32) {
    let lay = layout(scale, Tab::Look, 0, 0, crate::updater::has_update());
    let Some(sr) = hit_rect(&lay, Hit::Slider) else { return };
    let tw = (sr.right - sr.left).max(1);
    let t = (((x - sr.left) as f32 / tw as f32).clamp(0.0, 1.0) * 50.0).round();
    {
        let mut s = state().lock().unwrap();
        if (s.draft.panel_transparency - t).abs() < 0.01 {
            return;
        }
        s.draft.panel_transparency = t;
    }
    repaint();
}

// —— 名字编辑 ——

fn start_edit(i: usize) {
    let scale = display_scale();
    let (id, name, hwnd, edit, count, scroll) = {
        let s = state().lock().unwrap();
        let Some(b) = s.draft.boxes.get(i) else { return };
        (
            b.id.clone(),
            b.name.clone(),
            s.hwnd,
            s.edit,
            s.draft.boxes.len(),
            s.scroll,
        )
    };
    let (Some(_hwnd), Some(edit)) = (hwnd, edit) else { return };
    let lay = layout(scale, Tab::Boxes, count, scroll, crate::updater::has_update());
    let Some(nr) = hit_rect(&lay, Hit::BoxName(i)) else { return };
    // 单行 EDIT 的文字是顶对齐的：把控件压到字体行高附近再垂直居中，
    // 否则名字会贴在输入框上沿。
    let fh = state().lock().unwrap().font.map(ui::font_height).unwrap_or(0);
    let inner_h = nr.bottom - nr.top;
    let edit_h = if fh > 0 {
        (fh + (4.0 * scale).round() as i32).min(inner_h)
    } else {
        inner_h
    };
    let edit_y = nr.top + (inner_h - edit_h) / 2;
    unsafe {
        let text = wide(&name);
        let _ = SendMessageW(
            edit,
            WM_SETTEXT,
            Some(WPARAM(0)),
            Some(LPARAM(text.as_ptr() as isize)),
        );
        let _ = SetWindowPos(
            edit,
            None,
            nr.left,
            edit_y,
            nr.right - nr.left,
            edit_h,
            SWP_NOZORDER,
        );
        let _ = ShowWindow(edit, SW_SHOW);
        let _ = SetFocus(Some(edit));
        let _ = SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
    {
        let mut s = state().lock().unwrap();
        s.editing = Some(id);
        s.selected = i;
    }
}

/// 提交名字（回车 / 失焦 / 点别处）
fn commit_name() {
    let (edit, id) = {
        let mut s = state().lock().unwrap();
        (s.edit, s.editing.take())
    };
    let (Some(edit), Some(id)) = (edit, id) else { return };
    let text = unsafe {
        let n = GetWindowTextLengthW(edit);
        let mut buf = vec![0u16; (n as usize) + 1];
        let _ = GetWindowTextW(edit, &mut buf);
        crate::util::from_wide(&buf)
    };
    let v = text.trim();
    let v = if v.is_empty() { "收纳盒" } else { v };
    {
        let mut s = state().lock().unwrap();
        if let Some(b) = s.draft.boxes.iter_mut().find(|b| b.id == id) {
            b.name = v.to_string();
        }
    }
    let hwnd = state().lock().unwrap().hwnd;
    unsafe {
        let _ = ShowWindow(edit, SW_HIDE);
        if let Some(h) = hwnd {
            let _ = SetFocus(Some(h));
        }
    }
    repaint();
}

/// 点别处 = 结束改名（字体改用 ComboBox 选择，不再共用这个 EDIT）
fn commit_inline_edit() {
    commit_name();
}

/// 撤销改名（Esc）
fn cancel_edit() {
    let (edit, id) = {
        let mut s = state().lock().unwrap();
        (s.edit, s.editing.take())
    };
    if id.is_none() {
        return;
    }
    let hwnd = state().lock().unwrap().hwnd;
    unsafe {
        if let Some(e) = edit {
            let _ = ShowWindow(e, SW_HIDE);
        }
        if let Some(h) = hwnd {
            let _ = SetFocus(Some(h));
        }
    }
    repaint();
}

// —— 应用 ——

/// 把 draft 写进配置：落盘、重建长条、刷新面板
fn apply_changes() {
    commit_inline_edit();
    hide_font_combo();
    state().lock().unwrap().pending_font_combo = false;
    let (mut next, orig, auto, auto_orig, switched_desktop) = {
        let s = state().lock().unwrap();
        (
            s.draft.clone(),
            s.orig.clone(),
            s.autostart,
            s.autostart_orig,
            s.draft.bar.desktop != s.orig.bar.desktop,
        )
    };
    // 设置窗开着时，长条上的「+」、面板里的删除/改名、拖动位置都是直接写配置的。
    // 草稿只覆盖它自己管得着的项，别把这段时间里外面的改动抹掉。
    {
        let live = app().cfg.lock().unwrap().clone();
        // 盒子集合以当前配置为准：新增的补到末尾，已删的不复活；
        // 两边都有的，顺序/名字/图标听草稿的
        let mut boxes: Vec<config::BoxConfig> = next
            .boxes
            .iter()
            .filter(|b| live.boxes.iter().any(|l| l.id == b.id))
            .cloned()
            .collect();
        for b in &live.boxes {
            if !next.boxes.iter().any(|d| d.id == b.id) {
                boxes.push(b.clone());
            }
        }
        next.boxes = boxes;
        // 草稿没动过的定位 / 滚动，保留外面拖出来的结果
        if next.bar.drag_dx == orig.bar.drag_dx {
            next.bar.drag_dx = live.bar.drag_dx;
        }
        if next.bar.x == orig.bar.x && next.bar.y == orig.bar.y {
            next.bar.x = live.bar.x;
            next.bar.y = live.bar.y;
        }
        if next.bar.scroll == orig.bar.scroll {
            next.bar.scroll = live.bar.scroll;
        }
    }
    // 刚切到桌面模式：清掉旧坐标，让长条落到默认位置（任务栏上边缘、屏幕中间）
    if switched_desktop && next.bar.desktop {
        next.bar.x = 0;
        next.bar.y = 0;
    }
    if auto != auto_orig {
        match autostart::set(auto) {
            Ok(()) => log::info(&format!("开机自启已{}", if auto { "打开" } else { "关闭" })),
            Err(e) => {
                log::info(&format!("开机自启设置失败：{e}"));
                let hwnd = state().lock().unwrap().hwnd;
                if let Some(h) = hwnd {
                    warn(h, &format!("开机自启设置失败：{e}"));
                }
            }
        }
    }
    {
        let mut g = app().cfg.lock().unwrap();
        *g = next.clone();
        config::save(&g);
    }
    {
        let mut s = state().lock().unwrap();
        s.draft = next.clone();
        s.orig = next.clone();
        s.autostart_orig = auto;
    }
    // 弹窗字体换了要先灌给渲染器并清文字缓存，再让长条 / 面板重画，
    // 否则它们会拿旧字体的位图继续用。
    crate::render::set_font(&next.panel_font, next.panel_font_size);
    // 「打印诊断信息」立即生效：打开就开始写日志，关掉就彻底停（不再落盘）。
    log::set_enabled(next.diagnostics);
    crate::bar::refresh();
    crate::panel::apply_theme();
    crate::panel::reload();
    refresh_edit_brush();
    refresh_round();
    repaint();
}

fn warn(hwnd: HWND, msg: &str) {
    unsafe {
        MessageBoxW(
            Some(hwnd),
            &windows::core::HSTRING::from(msg),
            &windows::core::HSTRING::from("LightBox"),
            MB_OK | MB_ICONWARNING,
        );
    }
}
