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
//! 普通窗口：圆角靠 `SetWindowRgn` 裁区域（Win11 的 DWM 圆角只圆边框、
//! 客户区仍是方的，四角会露白直角），阴影 DWM 自带。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, GetDC, IntersectClipRect, ReleaseDC,
    SelectClipRgn, SelectObject, SetBkColor, SetTextColor, SetViewportOrgEx, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS,
    HBRUSH, HDC, HGDIOBJ, HFONT, InvalidateRect, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, KillTimer, LoadCursorW,
    RegisterClassW, SendMessageW, SetCursor, SetTimer,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, GWLP_WNDPROC, IDC_ARROW, IDC_HAND,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE,
    SW_SHOW,
    WINDOW_EX_STYLE, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND,
    WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETFONT, WM_SETTEXT,
    WM_TIMER, WNDCLASSW, WNDPROC, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_TABSTOP,
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

/// 关于页信息行：标签 + 值。**点一下可以复制**（开源链接另有一个「打开」）
const ABOUT_ROWS: [(&str, &str); 1] = [("开源链接", "https://gitee.com/gathon/lightbox")];

/// 关于页标题下面的一句简介（不用加粗，整句常规字重）
const ABOUT_TAGLINE: &str = "一款开源的轻量级 Windows 桌面图标整理工具";

/// 取色器弹出时设置页要临时加深 —— 两个窗口底色一样就分不出层次
static DIMMED: AtomicBool = AtomicBool::new(false);

/// 设置页现在是不是被加深了
fn dimmed() -> bool {
    DIMMED.load(Ordering::Relaxed)
}

// —— 尺寸（设计单位 = 96 DPI 下的逻辑像素）——
const DW: f32 = 680.0;
/// 顶部留白：相当于一条标题栏的高度。
///
/// 页头标题原来离窗口顶只有 8，紧贴着上沿显得憋屈；这里整页内容（品牌、
/// 导航、页头、内容区）统一下移这么多，窗口高度同步加高同样的值 ——
/// 内容区高度不变，所以「5 个盒子不滚动」这类阈值也不受影响。
const DTOP: f32 = 30.0;
const DH: f32 = 560.0 + DTOP;
const DTITLE: f32 = 52.0;
const DSIDEBAR: f32 = 160.0;
const DPAD: f32 = 22.0;
const DFOOT: f32 = 60.0;
const DNAV: f32 = 42.0;
/// 一行的高度。原来 46，加「解散 / 删除」按钮后压到 42：
/// 行内 38 高，刚好还能塞下 32 的图标和 24 的箭头。
const DROW_H: f32 = 42.0;
const DBTN_W: f32 = 84.0;
const DBTN_H: f32 = 34.0;
/// 内容区起始 y。页头标题 8~36、副标题 36~54，这里只留 20 的间隙 ——
/// 原来是 110/128 起步，中间空出一大块，看着很空。
const DCONTENT_TOP: f32 = 74.0 + DTOP;
/// 内容区下边界：贴着页脚分隔线（页脚线在 DH - DFOOT），留 6 的余量
const DCONTENT_GAP: f32 = 6.0;
/// 列表下方「+」按钮：距最后一行 8、168×34、主色底白加号
const DADD_GAP: f32 = 8.0;
const DADD_W: f32 = 168.0;
const DADD_H: f32 = 34.0;
/// 主界面模式标题 / 模式按钮 / 开关行相对「最后一行底部」的偏移。
/// 顺序是「+ 按钮 → 主界面模式 → 长条上隐藏名称」：先选模式，再配这个模式的显示项。
const DMODE_LABEL_Y: f32 = 66.0;
const DMODE_BTN_Y: f32 = 92.0;
const DSWITCH_Y: f32 = 142.0;
/// 行下方那一整块（+ 按钮 / 主界面模式 / 隐藏名称）的总高，
/// 用来算「整页要不要滚动」：5 个盒子刚好不滚动，6 个起出现滚动条。
const DBLOCK_H: f32 = 184.0;
/// 页面滚动条宽度（贴在窗口右边缘的细条）
const DSCROLL_W: f32 = 5.0;
/// 行内「解散 / 删除」按钮宽度。88 是去掉描边后按 11px 字重新量的：
/// 5 个汉字约 55，两侧各留 16，视觉上不再顶到边。
const DROWBTN_W: f32 = 88.0;
/// 关于页「更新信息」文本框（只有查到新版本时才画），在检查更新按钮下方
const DNOTES_TOP: f32 = 286.0;
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
            Tab::Look => "背景色、毛玻璃与圆角",
            Tab::Features => "开机自启、置顶、诊断与存储位置",
            Tab::About => "版本信息与开源项目",
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
    /// 行内「解散收纳盒」：图标放回桌面，盒子留着
    BoxDissolve(usize),
    /// 行内「删除收纳盒」：只有空盒子能删
    BoxDelete(usize),
    BoxUp(usize),
    BoxDown(usize),
    /// 列表下方「+」：新建收纳盒
    NewBox,
    /// 列表右侧滚动条（拖动手柄 / 点空白处翻页）
    Scrollbar,
    Swatch(ColorTarget),
    ResetColor,
    Switch(Setting),
    Mode(bool),
    OpenDir,
    CheckUpdate,
    Download,
    /// 关于页信息行（开源链接）：点一下复制
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
    /// 整页滚动条：（轨道, 手柄）。内容装得下时为 None
    scrollbar: Option<(RECT, RECT)>,
}

fn hit_rect(lay: &Layout, hit: Hit) -> Option<RECT> {
    lay.hits.iter().find(|(h, _)| *h == hit).map(|(_, r)| *r)
}

/// 内容区的上下边界（绘制裁剪、滚动条、滚轮、拖拽手柄共用同一份算术）
fn content_band(scale: f32) -> (i32, i32) {
    let r = |v: f32| (v * scale).round() as i32;
    (
        r(DCONTENT_TOP),
        r(DH) - r(DFOOT) - r(DCONTENT_GAP),
    )
}

/// 外观 / 功能两页的坐标是写死的绝对值（110 / 128 起），
/// 统一上移到 DCONTENT_TOP：绘制时用 `SetViewportOrgEx` 整体平移，
/// 命中测试时把鼠标 y 反向补回来，两边就还是同一套数。
fn content_shift(tab: Tab, scale: f32) -> i32 {
    let r = |v: f32| (v * scale).round() as i32;
    match tab {
        Tab::Look => r(DCONTENT_TOP) - r(110.0),
        // 功能页第一项是开关行，标题画在命中区上方 14 处，
        // 所以要让「命中区顶 128」落到 DCONTENT_TOP + 14，标题才贴着内容区顶。
        Tab::Features => r(DCONTENT_TOP) + r(14.0) - r(128.0),
        // 关于页整页用的是绝对坐标（72 / 112 / 166…），窗口顶部多留出来的
        // 那条空白只能靠整体平移补上，否则内容会顶到标题上面去。
        Tab::About => r(DTOP),
        _ => 0,
    }
}

fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 绘制上下文：缩放 + 各列边界
#[derive(Clone, Copy)]
struct Ctx {
    scale: f32,
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
    /// 收纳盒页整页滚动位移（设备像素，向下为正）
    page: i32,
    /// 正在拖整页滚动条：抓住手柄时的鼠标相对手柄顶部的偏移（像素）
    scroll_drag: Option<i32>,
    /// 关于页「更新信息」文本框滚到第几行
    notes_scroll: usize,
    hover: Option<Hit>,
    /// 正在拖整个窗口时的「鼠标相对窗口左上角」偏移
    drag_win: Option<POINT>,
    /// 名字输入框（只有一个，反复挪到正在改的那一行）
    edit: Option<HWND>,
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
            page: 0,
            scroll_drag: None,
            notes_scroll: 0,
            hover: None,
            drag_win: None,
            edit: None,
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
    // 从托盘 / 面板点某个盒子的「设置…」进来时，把它滚到能看见的位置
    let row_h = (DROW_H * scale).round() as i32;
    let (ctop, cbot) = content_band(scale);
    let view = cbot - ctop;
    let content_h = cfg.boxes.len() as i32 * row_h + (DBLOCK_H * scale).round() as i32;
    let max_page = (content_h - view).max(0);
    let page = (((selected as i32) + 1) * row_h - view)
        .max(0)
        .min(max_page);

    {
        let mut s = state().lock().unwrap();
        s.tab = want_tab;
        s.notes_scroll = 0;
        s.draft = cfg.clone();
        s.orig = cfg;
        s.autostart = auto;
        s.autostart_orig = auto;
        s.selected = selected;
        s.page = page;
        s.scroll_drag = None;
        s.hover = None;
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
    refresh_round();
    position(hwnd);
    // 摆好位置之后再裁一次圆角：窗口落到鼠标那颗屏上，DPI 才定下来
    refresh_round();
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetFocus(Some(hwnd));
    }
}

/// 关掉设置窗口（未应用的改动直接丢弃）
pub fn close() {
    // 调色板开着的话先收掉（它会把「加深」状态一起恢复）
    crate::picker::close();
    let (hwnd, brush, font) = {
        let mut s = state().lock().unwrap();
        let h = s.hwnd.take();
        s.edit = None;
        s.editing = None;
        s.hover = None;
        s.drag_win = None;
        let b = s.brush.take();
        let f = s.font.take();
        (h, b, f)
    };
    unsafe {
        if let Some(h) = hwnd {
            let _ = DestroyWindow(h);
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
/// `page` = 整页滚动位移（设备像素，向下为正）。
fn layout(scale: f32, tab: Tab, box_count: usize, page: i32, has_update: bool) -> Layout {
    let r = |v: f32| (v * scale).round() as i32;
    let w = r(DW);
    let h = r(DH);
    let sx = r(DSIDEBAR);
    let pad = r(DPAD);
    let cx = sx + pad;
    let cr = w - pad;
    let mut hits: Vec<(Hit, RECT)> = Vec::new();
    let mut rows: Vec<RECT> = Vec::new();
    let mut scrollbar: Option<(RECT, RECT)> = None;

    // 关闭按钮
    hits.push((
        Hit::Close,
        RECT {
            left: w - r(46.0),
            top: r(6.0) + r(DTOP),
            right: w - r(6.0),
            bottom: r(42.0) + r(DTOP),
        },
    ));

    // 左侧导航
    let mut ny = r(DTITLE) + r(10.0) + r(DTOP);
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
            let (top, bottom) = content_band(scale);
            let row_h = r(DROW_H);
            // 行内按钮的左右边界：从右往左 = 箭头(52) → 删除(78) → 解散(78) → 名字
            let del_r = cr + r(8.0) - r(66.0);
            let del_l = del_r - r(DROWBTN_W);
            let dis_r = del_l - r(6.0);
            let dis_l = dis_r - r(DROWBTN_W);
            // 所有盒子都排出来，靠整页滚动看下面的 —— 这里不再截断行数
            for i in 0..box_count {
                let y = top + (i as i32) * row_h - page;
                let row = RECT {
                    left: cx - r(8.0),
                    top: y,
                    right: cr + r(8.0),
                    bottom: y + row_h - r(4.0),
                };
                // 先放整行，再放行内的小块：命中测试从后往前找，
                // 这样图标 / 名字 / 按钮 / 箭头会盖过「整行」的点击区
                hits.push((Hit::BoxRow(i), row));
                rows.push(row);
                hits.push((
                    Hit::BoxIcon(i),
                    RECT {
                        left: row.left + r(7.0),
                        top: y + r(3.0),
                        right: row.left + r(39.0),
                        bottom: y + r(35.0),
                    },
                ));
                hits.push((
                    Hit::BoxName(i),
                    RECT {
                        left: row.left + r(47.0),
                        top: y + r(3.0),
                        right: dis_l - r(8.0),
                        bottom: y + r(35.0),
                    },
                ));
                hits.push((
                    Hit::BoxDissolve(i),
                    RECT {
                        left: dis_l,
                        top: y + r(6.0),
                        right: dis_r,
                        bottom: y + r(32.0),
                    },
                ));
                hits.push((
                    Hit::BoxDelete(i),
                    RECT {
                        left: del_l,
                        top: y + r(6.0),
                        right: del_r,
                        bottom: y + r(32.0),
                    },
                ));
                hits.push((
                    Hit::BoxUp(i),
                    RECT {
                        left: row.right - r(58.0),
                        top: y + r(7.0),
                        right: row.right - r(34.0),
                        bottom: y + r(31.0),
                    },
                ));
                hits.push((
                    Hit::BoxDown(i),
                    RECT {
                        left: row.right - r(30.0),
                        top: y + r(7.0),
                        right: row.right - r(6.0),
                        bottom: y + r(31.0),
                    },
                ));
            }

            // 最后一行下面：加号 → 主界面模式 → 主界面隐藏盒子名称
            let y0 = top + (box_count as i32) * row_h - page;

            let aw = r(DADD_W);
            let ax = cx + (cr - cx - aw) / 2;
            let ay = y0 + r(DADD_GAP);
            hits.push((
                Hit::NewBox,
                RECT {
                    left: ax,
                    top: ay,
                    right: ax + aw,
                    bottom: ay + r(DADD_H),
                },
            ));

            // 主界面隐藏盒子名称（排在模式按钮后面）
            let ly = y0 + r(DSWITCH_Y);
            hits.push((
                Hit::Switch(Setting::HideLabel),
                RECT {
                    left: cr - r(42.0),
                    top: ly + r(6.0),
                    right: cr,
                    bottom: ly + r(28.0),
                },
            ));

            // 主界面模式：任务栏模式 / 桌面模式
            let my = y0 + r(DMODE_BTN_Y);
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

            // 整页滚动条：内容（所有盒子 + 下面那块设置）装不下才出现，
            // 贴在窗口右边缘，不随内容滚动
            let content_h = (box_count as i32) * row_h + r(DBLOCK_H);
            let view_h = bottom - top;
            if content_h > view_h {
                let track = RECT {
                    left: w - r(10.0),
                    top,
                    right: w - r(10.0) + r(DSCROLL_W),
                    bottom,
                };
                let th = ((view_h as f32) * (view_h as f32) / (content_h as f32)).round() as i32;
                let th = th.max(r(32.0)).min(view_h);
                let max_page = content_h - view_h;
                let span = view_h - th;
                let used = page.clamp(0, max_page);
                let ty = top + (span as f32 * used as f32 / max_page as f32).round() as i32;
                let thumb = RECT {
                    left: track.left,
                    top: ty,
                    right: track.right,
                    bottom: ty + th,
                };
                hits.push((Hit::Scrollbar, track));
                scrollbar = Some((track, thumb));
            }
        }
        Tab::Look => {
            hits.push((
                Hit::ResetColor,
                RECT {
                    left: cr - r(84.0),
                    top: r(110.0),
                    right: cr,
                    bottom: r(132.0),
                },
            ));
            // 「收纳盒」「弹出窗口」两块色卡并排放，一行看完
            let half = (cr - cx - r(8.0)) / 2;
            hits.push((
                Hit::Swatch(ColorTarget::Box),
                RECT {
                    left: cx,
                    top: r(136.0),
                    right: cx + half,
                    bottom: r(182.0),
                },
            ));
            hits.push((
                Hit::Swatch(ColorTarget::Panel),
                RECT {
                    left: cx + half + r(8.0),
                    top: r(136.0),
                    right: cr,
                    bottom: r(182.0),
                },
            ));
            for (s, top) in [(Setting::Acrylic, 212.0), (Setting::Radius, 258.0)] {
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
        }
        Tab::Features => {
            for (s, top) in [
                (Setting::Autostart, 128.0),
                (Setting::Topmost, 176.0),
                (Setting::Diagnostics, 224.0),
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
                    top: r(284.0),
                    right: cr,
                    bottom: r(314.0),
                },
            ));
        }
        Tab::About => {
            // 信息行都可以点一下复制：整行都是命中区
            for i in 0..ABOUT_ROWS.len() {
                let y = r(184.0) + i as i32 * r(34.0);
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
            // 「打开」压在这一行右侧：命中测试从后往前找，这一小块盖过整行
            hits.push((
                Hit::Link,
                RECT {
                    left: cr - r(62.0),
                    top: r(181.0),
                    right: cr,
                    bottom: r(207.0),
                },
            ));
            hits.push((
                Hit::CheckUpdate,
                RECT {
                    left: cx,
                    top: r(240.0),
                    right: cx + r(112.0),
                    bottom: r(274.0),
                },
            ));
            if has_update {
                hits.push((
                    Hit::Download,
                    RECT {
                        left: cx + r(120.0),
                        top: r(240.0),
                        right: cx + r(232.0),
                        bottom: r(274.0),
                    },
                ));
            }
        }
    }

    Layout {
        hits,
        rows,
        scrollbar,
    }
}

/// 不随内容平移、也不受内容区上下边界限制的命中区：
/// 侧栏、关闭按钮、页脚「应用 / 取消 / 确定」都是窗口固定件，画在原始坐标上。
fn pinned_hit(h: &Hit) -> bool {
    matches!(
        h,
        Hit::Close | Hit::Nav(_) | Hit::Apply | Hit::Cancel | Hit::Ok
    )
}

/// 鼠标在某点落在哪个可点区域（从后往前找 = 小块优先于整行）
fn hit_at(x: i32, y: i32, scale: f32) -> Option<Hit> {
    let (tab, count, page) = {
        let s = state().lock().unwrap();
        (s.tab, s.draft.boxes.len(), s.page)
    };
    let lay = layout(scale, tab, count, page, crate::updater::has_update());
    // 外观 / 功能页的内容整体上移过（GDI 视口原点），鼠标坐标要补回来。
    // 注意：侧栏、关闭按钮、页脚那三个按钮**没有**跟着平移，用的是原始坐标。
    let dy = content_shift(tab, scale);
    // 收纳盒页是整页滚动的：滚出去的行 / 按钮不许在页头页脚上接点击
    let (ctop, cbot) = content_band(scale);
    lay.hits
        .iter()
        .rev()
        .find(|(h, r)| {
            if pinned_hit(h) {
                contains(r, x, y)
            } else {
                let cy = y - dy;
                contains(r, x, cy) && cy >= ctop && cy < cbot
            }
        })
        .map(|(h, _)| *h)
}

/// 这个命中区能不能点（边界上的 ↑ / ↓、空盒子的「解散」、没改动时的「应用」是灰的）
fn clickable(h: Hit) -> bool {
    let (is_dirty, count, ids) = {
        let s = state().lock().unwrap();
        (
            s.draft != s.orig || s.autostart != s.autostart_orig,
            s.draft.boxes.len(),
            s.draft
                .boxes
                .iter()
                .map(|b| b.id.clone())
                .collect::<Vec<_>>(),
        )
    };
    match h {
        Hit::Apply => is_dirty,
        Hit::BoxUp(i) => i > 0,
        Hit::BoxDown(i) => i + 1 < count,
        // 空盒子没有图标可放回桌面，按钮置灰（点下去也不会有任何反应）
        Hit::BoxDissolve(i) => ids.get(i).is_some_and(|id| config::count_items(id) > 0),
        _ => true,
    }
}

/// 设置窗口自己所在显示器的缩放系数（96 DPI = 1.0）。
///
/// 必须按**窗口自己的 DPI** 算，不能按长条所在显示器算：设置页可以拖到
/// 另一颗屏上（100% ↔ 125%），那时窗口的 DPI 和长条的 DPI 已经不是一回事，
/// 用错了就会出现「内容按新比例画进按旧比例开的窗口里」——右/下被裁掉，
/// 或者右边和底部留一大片空白。
///
/// 窗口还没建出来时按鼠标位置算（窗口就开在鼠标那儿）。
fn display_scale() -> f32 {
    if let Some(h) = state().lock().unwrap().hwnd {
        let dpi = unsafe { GetDpiForWindow(h) };
        if dpi > 0 {
            return dpi as f32 / 96.0;
        }
    }
    cursor_scale()
}

/// 鼠标所在显示器的缩放系数（开窗定位、建窗前算尺寸用）
fn cursor_scale() -> f32 {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let mon = geom::work_area_at(pt.x, pt.y);
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
            // HIWORD(wp) = 通知码（EN_KILLFOCUS）
            // LOWORD(lp) = 发消息的控件句柄
            let notify_code = ((wp.0 >> 16) & 0xffff) as u32;
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
        WM_DPICHANGED => {
            // 窗口被拖到另一颗不同缩放的显示器上。系统只给一个「建议矩形」，
            // 不会替我们重排：必须按新比例把窗口尺寸重新算一遍，否则内容
            // 按新 scale 画进按旧 scale 开的窗口里 —— 100%→125% 时右侧和底部
            // 被裁（关闭按钮、应用/取消/确定看不见），反过来则留一大片空白。
            let suggested = unsafe { *(lp.0 as *const RECT) };
            let scale = (wp.0 & 0xffff) as u32 as f32 / 96.0;
            let w = (DW * scale).round() as i32;
            let h = (DH * scale).round() as i32;
            // 改名输入框是按旧比例摆的，先提交再重排，免得留一个字号和位置
            // 都对不上的浮窗；输入框的字体也一并按新比例重造
            commit_inline_edit();
            rescale_edit(scale);
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    w,
                    h,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            // 圆角区域按新尺寸重裁（别用 refresh_round：WM_DPICHANGED 里
            // GetDpiForWindow 未必已经是新值，这里直接用手上的 scale）
            round_corners(hwnd, w, h);
            repaint();
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

/// 窗口圆角半径（设计单位 8，按当前缩放取整）。
///
/// 裁区（`SetWindowRgn`）和自绘描边必须用**同一个**半径：两边差一像素，
/// 描边就会跑出裁区被切掉半条，或者和裁区错开一像素 —— 看着就是「圆角
/// 边缘毛毛的」。早先一个写死 8、一个写死 10，正是那个毛病。
fn corner_radius(scale: f32) -> i32 {
    (8.0 * scale).round().max(4.0) as i32
}

/// 圆角：跟 draft 里的「圆角窗口」开关走，**两种系统都靠 `SetWindowRgn` 硬裁**。
///
/// Win11 的 DWM 圆角（`DWMWA_WINDOW_CORNER_PREFERENCE = ROUND`）只圆了它自己
/// 那 1px 边框，客户区还是方的 —— 我们这个窗口整块是不透明自绘，四角就会露出
/// 「白直角 + 里面一条圆角细线」（Win11 虚拟机里一眼能看到）。所以情愿用区域
/// 硬裁（1 像素台阶，和 Win10 那套一样），形状至少是真的圆角。
/// 相应的 DWM 圆角要显式关掉，否则两层圆角叠在一起会多出一条错位的边。
fn round_corners(hwnd: HWND, w: i32, h: i32) {
    let radius = state().lock().unwrap().draft.panel_radius;
    let r = corner_radius(display_scale());
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &DWMWCP_DONOTROUND as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
        // 先把上一次的区域清掉，再按开关决定要不要圆角
        let _ = windows::Win32::Graphics::Gdi::SetWindowRgn(hwnd, None, false);
        if radius {
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

/// 自己的圆角跟着 draft 走（预览）
fn refresh_round() {
    let hwnd = state().lock().unwrap().hwnd;
    let Some(hwnd) = hwnd else { return };
    let scale = display_scale();
    let w = (DW * scale).round() as i32;
    let h = (DH * scale).round() as i32;
    round_corners(hwnd, w, h);
}

/// 换显示器后把输入框的字号换成新比例。
///
/// EDIT 的字体是 `create_edit` 里按当时的 scale 造一次的，窗口拖到另一颗
/// 屏上（100% ↔ 125%）后如果不重造，改名框里的字会明显比页面上的小/大。
fn rescale_edit(scale: f32) {
    let (edit, old) = {
        let s = state().lock().unwrap();
        (s.edit, s.font)
    };
    let Some(edit) = edit else { return };
    let font = ui::create_font((14.0 * scale).round() as i32, false);
    unsafe {
        // 先把新字体换上去，再删旧的（选进控件的字体不能直接删）
        let _ = SendMessageW(
            edit,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        if let Some(f) = old {
            let _ = DeleteObject(HGDIOBJ(f.0));
        }
    }
    state().lock().unwrap().font = Some(font);
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

/// 把窗口摆在鼠标附近（居中，再夹进工作区）。
/// 尺寸按**鼠标所在那颗屏**的缩放算：窗口就开在那儿。
fn position(hwnd: HWND) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let mon = geom::work_area_at(pt.x, pt.y);
    let scale = mon.scale as f32 / 96.0;
    let r = |v: f32| (v * scale).round() as i32;
    let w = r(DW);
    let h = r(DH);
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
        page,
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
            s.page,
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
    let lay = layout(scale, tab, draft.boxes.len(), page, has_update);

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
        // 开关这类曲线图形走像素直写的抗锯齿通道（没有离屏位图就自动退回 GDI）
        if let Some(b) = &buf {
            g.attach(b.surface());
        }

        let r = |v: f32| (v * scale).round() as i32;
        let c = Ctx {
            scale,
            sx: r(DSIDEBAR),
            cx: r(DSIDEBAR) + r(DPAD),
            cr: cw - r(DPAD),
        };
        let bg = bg_color();
        let col = Colors {
            bg,
            fg: theme::text_on(bg),
            // 次要文字：浅灰底上的小字本来就显得软，往深里收一点才立得住。
            // 0.78 ≈ 旧版 `rgba(0,0,0,.7)` 落在 #f9fafd 上的观感（≈ #4b4b4b），
            // 早先的 0.70 比它浅一档，说明文字看着发灰。
            muted: theme::mix(bg, theme::text_on(bg), 0.78),
            accent: theme::accent_on(bg),
            line: line_color(bg),
            sunken: sunken_color(bg),
        };

        // —— 底色 + 左侧导航 ——
        g.fill_rect(0, 0, cw, ch, bg);
        let sidebar = theme::mix(bg, col.fg, 0.045);
        g.fill_rect(0, 0, c.sx, ch, sidebar);
        let brand = "LightBox";
        g.font(r(14.0), true);
        let brand_w = g.text_w(brand);
        g.text(
            r(20.0),
            r(10.0) + r(DTOP),
            c.sx - r(40.0),
            r(30.0),
            brand,
            col.fg,
            Align::Left,
        );
        // 版本号：贴着品牌名右侧一小段浅灰小字（排障时一眼看出装的是哪版）
        g.font(r(11.0), false);
        let vx = r(20.0) + brand_w + r(7.0);
        g.text(
            vx,
            r(10.0) + r(DTOP),
            c.sx - vx - r(8.0),
            r(30.0),
            &format!("v{}", env!("CARGO_PKG_VERSION")),
            col.fg,
            Align::Left,
        );
        for t in TABS {
            let Some(nr) = hit_rect(&lay, Hit::Nav(t)) else { continue };
            let active = t == tab;
            let hot = hover == Some(Hit::Nav(t));
            if active {
                g.fill_round_aa(
                    nr.left,
                    nr.top,
                    nr.right - nr.left,
                    nr.bottom - nr.top,
                    r(7.0),
                    theme::mix(bg, col.accent, 0.16),
                );
            } else if hot {
                g.fill_round_aa(
                    nr.left,
                    nr.top,
                    nr.right - nr.left,
                    nr.bottom - nr.top,
                    r(7.0),
                    theme::mix(bg, col.fg, 0.08),
                );
            }
            g.font(r(12.0), active);
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
        g.text(
            c.cx,
            r(8.0) + r(DTOP),
            title_w,
            r(28.0),
            tab.title(),
            col.fg,
            Align::Left,
        );
        g.font(r(11.0), false);
        g.text(
            c.cx,
            r(36.0) + r(DTOP),
            title_w,
            r(18.0),
            tab.subtitle(),
            col.muted,
            Align::Left,
        );

        // —— 关闭按钮 ——
        if let Some(cr) = hit_rect(&lay, Hit::Close) {
            if hover == Some(Hit::Close) {
                g.fill_round_aa(
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
        // 内容区裁剪：收纳盒页是整页滚动的，滚出去的半行不能画到页头 / 页脚上。
        // 裁剪矩形用设备坐标（此时视口原点还是 0,0）。
        let (ctop, cbot) = content_band(scale);
        let _ = IntersectClipRect(dc, 0, ctop, cw, cbot);
        g.clip_y(Some((ctop, cbot)));
        // 外观 / 功能页整体上移，代码里的绝对坐标不用逐条改
        let dy = content_shift(tab, scale);
        if dy != 0 {
            let _ = SetViewportOrgEx(dc, 0, dy, None);
            g.viewport_y(dy);
        }
        match tab {
            Tab::Boxes => paint_boxes(
                &mut g, &c, &lay, &col, &draft, hover, selected, page, &editing,
            ),
            Tab::Look => paint_look(&mut g, &c, &lay, &col, &draft, hover),
            Tab::Features => paint_features(&mut g, &c, &lay, &col, &draft, hover, auto_on),
            Tab::About => paint_about(&mut g, &c, &lay, &col, hover, has_update, notes_scroll),
        }
        if dy != 0 {
            let _ = SetViewportOrgEx(dc, 0, 0, None);
            g.viewport_y(0);
        }
        let _ = SelectClipRgn(dc, None);
        g.clip_y(None);

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
        // 沿圆角**外缘**描一圈（抗锯齿版本）：这一圈同时把 `SetWindowRgn`
        // 硬裁出来的 1 像素台阶盖掉了大半 —— 最外那圈像素本来就是「底色 ×
        // 描边色」的过渡，看着是边缘，不是锯齿。
        let edge_rad = if draft.panel_radius {
            corner_radius(scale)
        } else {
            0
        };
        g.stroke_round_aa(0, 0, cw, ch, edge_rad, theme::mix(bg, col.fg, 0.32), 1);

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
    _page: i32,
    editing: &Option<String>,
) {
    let r = |v: f32| c.r(v);
    let (top, _bottom) = content_band(c.scale);

    if draft.boxes.is_empty() {
        g.font(r(12.0), false);
        g.text(
            c.cx,
            top + r(36.0),
            c.cr - c.cx,
            r(24.0),
            "还没有收纳盒。点下面的「+」新建一个。",
            col.muted,
            Align::Left,
        );
    }

    for (k, row) in lay.rows.iter().enumerate() {
        let i = k;
        let Some(b) = draft.boxes.get(i) else { continue };
        let hot = matches!(
            hover,
            Some(Hit::BoxRow(j)) | Some(Hit::BoxName(j)) | Some(Hit::BoxIcon(j))
                | Some(Hit::BoxDissolve(j)) | Some(Hit::BoxDelete(j))
                | Some(Hit::BoxUp(j)) | Some(Hit::BoxDown(j)) if j == i
        );
        let row_bg = if i == selected {
            theme::mix(col.bg, col.accent, 0.14)
        } else if hot {
            theme::mix(col.bg, col.fg, 0.08)
        } else {
            col.bg
        };
        if i == selected || hot {
            g.fill_round_aa(
                row.left,
                row.top,
                row.right - row.left,
                row.bottom - row.top,
                r(8.0),
                row_bg,
            );
        }
        if i == selected {
            g.fill_round_aa(
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
                    g.fill_round_aa(
                        nr.left,
                        nr.top,
                        nr.right - nr.left,
                        nr.bottom - nr.top,
                        r(6.0),
                        theme::mix(row_bg, col.fg, 0.06),
                    );
                }
                g.font(r(12.0), false);
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

        // 解散 / 删除：放在名字右边、上下箭头左边。
        // 「解散」把盒子里的图标放回桌面（盒子留着），空盒子时灰掉；
        // 「删除」只删空盒子，里面有图标就弹提示让先解散。
        for (hit, label, enabled) in [
            (
                Hit::BoxDissolve(i),
                "解散收纳盒",
                config::count_items(&b.id) > 0,
            ),
            (Hit::BoxDelete(i), "删除收纳盒", true),
        ] {
            let Some(br) = hit_rect(lay, hit) else { continue };
            draw_row_button(g, c, col, br, label, hover == Some(hit), enabled);
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
                g.fill_round_aa(
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

    // 「+」新建收纳盒（列表正下方，水平居中）
    if let Some(ar) = hit_rect(lay, Hit::NewBox) {
        draw_add_button(g, c, col, ar, hover == Some(Hit::NewBox));
    }

    // 右侧滚动条（盒子多到装不下时才有）
    if let Some((track, thumb)) = lay.scrollbar {
        let hot = hover == Some(Hit::Scrollbar);
        let tw = track.right - track.left;
        g.fill_round_aa(
            track.left,
            track.top,
            tw,
            track.bottom - track.top,
            tw / 2,
            theme::mix(col.bg, col.fg, 0.06),
        );
        g.fill_round_aa(
            thumb.left,
            thumb.top,
            tw,
            thumb.bottom - thumb.top,
            tw / 2,
            if hot {
                theme::mix(col.fg, col.bg, 0.25)
            } else {
                theme::mix(col.fg, col.bg, 0.42)
            },
        );
    }

    // 主界面模式（和上面的「+」之间留出一段，免得两块黏在一起）
    let my = hit_rect(lay, Hit::Mode(false)).map(|m| m.top).unwrap_or(0);
    g.font(r(12.0), false);
    g.text(
        c.cx,
        my - r(DMODE_BTN_Y - DMODE_LABEL_Y),
        c.cr - c.cx,
        r(20.0),
        "主界面模式",
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
        g.fill_round_aa(
            mr.left,
            mr.top,
            mr.right - mr.left,
            mr.bottom - mr.top,
            r(7.0),
            fill,
        );
        // 和标题「主界面模式」同为 12px：之前是 11，100% 下和标题差 1px 看着还行，
        // 到 125% 一取整（15 vs 14）就明显比标题小一圈，像是「字缩了」。
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

    // 长条上是否显示盒子名称：只影响任务栏 / 桌面长条，弹窗面板里名字一直显示，
    // 所以叫「主界面隐藏盒子名称」而不是笼统的「隐藏盒子名称」。
    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::HideLabel),
        "主界面隐藏盒子名称",
        draft.bar_hide_label,
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

    // 背景颜色（收纳盒 / 弹出窗口各一个色块）
    // 透明度那一项已经去掉：面板固定完全不透明，这里直接从内容区顶部开始。
    g.font(r(12.0), false);
    g.text(
        c.cx,
        r(110.0),
        r(200.0),
        r(22.0),
        "背景颜色",
        col.fg,
        Align::Left,
    );
    if let Some(rr) = hit_rect(lay, Hit::ResetColor) {
        let hot = hover == Some(Hit::ResetColor);
        g.font(r(10.0), false);
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
        g.fill_round_aa(
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
        g.font(r(11.0), false);
        g.text(
            rect.left + r(46.0),
            rect.top,
            r(110.0),
            sh,
            label,
            col.fg,
            Align::Left,
        );
        g.font(r(10.0), false);
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
        draft.panel_acrylic,
    );
    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::Radius),
        "圆角窗口",
        draft.panel_radius,
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
        auto_on,
    );
    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::Topmost),
        "总是置顶",
        draft.bar.topmost,
    );
    draw_switch_row(
        g,
        c,
        lay,
        col,
        Hit::Switch(Setting::Diagnostics),
        "打印诊断信息",
        draft.diagnostics,
    );

    // 存储位置
    g.font(r(12.0), false);
    g.text(
        c.cx,
        r(262.0),
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
    g.text(c.cx, r(288.0), pw, r(22.0), &shown, col.muted, Align::Left);
    if let Some(rect) = hit_rect(lay, Hit::OpenDir) {
        draw_button(g, c, col, rect, "打开目录", 1, hover == Some(Hit::OpenDir));
    }
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

    // 和页头那行「关于」同号（17px 粗体）：原来 26px 比页标题大一圈，看着突兀
    g.font(r(17.0), true);
    g.text(
        c.cx,
        r(72.0),
        c.cr - c.cx,
        r(30.0),
        "LightBox",
        col.fg,
        Align::Left,
    );
    // 一句简介：整句常规字重，一行放得下（标题那行已经是大号粗体 LightBox）
    g.font(r(12.0), false);
    g.text(
        c.cx,
        r(104.0),
        c.cr - c.cx,
        r(20.0),
        ABOUT_TAGLINE,
        col.muted,
        Align::Left,
    );

    g.font(r(12.0), false);
    g.text(
        c.cx,
        r(134.0),
        c.cr - c.cx,
        r(20.0),
        &format!("版本 v{}", env!("CARGO_PKG_VERSION")),
        col.muted,
        Align::Left,
    );
    g.hline(c.cx, c.cr, r(164.0), col.line);

    // 一行信息可以点一下复制（开源链接），
    // 悬停变强调色、复制后右侧冒出「已复制」。右侧另有「打开」按钮。
    let copied = state().lock().unwrap().copied;
    for (i, (label, value)) in ABOUT_ROWS.iter().enumerate() {
        let y = r(184.0) + i as i32 * r(34.0);
        // 现在只剩「开源链接」这一行，值用强调色 + 粗体，当链接看
        let is_link = i == 0;
        let hot = hover == Some(Hit::CopyInfo(i));
        g.font(r(12.0), false);
        g.text(c.cx, y, r(90.0), r(24.0), label, col.muted, Align::Left);

        // 右侧留给「打开」按钮，文字宽度要收一收
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

    // 「检查更新」和上面的开源链接之间靠留白分层就够
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
        r(240.0),
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
    g.fill_round_aa(x, y, w, h, r(8.0), col.sunken);
    g.stroke_round_aa(x, y, w, h, r(8.0), col.line, 1);

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

/// 一行开关：左边标题，右边开关
fn draw_switch_row(
    g: &mut Gfx,
    c: &Ctx,
    lay: &Layout,
    col: &Colors,
    hit: Hit,
    title: &str,
    on: bool,
) {
    let Some(rect) = hit_rect(lay, hit) else { return };
    let text_w = (rect.left - c.cx - c.r(16.0)).max(c.r(60.0));
    // 标题和开关**同一条基线**：标题框贴开关的上下中缝居中。
    // 早先标题画在开关上方 14（那时下面还有一行说明小字），说明去掉后
    // 标题就和开关错开半个行高，看着是歪的。
    let th = c.r(20.0);
    let ty = rect.top + (rect.bottom - rect.top - th) / 2;
    g.font(c.r(12.0), false);
    g.text(c.cx, ty, text_w, th, title, col.fg, Align::Left);
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
    g.fill_round_aa(x, y, w, h, c.r(7.0), fill);
    if let Some(bd) = border {
        g.stroke_round_aa(x, y, w, h, c.r(7.0), bd, 1);
    }
    g.font(c.r(12.0), false);
    g.text(x, y, w, h, label, text, Align::Center);
}

/// 列表行里的「解散收纳盒 / 删除收纳盒」：细描边小按钮，
/// 嵌在 42 高的行里只占 26，颜色比正文略淡，不抢名字的注意力。
fn draw_row_button(
    g: &mut Gfx,
    c: &Ctx,
    col: &Colors,
    rect: RECT,
    label: &str,
    hot: bool,
    enabled: bool,
) {
    let (x, y) = (rect.left, rect.top);
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let radius = c.r(6.0);
    // 只留文字，不画描边也不铺底色。
    //
    // 原来是一圈浅灰描边的实体小按钮：78 宽里塞 5 个汉字，为了不贴边把字号
    // 压到 10px —— 而这个尺寸在 100% 下本就偏虚，到 125% 又取整成 13，
    // 两颗屏一比就像「字变大了」；盒子名 12px 一路是 15，反而稳定。
    // 现在去掉边框（边框才是显得拥塞的主因），按钮加宽到 88、字回到 11px，
    // 只在悬停时浮一层淡蓝底提示可点。
    if hot && enabled {
        g.fill_round_aa(x, y, w, h, radius, theme::mix(col.bg, col.accent, 0.14));
    }
    let text = if !enabled {
        theme::muted_on(col.bg)
    } else if hot {
        col.accent
    } else {
        theme::mix(col.fg, col.bg, 0.12)
    };
    g.font(c.r(11.0), false);
    g.text(x, y, w, h, label, text, Align::Center);
}

/// 列表下方的「+」新建按钮：整条主色底（和页脚「确定」同一套蓝）+ 白色加号
fn draw_add_button(g: &mut Gfx, c: &Ctx, col: &Colors, rect: RECT, hot: bool) {
    let (x, y) = (rect.left, rect.top);
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let radius = c.r(7.0);
    let fill = if hot {
        theme::mix(col.accent, theme::SNOW, 0.12)
    } else {
        col.accent
    };
    g.fill_round_aa(x, y, w, h, radius, fill);
    let ink = theme::text_on(col.accent);
    let bar = c.r(14.0);
    let thick = (c.r(2.0)).max(1);
    let cxp = x + w / 2;
    let cyp = y + h / 2;
    g.fill_rect(cxp - bar / 2, cyp - thick / 2, bar, thick, ink);
    g.fill_rect(cxp - thick / 2, cyp - bar / 2, thick, bar, ink);
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
        g.fill_round_aa(
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
                s.page = 0;
            }
            repaint();
        }
        Hit::BoxRow(i) => select_box(i),
        Hit::BoxName(i) => {
            select_box(i);
            start_edit(i);
        }
        Hit::BoxIcon(i) => pick_icon(i),
        Hit::BoxDissolve(i) => dissolve_box(i),
        Hit::BoxDelete(i) => delete_box(i),
        Hit::NewBox => new_box(),
        Hit::Scrollbar => start_scroll_drag(hwnd, y),
        Hit::BoxUp(i) => move_box(i, -1),
        Hit::BoxDown(i) => move_box(i, 1),
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
    let was_dragging = {
        let mut s = state().lock().unwrap();
        let d = s.drag_win.is_some() || s.scroll_drag.is_some();
        s.drag_win = None;
        s.scroll_drag = None;
        d
    };
    unsafe {
        let _ = ReleaseCapture();
    }
    if was_dragging {
        repaint();
    }
}

fn on_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let scale = display_scale();

    if state().lock().unwrap().scroll_drag.is_some() {
        drag_scroll_to(y, scale);
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
        // 滚动条拖的是位置，不是「点一下」——用箭头，别给手型
        let hand = hit.map_or(false, |h| clickable(h) && h != Hit::Scrollbar);
        unsafe {
            let cur = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).ok();
            let _ = SetCursor(cur);
        }
        repaint();
    }
}

fn on_wheel(delta: i16) {
    let scale = display_scale();
    let (tab, count, page, notes_scroll) = {
        let s = state().lock().unwrap();
        (s.tab, s.draft.boxes.len(), s.page, s.notes_scroll)
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
    // 收纳盒页：滚整页（盒子 + 下面那块设置一起动）
    if tab != Tab::Boxes {
        return;
    }
    // 名字输入框是独立子窗口，不跟着内容走：滚之前先提交，免得它悬在半空
    commit_inline_edit();
    let max_page = max_page(scale, count);
    if max_page <= 0 {
        return;
    }
    // 一格滚轮 ≈ 3 行，跟系统设置的节奏接近
    let step = (DROW_H * scale * 3.0).round() as i32;
    let next = if delta > 0 {
        (page - step).max(0)
    } else {
        (page + step).min(max_page)
    };
    if next != page {
        state().lock().unwrap().page = next;
        repaint();
    }
}

/// 整页最多能滚多远（设备像素）；装得下就是 0
fn max_page(scale: f32, count: usize) -> i32 {
    let (ctop, cbot) = content_band(scale);
    let view = cbot - ctop;
    let content_h = count as i32 * (DROW_H * scale).round() as i32
        + (DBLOCK_H * scale).round() as i32;
    (content_h - view).max(0)
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

/// 把第 i 个盒子滚进可视范围（新建 / 打开指定盒子时用）
fn scroll_into_view(i: usize) {
    let scale = display_scale();
    let (ctop, cbot) = content_band(scale);
    let view = cbot - ctop;
    let row_h = (DROW_H * scale).round() as i32;
    let mut s = state().lock().unwrap();
    let count = s.draft.boxes.len();
    let max = max_page(scale, count);
    let row_top = (i as i32) * row_h - s.page;
    if row_top < 0 {
        s.page = (s.page + row_top).max(0);
    } else if row_top + row_h > view {
        s.page = (s.page + row_top + row_h - view).min(max);
    }
}

// —— 列表下方的三个动作 ——
//
// 这三个都是**立刻生效**的：解散要动文件、删除要删目录、新建要建盒子，
// 没法像名字 / 图标那样攒到「确定」再一起写。做完同步一份到 draft / orig，
// 否则点「确定」时会被 apply_changes 的「以当前配置为准」又补回来。

/// 解散：盒子里所有图标放回桌面，盒子本身留着（变成空盒）
fn dissolve_box(i: usize) {
    let (id, name) = {
        let s = state().lock().unwrap();
        match s.draft.boxes.get(i) {
            Some(b) => (b.id.clone(), b.name.clone()),
            None => return,
        }
    };
    let count = config::count_items(&id);
    if count == 0 {
        return;
    }
    // 先问一句再动手：这一步不可撤销，图标会被真的搬回桌面。
    let hwnd = state().lock().unwrap().hwnd;
    let Some(h) = hwnd else { return };
    if !confirm(
        h,
        &format!(
            "「{name}」里的 {count} 个图标会被放回桌面，盒子保留为空。\n\n确定要解散吗？"
        ),
    ) {
        return;
    }
    let n = config::restore_all(&id);
    // 顺序表里的文件名已经不在了，顺手清掉，免得留下空洞
    {
        let mut s = state().lock().unwrap();
        config::prune_order(&mut s.draft, &id);
    }
    crate::bar::refresh();
    crate::panel::reload();
    log::info(&format!("解散收纳盒 {name}：{n} 个图标已放回桌面"));
    repaint();
}

/// 删除：只有空盒子能删（和右键菜单里的规则一致）
fn delete_box(i: usize) {
    let (id, name) = {
        let s = state().lock().unwrap();
        match s.draft.boxes.get(i) {
            Some(b) => (b.id.clone(), b.name.clone()),
            None => return,
        }
    };
    let count = config::count_items(&id);
    if count > 0 {
        if let Some(h) = state().lock().unwrap().hwnd {
            warn(
                h,
                &format!(
                    "「{name}」里还有 {count} 个图标。\n请先「解散收纳盒」把图标放回桌面，再删除。"
                ),
            );
        }
        return;
    }
    {
        let mut g = app().cfg.lock().unwrap();
        config::remove_box(&mut g, &id);
    }
    {
        let mut s = state().lock().unwrap();
        // draft 和 orig 都要摘掉：orig 留着的话「应用」会一直是可点的脏状态
        s.draft.boxes.retain(|b| b.id != id);
        s.orig.boxes.retain(|b| b.id != id);
        s.selected = s.selected.min(s.draft.boxes.len().saturating_sub(1));
    }
    clamp_page();
    crate::bar::refresh();
    crate::panel::reload();
    log::info(&format!("已删除收纳盒 {name}"));
    repaint();
}

/// 删掉盒子之后滚动位移可能越界（内容变短了），不夹一下会滚出一片空白。
fn clamp_page() {
    let scale = display_scale();
    let count = state().lock().unwrap().draft.boxes.len();
    let max = max_page(scale, count);
    let mut s = state().lock().unwrap();
    if s.page > max {
        s.page = max;
    }
}

/// 新建：直接走菜单里那一套（命名 + 存盘 + 长条滚到末尾）
fn new_box() {
    commit_inline_edit();
    crate::menu::add_box();
    let live = app().cfg.lock().unwrap().clone();
    let mut added = None;
    {
        let mut s = state().lock().unwrap();
        if let Some(nb) = live.boxes.last() {
            if !s.draft.boxes.iter().any(|b| b.id == nb.id) {
                s.draft.boxes.push(nb.clone());
                s.orig.boxes.push(nb.clone());
                added = Some(s.draft.boxes.len() - 1);
            }
        }
        if let Some(i) = added {
            s.selected = i;
        }
    }
    if let Some(i) = added {
        scroll_into_view(i);
    }
    repaint();
}

/// 按下滚动条：摸到手柄就拖，摸到轨道空白处就翻一页（随即也能接着拖）
fn start_scroll_drag(hwnd: HWND, y: i32) {
    let scale = display_scale();
    let (count, page, tab) = {
        let s = state().lock().unwrap();
        (s.draft.boxes.len(), s.page, s.tab)
    };
    if tab != Tab::Boxes {
        return;
    }
    let lay = layout(scale, Tab::Boxes, count, page, crate::updater::has_update());
    let Some((track, thumb)) = lay.scrollbar else {
        return;
    };
    let th = thumb.bottom - thumb.top;
    let max = max_page(scale, count);
    let view = track.bottom - track.top;
    let grab = if y >= thumb.top && y < thumb.bottom {
        y - thumb.top
    } else {
        // 点在轨道空白：先翻一页，再让手柄跟到鼠标位置
        let dir = if y < thumb.top { -1 } else { 1 };
        let next = (page + dir * view).clamp(0, max);
        state().lock().unwrap().page = next;
        th / 2
    };
    state().lock().unwrap().scroll_drag = Some(grab);
    unsafe {
        let _ = SetCapture(hwnd);
    }
    repaint();
}

/// 拖滚动条：手柄顶部跟手，换算回整页位移
fn drag_scroll_to(y: i32, scale: f32) {
    let grab = match state().lock().unwrap().scroll_drag {
        Some(g) => g,
        None => return,
    };
    let (count, page) = {
        let s = state().lock().unwrap();
        (s.draft.boxes.len(), s.page)
    };
    let max = max_page(scale, count);
    if max <= 0 {
        return;
    }
    let lay = layout(scale, Tab::Boxes, count, page, crate::updater::has_update());
    let Some((track, thumb)) = lay.scrollbar else { return };
    let area_h = track.bottom - track.top;
    let th = thumb.bottom - thumb.top;
    let span = (area_h - th).max(1);
    let ty = (y - grab - track.top).clamp(0, span);
    let next = (ty as f32 * max as f32 / span as f32).round() as i32;
    let next = next.clamp(0, max);
    if next != page {
        state().lock().unwrap().page = next;
        repaint();
    }
}

/// 上移 / 下移一个盒子（只改 draft）
fn move_box(i: usize, delta: i32) {
    let count = state().lock().unwrap().draft.boxes.len();
    let j = i as i32 + delta;
    if j < 0 || j as usize >= count {
        return;
    }
    let j = j as usize;
    {
        let mut s = state().lock().unwrap();
        s.draft.boxes.swap(i, j);
        s.selected = j;
    }
    scroll_into_view(j);
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

// —— 名字编辑 ——

fn start_edit(i: usize) {
    let scale = display_scale();
    let (id, name, hwnd, edit, count, page) = {
        let s = state().lock().unwrap();
        let Some(b) = s.draft.boxes.get(i) else { return };
        (
            b.id.clone(),
            b.name.clone(),
            s.hwnd,
            s.edit,
            s.draft.boxes.len(),
            s.page,
        )
    };
    let (Some(_hwnd), Some(edit)) = (hwnd, edit) else { return };
    let lay = layout(scale, Tab::Boxes, count, page, crate::updater::has_update());
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
    // 弹窗字体是固定值（微软雅黑 12px），设置里已经不提供修改；
    // 这里仍写一遍，保证把渲染器里的字体清成固定值并清掉文字位图缓存。
    next.panel_font = config::default_panel_font();
    next.panel_font_size = config::FONT_SIZE_BASE;
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
    crate::dialog::tell(Some(hwnd), msg);
}

/// 二次确认：点「确定」返回 true，点「取消」/ 关掉窗口返回 false。
///
/// 解散是不可撤销的（图标会真的被搬回桌面），所以走系统弹窗让用户点一次，
/// 免得误触行尾那颗按钮就把一盒图标全撒到桌面上。
fn confirm(hwnd: HWND, msg: &str) -> bool {
    crate::dialog::ask(Some(hwnd), msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(lay: &Layout, h: Hit) -> RECT {
        hit_rect(lay, h).expect("命中区应该存在")
    }

    /// 内容区顶到页头副标题下面 20 —— 原来空了一大块
    #[test]
    fn 收纳盒从内容区顶开始排() {
        let lay = layout(1.0, Tab::Boxes, 3, 0, false);
        assert_eq!(lay.rows.len(), 3);
        assert_eq!(lay.rows[0].top, (DCONTENT_TOP).round() as i32);
        assert_eq!(lay.rows[1].top - lay.rows[0].top, DROW_H as i32);
    }

    /// 5 个盒子刚好一屏放完：不出现滚动条，最后一块设置也不越过页脚线
    #[test]
    fn 五个盒子不用滚动() {
        let lay = layout(1.0, Tab::Boxes, 5, 0, false);
        assert!(lay.scrollbar.is_none());
        let (ctop, cbot) = content_band(1.0);
        let y0 = ctop + 5 * DROW_H as i32;
        let desc_bottom = y0 + DBLOCK_H as i32;
        assert!(desc_bottom <= cbot, "说明文字越过了页脚线：{desc_bottom} > {cbot}");
    }

    /// 6 个盒子开始整页滚动，滚动位移直接作用在行位置上
    #[test]
    fn 六个盒子出现滚动条且能滚动() {
        let lay = layout(1.0, Tab::Boxes, 6, 0, false);
        assert!(lay.scrollbar.is_some());
        let lay2 = layout(1.0, Tab::Boxes, 6, 20, false);
        assert_eq!(lay2.rows[0].top, lay.rows[0].top - 20);
        assert_eq!(max_page(1.0, 6), 6 * DROW_H as i32 + DBLOCK_H as i32 - {
            let (a, b) = content_band(1.0);
            b - a
        });
        assert_eq!(max_page(1.0, 5), 0);
    }

    /// 「+」紧贴最后一行（中间只留 DADD_GAP），不再是原来那一大块空白
    #[test]
    fn 加号按钮紧跟最后一行() {
        let lay = layout(1.0, Tab::Boxes, 3, 0, false);
        let last = *lay.rows.last().unwrap();
        let add = hit(&lay, Hit::NewBox);
        assert_eq!(add.top - last.bottom, 4 + DADD_GAP as i32);
        // 加宽 + 居中
        assert_eq!(add.right - add.left, DADD_W as i32);
        assert_eq!(add.bottom - add.top, DADD_H as i32);
        let center = (add.left + add.right) / 2;
        // 内容列 = DSIDEBAR + DPAD(=182) 到 DW - DPAD(=658)
        assert_eq!(center, (182 + 658) / 2);
    }

    /// 外观 / 功能页整体上移到内容区顶：首行不再悬在半空
    #[test]
    fn 外观与功能页内容贴顶() {
        assert_eq!(content_shift(Tab::Look, 1.0) + 110, DCONTENT_TOP as i32);
        // 功能页第一项是开关行，标题画在命中区上方 14
        assert_eq!(
            content_shift(Tab::Features, 1.0) + 128 - 14,
            DCONTENT_TOP as i32
        );
        assert_eq!(content_shift(Tab::Boxes, 1.0), 0);
        // 关于页整页用绝对坐标，靠同量的平移补上顶部留白
        assert_eq!(content_shift(Tab::About, 1.0), DTOP as i32);
    }

    /// 窗口固定件（侧栏 / 关闭 / 页脚三个按钮）不跟内容平移，也不吃内容区裁剪
    #[test]
    fn 侧栏与页脚是固定件() {
        for h in [
            Hit::Close,
            Hit::Nav(Tab::Boxes),
            Hit::Nav(Tab::Look),
            Hit::Apply,
            Hit::Cancel,
            Hit::Ok,
        ] {
            assert!(pinned_hit(&h), "{h:?} 应该是固定件");
        }
        for h in [
            Hit::BoxRow(0),
            Hit::BoxDissolve(0),
            Hit::BoxDelete(0),
            Hit::NewBox,
            Hit::ResetColor,
            Hit::Mode(false),
            Hit::Switch(Setting::HideLabel),
        ] {
            assert!(!pinned_hit(&h), "{h:?} 不该当固定件");
        }
    }

    /// 页脚按钮本来就在内容区下方：不加例外就会被内容区过滤掉，点了没反应
    #[test]
    fn 页脚按钮在内容区外() {
        let (_ctop, cbot) = content_band(1.0);
        for tab in [Tab::Boxes, Tab::Look, Tab::Features, Tab::About] {
            let lay = layout(1.0, tab, 3, 0, false);
            let ok = hit(&lay, Hit::Ok);
            assert!(ok.top >= cbot, "{tab:?} 页的「确定」应在内容区下方");
        }
    }

    /// 侧栏吸附位置与当前页无关：切到外观 / 功能页后，鼠标位置不能偏
    #[test]
    fn 侧栏命中区不随页面平移() {
        let a = layout(1.0, Tab::Boxes, 3, 0, false);
        for tab in [Tab::Look, Tab::Features, Tab::About] {
            let b = layout(1.0, tab, 3, 0, false);
            for t in TABS {
                assert_eq!(
                    hit(&a, Hit::Nav(t)),
                    hit(&b, Hit::Nav(t)),
                    "{tab:?} 页的侧栏位置变了"
                );
            }
        }
    }
}
