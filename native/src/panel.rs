//! 弹出面板：点长条上的分段，弹出这个收纳盒里的图标网格。
//!
//! 也是一个顶层分层窗口（逐像素透明 + 圆角）。面板不激活（WS_EX_NOACTIVATE），
//! 所以点它不会把当前窗口切走；「点面板外面关闭」用一个低级鼠标钩子实现 ——
//! 钩子只观察不拦截，点击照样穿透到下面的窗口。
//!
//! 面板里的三种手势（和旧版一致）：
//! - **单击**：打开图标
//! - **Ctrl + 单击**：多选；**空白处框选**：批量选
//! - **拖动**：在面板里挪动 = 重新排序；拖出面板 = 把这批图标拖回桌面

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{DROPEFFECT_MOVE, IDropTarget};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_A, VK_CONTROL, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, LoadCursorW,
    PostMessageW, RegisterClassW, SetWindowsHookExW, SetWindowPos, ShowWindow,
    UnhookWindowsHookEx, HC_ACTION, HHOOK, HTCLIENT, IDC_ARROW, IDC_HAND, KBDLLHOOKSTRUCT,
    MA_NOACTIVATE, MSLLHOOKSTRUCT, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER,
    SWP_SHOWWINDOW, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_DESTROY, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCALCSIZE, WM_NCHITTEST, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WNDCLASSW,
};

use crate::app::app;
use crate::config::{self, Item};
use crate::geom;
use crate::log;
use crate::render::{Canvas, IconBmp};
use crate::util::wide;

const CLASS_NAME: &str = "LightBoxPanel";

/// 面板收到「用户点了外面」的通知
const WM_CLICK_OUTSIDE: u32 = 0x8100;

/// 面板自己发起的拖出（拖回桌面）正在进行。
/// 这期间用户是在面板外松手的，不能当成「点了面板外面」。
static OUT_DRAG: AtomicBool = AtomicBool::new(false);

// —— 尺寸（设计单位 = 96 DPI 下的逻辑像素，实际按显示器缩放）——
//
// 尺寸照着**原版（带 WebView2 那版）的样式**来：图标 36、名字 12px、
// 一列 100、一格 84、格缝 4、标题栏 46。用户拿原版截图逐个量过，
// 这样换成原生版之后两边看着一样。
/// 网格四周留白
const D_PAD: f32 = 10.0;
/// 一列的宽（图标在这条里居中）
const D_CELL_W: f32 = 100.0;
/// 一格（图标 + 名字）的高
const D_CELL_H: f32 = 84.0;
/// 格与格之间的缝
const D_GAP: f32 = 4.0;
/// 图标边长
const D_ICON: f32 = 36.0;
/// 图标名的字号
const D_LABEL: f32 = 12.0;
/// 一行放几个图标
const MAX_COLS: usize = 4;
/// 图标太多、面板放不下时允许加宽到几列
const MAX_COLS_WIDE: usize = 8;
/// 网格下面留的一条空白带。框选（拉框选一批图标）得从这块空白起手 ——
/// 旧版面板的网格是 `align-content: start`，最后一行下面天然有空地。
const D_GUTTER: f32 = 30.0;
/// 面板头部的高度：盒子名 + 图标个数 + 「⋯」+「✕」
const D_HEAD_H: f32 = 46.0;
/// 头部下面的分隔线
const D_HEAD_LINE: f32 = 1.0;
/// 头部按钮（⋯ / ✕）的边长
const D_HEAD_BTN: f32 = 28.0;
/// 头部左右留白（左边比网格多 4，标题才不顶格）
const D_HEAD_PAD_L: f32 = 14.0;
/// 两个头部按钮之间的缝
const D_HEAD_BTN_GAP: f32 = 8.0;
/// 头部「有更新」小牌的宽（设计单位）
const D_UPD_BTN_W: f32 = 56.0;
/// 格子里图标上面的留白
const D_CELL_PAD: f32 = 6.0;
/// 图标和名字之间的缝
const D_CELL_GAP: f32 = 5.0;
/// 面板和长条（任务栏上沿）之间的间隙：别贴着，显得挤
const D_BAR_GAP: f32 = 6.0;

/// 指针移动超过这么多像素才算「拖动」
const DRAG_THRESHOLD: i32 = 6;
/// 指针跑出面板这么多像素就算「拖到桌面」
const OUT_MARGIN: i32 = 14;
/// 选中高亮的颜色
const SEL_COLOR: (u8, u8, u8) = (86, 148, 255);

/// 当前弹出窗口的缩放系数（乘 100 存成整数，用原子量存，省得每次都去问显示器）
static SCALE_100: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(100);

/// 记下这次的缩放系数（面板打开时按长条所在显示器算）
fn set_scale(s: f32) {
    let v = (s.clamp(0.75, 3.0) * 100.0).round() as u32;
    SCALE_100.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// 当前缩放系数
fn scale() -> f32 {
    SCALE_100.load(std::sync::atomic::Ordering::Relaxed) as f32 / 100.0
}

/// 设计单位 → 物理像素
fn px(v: f32) -> i32 {
    (v * scale()).round() as i32
}

/// 面板四周留白
fn pad() -> i32 {
    px(D_PAD)
}
/// 一个格子的宽
fn cell_w() -> i32 {
    px(D_CELL_W)
}
/// 格与格之间的缝
fn gap() -> i32 {
    px(D_GAP)
}
/// 一个格子的高
fn cell_h() -> i32 {
    px(D_CELL_H)
}
/// 图标边长
fn icon_size() -> i32 {
    px(D_ICON)
}
/// 图标名的字号
fn label_px() -> i32 {
    px(D_LABEL)
}
/// 网格下面那条空白带
fn gutter() -> i32 {
    px(D_GUTTER)
}
/// 头部高度
fn head_h() -> i32 {
    px(D_HEAD_H)
}
/// 头部按钮边长
fn head_btn() -> i32 {
    px(D_HEAD_BTN)
}
/// 网格从 y=多少开始（头部下面：分隔线 + 网格留白）
fn grid_top() -> i32 {
    head_h() + px(D_HEAD_LINE) + pad()
}

/// 拖动到了哪一步
#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    /// 还没超过阈值
    Idle,
    /// 在面板里挪动（重新排序）
    Reflow,
    /// 已经拖出去了（交给 OLE）
    Out,
}

/// 头部可点的三个区域
#[derive(Clone, Copy, PartialEq, Debug)]
enum HeadHit {
    /// 盒子名（点了改名）
    Title,
    /// 「⋯」：更多菜单
    More,
    /// 「✕」：关掉面板
    Close,
    /// 「有更新」提示（只在后台检查到新版本时出现）
    Update,
}

/// 右侧两个方按钮（更多 / 关闭）。
/// 单独抽出来：「有更新」小牌要挨着「⋯」排，但不能反过来调 head_hits。
fn right_buttons(w: i32) -> (RECT, RECT) {
    let btn_y = (head_h() - head_btn()) / 2;
    let close = RECT {
        left: w - pad() - head_btn(),
        top: btn_y,
        right: w - pad(),
        bottom: btn_y + head_btn(),
    };
    let more = RECT {
        left: close.left - px(D_HEAD_BTN_GAP) - head_btn(),
        top: btn_y,
        right: close.left - px(D_HEAD_BTN_GAP),
        bottom: btn_y + head_btn(),
    };
    (more, close)
}

/// 「有更新」小牌：紧挨「⋯」左边。没有新版本时它不存在，
/// 标题也就不用给它让位置。
fn update_hit(w: i32) -> Option<RECT> {
    if !crate::updater::has_update() {
        return None;
    }
    let (more, _) = right_buttons(w);
    let bw = px(D_UPD_BTN_W);
    Some(RECT {
        left: more.left - px(D_HEAD_BTN_GAP) - bw,
        top: more.top,
        right: more.left - px(D_HEAD_BTN_GAP),
        bottom: more.bottom,
    })
}

/// 头部可点区域（相对面板左上角）：标题、更多、关闭
fn head_hits(w: i32) -> (RECT, RECT, RECT) {
    let (more, close) = right_buttons(w);
    // 有更新时把标题的可用宽度让出小牌，长盒子名不会压到按钮上
    let reserve = update_hit(w)
        .map(|u| u.right - u.left + px(D_HEAD_BTN_GAP))
        .unwrap_or(0);
    let title = RECT {
        left: px(D_HEAD_PAD_L),
        top: 0,
        right: more.left - 8 - reserve,
        bottom: head_h(),
    };
    (title, more, close)
}

/// 点在头部的哪个区域
fn head_hit_at(x: i32, y: i32, w: i32, title_w: i32) -> Option<HeadHit> {
    let (_, more, close) = head_hits(w);
    let title = title_text_rect(w, title_w);
    let inside = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
    if inside(&close) {
        Some(HeadHit::Close)
    } else if inside(&more) {
        Some(HeadHit::More)
    } else if update_hit(w).map_or(false, |r| inside(&r)) {
        Some(HeadHit::Update)
    } else if inside(&title) {
        Some(HeadHit::Title)
    } else {
        None
    }
}

/// 盒子名文字那块（只有点到名字本身才算「点标题」）
///
/// 头部还有「⋯」「✕」和一大片空白，它们都不能触发改名 ——
/// 用户点空白就改名太容易误触了。
fn title_text_rect(w: i32, title_w: i32) -> RECT {
    let (title, more, _) = head_hits(w);
    RECT {
        left: title.left,
        top: title.top,
        right: (title.left + title_w.max(0)).min(more.left - 8),
        bottom: title.bottom,
    }
}

/// 排序预览的状态
#[derive(Clone)]
struct Sort {
    /// 落点下标。已经扣掉了被拖走的图标，所以是「剩下的图标」里的序号
    to: usize,
    /// 指针相对被拖图标原位的偏移
    dx: i32,
    dy: i32,
}

struct PanelState {
    hwnd: Option<HWND>,
    hook: Option<HHOOK>,
    /// 键盘钩子（接 Ctrl+A 全选）。面板不激活，普通消息里收不到键盘。
    kb_hook: Option<HHOOK>,
    box_id: String,
    items: Vec<Item>,
    /// 网格列数
    cols: usize,
    /// 网格行数
    rows: usize,
    /// 打开面板时长条所在的屏幕矩形（图标增减后要重新排版）
    bar_rect: RECT,
    /// 打开面板那个分段在长条里的横向位置和宽度（面板要对准它居中）
    seg_x: i32,
    seg_w: i32,
    origin: POINT,
    win_w: i32,
    win_h: i32,
    hover: Option<usize>,
    /// 指针停在头部的哪个按钮上
    head_hover: Option<HeadHit>,
    /// 盒子名文字的宽度（像素）：改名热区只包住这几个字
    title_w: i32,
    /// 按下左键时压在头部哪个按钮上
    head_press: Option<HeadHit>,
    /// 按下左键时压在第几个图标上
    press_idx: Option<usize>,
    /// 按下时的位置（用来判断是不是拖动）
    press_xy: POINT,
    /// 按下时 Ctrl 是不是按着
    ctrl_at_press: bool,
    drag_mode: DragMode,
    /// 这次拖动带走的图标下标
    batch: Vec<usize>,
    /// 排序预览
    sort: Option<Sort>,
    /// 框选矩形
    marquee: Option<RECT>,
    /// 选中的图标下标
    sel: Vec<usize>,
    /// 面板作为拖放目标
    drop_target: Option<IDropTarget>,
    /// 毛玻璃底图（屏幕快照，糊过的）
    backdrop: Option<IconBmp>,
}

static PANEL: OnceLock<Mutex<PanelState>> = OnceLock::new();

// 状态只在主线程里读写，HWND 只是句柄。
unsafe impl Send for PanelState {}

fn state() -> &'static Mutex<PanelState> {
    PANEL.get_or_init(|| {
        Mutex::new(PanelState {
            hwnd: None,
            hook: None,
            kb_hook: None,
            box_id: String::new(),
            items: Vec::new(),
            cols: 1,
            rows: 1,
            bar_rect: RECT::default(),
            seg_x: 0,
            seg_w: 0,
            origin: POINT::default(),
            win_w: 0,
            win_h: 0,
            hover: None,
            head_hover: None,
            title_w: 0,
            head_press: None,
            press_idx: None,
            press_xy: POINT::default(),
            ctrl_at_press: false,
            drag_mode: DragMode::Idle,
            batch: Vec::new(),
            sort: None,
            marquee: None,
            sel: Vec::new(),
            drop_target: None,
            backdrop: None,
        })
    })
}

fn x_of(lp: LPARAM) -> i32 {
    (lp.0 & 0xffff) as i16 as i32
}

fn y_of(lp: LPARAM) -> i32 {
    ((lp.0 >> 16) & 0xffff) as i16 as i32
}

/// 第几个格子的左上角
fn cell_origin(idx: usize, cols: usize) -> (i32, i32) {
    let col = idx % cols.max(1);
    let row = idx / cols.max(1);
    (
        pad() + col as i32 * (cell_w() + gap()),
        grid_top() + row as i32 * (cell_h() + gap()),
    )
}

/// 点到了第几个图标
fn item_at(x: i32, y: i32) -> Option<usize> {
    let s = state().lock().unwrap();
    if x < pad() || y < grid_top() {
        return None;
    }
    // 网格下面那条空白带不是图标 —— 在那儿按下是「框选」
    if y >= grid_top() + s.rows as i32 * (cell_h() + gap()) {
        return None;
    }
    let col = (x - pad()) / (cell_w() + gap());
    let row = (y - grid_top()) / (cell_h() + gap());
    if col >= s.cols as i32 {
        return None;
    }
    let idx = row as usize * s.cols + col as usize;
    if idx < s.items.len() {
        Some(idx)
    } else {
        None
    }
}

/// 指针位置对应的格子下标（越界就夹到最后一个）
fn index_at(x: i32, y: i32, n: usize, cols: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let cols = cols.max(1) as i32;
    let col = ((x - pad()) / (cell_w() + gap())).clamp(0, cols - 1);
    let row = ((y - grid_top()) / (cell_h() + gap())).max(0);
    let idx = row as usize * cols as usize + col as usize;
    idx.min(n - 1)
}

/// 落点下标。单选时用原表序号 —— 和旧版一致，图标正好落在指针指着的那一格。
/// 多选时换成「抽掉被拖图标之后」的序号，不然整批会整体差一格。
fn drop_at(x: i32, y: i32, n: usize, cols: usize, batch: &[usize]) -> usize {
    let full = index_at(x, y, n, cols);
    if batch.len() <= 1 {
        return full;
    }
    let before = batch.iter().filter(|&&b| b < full).count();
    full.saturating_sub(before)
}

/// 按图标数算列数、行数和面板大小。
/// 行数太多、塞不进工作区时就加宽（宁可宽一点也别让面板高到出屏）。
fn grid_size(n: usize, avail_h: i32) -> (usize, usize, i32, i32) {
    // 空盒子也按一排 4 格算宽度 —— 否则刚建出来只有一个窄条，没地方放图标
    let mut cols = if n == 0 {
        MAX_COLS
    } else if n == 1 {
        1
    } else {
        n.min(MAX_COLS)
    };
    let mut rows = if n == 0 { 1 } else { n.div_ceil(cols) };
    // 面板总高 = 头部 + 分隔线 + 网格 + 网格下面的空白带 + 底边留白
    let height = |rows: usize| {
        grid_top() + rows as i32 * cell_h() + (rows as i32 - 1).max(0) * gap() + gutter() + pad()
    };
    while height(rows) > avail_h && cols < MAX_COLS_WIDE {
        cols += 1;
        rows = n.div_ceil(cols);
    }
    let w = pad() * 2 + cols as i32 * cell_w() + (cols as i32 - 1).max(0) * gap();
    let h = height(rows);
    (cols, rows, w, h)
}

/// 面板摆哪：水平对准分段中心；垂直优先在长条**正上方**，留一个小间隙，
/// 上面放不下才翻到下方，两边都不许盖住长条本身。
fn place(seg_x: i32, seg_w: i32, bar: &RECT, win_w: i32, win_h: i32, work: &RECT) -> (i32, i32) {
    let mut x = bar.left + seg_x + seg_w / 2 - win_w / 2;
    if x + win_w > work.right {
        x = work.right - win_w;
    }
    if x < work.left {
        x = work.left;
    }
    let g = px(D_BAR_GAP);
    let above = bar.top - g - win_h;
    let below = bar.bottom + g;
    let mut y = if above >= work.top {
        above
    } else if below + win_h <= work.bottom {
        below
    } else {
        (work.bottom - win_h).max(work.top)
    };
    if y + win_h > work.bottom {
        y = (work.bottom - win_h).max(work.top);
    }
    (x, y)
}

/// Ctrl 键按着吗
fn ctrl_down() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

/// 这次拖动要带走的图标：多选状态里拖其中一个 = 整批
fn batch_for(s: &PanelState, from: usize) -> Vec<usize> {
    if s.sel.len() > 1 && s.sel.contains(&from) {
        s.sel.clone()
    } else {
        vec![from]
    }
}

/// 把 `batch` 挪到第 `to` 个位置
fn apply_reorder(items: &mut Vec<Item>, batch: &[usize], to: usize) {
    if batch.is_empty() {
        return;
    }
    let moved: Vec<Item> = batch
        .iter()
        .filter_map(|&i| items.get(i).cloned())
        .collect();
    if moved.is_empty() {
        return;
    }
    let mut rest: Vec<Item> = items
        .iter()
        .enumerate()
        .filter(|(i, _)| !batch.contains(i))
        .map(|(_, it)| it.clone())
        .collect();
    let at = to.min(rest.len());
    rest.splice(at..at, moved);
    *items = rest;
}

/// 框选矩形盖住了哪些图标
fn items_in_rect(items: &[Item], cols: usize, r: &RECT) -> Vec<usize> {
    let mut out = Vec::new();
    for i in 0..items.len() {
        let (cx, cy) = cell_origin(i, cols);
        let cell = RECT {
            left: cx,
            top: cy,
            right: cx + cell_w(),
            bottom: cy + cell_h(),
        };
        if cell.left < r.right && cell.right > r.left && cell.top < r.bottom && cell.bottom > r.top {
            out.push(i);
        }
    }
    out
}

/// 低级鼠标钩子：面板开着的时候盯着左键，在面板外松手就通知关闭。
///
/// 判定用「松手」而不是「按下」：从桌面往面板里拖图标时，按下发生在桌面
/// （面板外），如果按下就关，面板会在拖到一半时消失，放下就落空了。
/// 改成松手判定后，落在面板里的松手不会关面板，OLE 放下才能正常生效。
unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && (wp.0 as u32) == WM_LBUTTONUP {
        // 自己发起的拖出（面板 → 桌面）松手在面板外，不算「点了外面」
        if OUT_DRAG.load(Ordering::Relaxed) {
            return CallNextHookEx(None, code, wp, lp);
        }
        // 菜单开着的时候，菜单上的点击不算「点了外面」
        if crate::menu::is_open() {
            return CallNextHookEx(None, code, wp, lp);
        }
        // 改名窗开着的时候，键盘和鼠标都归它 —— 点输入框不算「点了外面」
        if crate::rename::is_open() {
            return CallNextHookEx(None, code, wp, lp);
        }
        let ms = &*(lp.0 as *const MSLLHOOKSTRUCT);
        let hwnd = state().lock().unwrap().hwnd;
        if let Some(h) = hwnd {
            let r = {
                let s = state().lock().unwrap();
                RECT {
                    left: s.origin.x,
                    top: s.origin.y,
                    right: s.origin.x + s.win_w,
                    bottom: s.origin.y + s.win_h,
                }
            };
            let p = ms.pt;
            let inside = p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
            // 点在长条上也不算「点了外面」—— 长条自己会处理（切盒子 / 收起面板）
            let on_bar = crate::bar::contains_screen(p.x, p.y);
            if !inside && !on_bar {
                let _ = PostMessageW(Some(h), WM_CLICK_OUTSIDE, WPARAM(0), LPARAM(0));
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// 鼠标指针是不是停在面板上
fn cursor_in_panel() -> bool {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let s = state().lock().unwrap();
    pt.x >= s.origin.x
        && pt.x < s.origin.x + s.win_w
        && pt.y >= s.origin.y
        && pt.y < s.origin.y + s.win_h
}

/// 选中全部图标
fn select_all() {
    let hwnd = {
        let mut s = state().lock().unwrap();
        s.sel = (0..s.items.len()).collect();
        s.hwnd
    };
    if let Some(h) = hwnd {
        repaint(h);
    }
}

/// 低级键盘钩子：接 Ctrl+A（全选）和 Esc（先清选择，再关面板）。
///
/// 面板是 WS_EX_NOACTIVATE 的，永远拿不到键盘焦点，WM_KEYDOWN 里收不到这些键
/// （旧版是 WebView，窗口有焦点才收得到）。改用全局钩子，但只在**鼠标指针就停在
/// 面板上**时才接管 —— 否则用户在别的程序里按 Ctrl+A 会被我们抢走，
/// 那比没有这个功能还糟。
unsafe extern "system" fn keyboard_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && (wp.0 as u32) == WM_KEYDOWN {
        // 改名窗开着的时候键盘归它：Esc 是「撤销改名」，不是「关面板」
        if crate::rename::is_open() {
            return CallNextHookEx(None, code, wp, lp);
        }
        if cursor_in_panel() {
            let ks = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            if ks.vkCode == VK_A.0 as u32 && ctrl_down() {
                select_all();
                // 吃掉这个键：不然前台的程序也会收到，在那边也选一遍
                return LRESULT(1);
            }
            if ks.vkCode == VK_ESCAPE.0 as u32 {
                // 和资源管理器一致：有选中先取消选择，再按一次才关面板
                let cleared = {
                    let mut s = state().lock().unwrap();
                    let had = !s.sel.is_empty();
                    s.sel.clear();
                    had
                };
                if cleared {
                    // 锁先放开再重画（同 refresh_head：临时守卫会活到语句结束）
                    let h = { state().lock().unwrap().hwnd };
                    if let Some(h) = h {
                        repaint(h);
                    }
                } else {
                    close();
                }
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE => LRESULT(0),
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_CLICK_OUTSIDE => {
            close();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_mouse_down(hwnd, x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_mouse_up(hwnd, x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            on_right_click(x_of(lp), y_of(lp));
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if wp.0 == 0x1b {
                close();
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 按下左键
fn on_mouse_down(hwnd: HWND, x: i32, y: i32) {
    // 改名窗是独立小窗，而面板是 WS_EX_NOACTIVATE —— 点面板不会让改名窗
    // 收到 WM_ACTIVATE，编辑状态就一直不收。这里主动提交并吞掉这一下：
    // 第一次点击只用来收起编辑，免得顺手误开程序 / 误触菜单。
    // （点改名窗自己不会走到这里；点到别的程序由 WM_ACTIVATE 负责提交。）
    if crate::rename::is_open() {
        crate::rename::commit();
        return;
    }
    let idx = item_at(x, y);
    let ctrl = ctrl_down();
    {
        let mut s = state().lock().unwrap();
        s.ctrl_at_press = ctrl;
        s.press_xy = POINT { x, y };
        s.drag_mode = DragMode::Idle;
        s.sort = None;
        s.batch.clear();
        // 头部按钮：先记下按在哪个上，松手时还在同一个按钮上才生效
        s.head_press = head_hit_at(x, y, s.win_w, s.title_w);
        if s.head_press.is_some() || y < head_h() {
            // 头部（标题 / 按钮 / 空白）不参与框选
            s.press_idx = None;
            s.marquee = None;
        } else {
            match idx {
                Some(i) => {
                    s.press_idx = Some(i);
                    s.marquee = None;
                }
                None => {
                    // 空白处：框选
                    s.press_idx = None;
                    s.marquee = Some(RECT {
                        left: x,
                        top: y,
                        right: x,
                        bottom: y,
                    });
                }
            }
        }
    }
    unsafe {
        let _ = SetCapture(hwnd);
    }
}

/// 移动：既管悬停高亮，也管拖动（排序 / 拖出）
fn on_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let mut start_out: Option<Vec<usize>> = None;
    let mut dirty = false;
    // item_at 自己也要拿锁，所以必须在下面这个大锁之前算好（锁不可重入）
    let hover_idx = item_at(x, y);
    let (win_w, title_w) = {
        let s = state().lock().unwrap();
        (s.win_w, s.title_w)
    };
    let hover_head = head_hit_at(x, y, win_w, title_w);
    {
        let mut s = state().lock().unwrap();
        // 「指针跑出面板」这个判断必须每次移动都做 —— 旧版 React 也是这么写的。
        // 否则第一次跨过阈值时（指针还在面板里）就切成排序态，之后再也没机会
        // 转成「拖出」，拖回桌面就永远触发不了。
        let outside = x < -OUT_MARGIN
            || y < -OUT_MARGIN
            || x > s.win_w + OUT_MARGIN
            || y > s.win_h + OUT_MARGIN;
        if s.press_idx.is_some() && outside && s.drag_mode != DragMode::Out {
            // 拖出面板 = 拖回桌面
            s.drag_mode = DragMode::Out;
            s.sort = None;
            s.batch = batch_for(&s, s.press_idx.unwrap());
            start_out = Some(s.batch.clone());
            dirty = true;
        } else {
            match s.drag_mode {
                DragMode::Idle => {
                    if let Some(i) = s.press_idx {
                        let dx = x - s.press_xy.x;
                        let dy = y - s.press_xy.y;
                        if dx.abs() > DRAG_THRESHOLD || dy.abs() > DRAG_THRESHOLD {
                            // 还在面板里 = 重新排序
                            s.drag_mode = DragMode::Reflow;
                            s.batch = batch_for(&s, i);
                            let rest = s.items.len().saturating_sub(s.batch.len());
                            let to = drop_at(x, y, s.items.len(), s.cols, &s.batch).min(rest);
                            s.sort = Some(Sort { to, dx, dy });
                            dirty = true;
                        }
                    } else if let Some(m) = s.marquee {
                        s.marquee = Some(RECT {
                            left: m.left.min(x),
                            top: m.top.min(y),
                            right: m.right.max(x),
                            bottom: m.bottom.max(y),
                        });
                        dirty = true;
                    }
                }
                DragMode::Reflow => {
                    let dx = x - s.press_xy.x;
                    let dy = y - s.press_xy.y;
                    let rest = s.items.len().saturating_sub(s.batch.len());
                    let to = drop_at(x, y, s.items.len(), s.cols, &s.batch).min(rest);
                    let changed = match &s.sort {
                        Some(t) => t.to != to || t.dx != dx || t.dy != dy,
                        None => true,
                    };
                    if changed {
                        s.sort = Some(Sort { to, dx, dy });
                        dirty = true;
                    }
                }
                DragMode::Out => {}
            }
        }

        // 悬停高亮（拖动中就不管了）
        if s.drag_mode == DragMode::Idle {
            if s.head_hover != hover_head {
                s.head_hover = hover_head;
                dirty = true;
            }
            if s.hover != hover_idx {
                s.hover = hover_idx;
                dirty = true;
                let on_button = hover_head.is_some();
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetCursor(
                        LoadCursorW(
                            None,
                            if hover_idx.is_some() || on_button {
                                IDC_HAND
                            } else {
                                IDC_ARROW
                            },
                        )
                        .ok(),
                    );
                }
            }
        }
    }

    if let Some(batch) = start_out {
        begin_drag_out(hwnd, batch);
    }
    if dirty {
        repaint(hwnd);
    }
}

/// 松开左键
fn on_mouse_up(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let _ = ReleaseCapture();
    }
    // 头部按钮：按下和松手都在同一个按钮上才生效
    let head = {
        let mut s = state().lock().unwrap();
        let pressed = s.head_press.take();
        s.head_hover = None;
        match pressed {
            Some(h) if head_hit_at(x, y, s.win_w, s.title_w) == Some(h) => Some(h),
            _ => None,
        }
    };
    if let Some(h) = head {
        match h {
            HeadHit::Close => {
                close();
                // 窗口已经销毁，不能再重画
                return;
            }
            HeadHit::More => {
                let id = state().lock().unwrap().box_id.clone();
                crate::menu::panel_header_menu(&id);
            }
            // 「有更新」只负责把人带到「关于」页，说明和下载入口都在那儿
            HeadHit::Update => crate::settings::open_about(),
            HeadHit::Title => {
                // 就地改名：弹一个带真 EDIT 的小窗（分层窗口里放不了输入框，
                // 而且真控件才支持中文输入法）
                let (id, origin, w) = {
                    let s = state().lock().unwrap();
                    (s.box_id.clone(), s.origin, s.win_w)
                };
                let name = {
                    let cfg = app().cfg.lock().unwrap().clone();
                    cfg.boxes
                        .iter()
                        .find(|b| b.id == id)
                        .map(|b| b.name.clone())
                        .unwrap_or_default()
                };
                let (title, _, _) = head_hits(w);
                let at = RECT {
                    left: origin.x + title.left,
                    top: origin.y + title.top,
                    right: origin.x + title.right,
                    bottom: origin.y + title.bottom,
                };
                crate::rename::open(&id, &name, at, scale(), hwnd);
            }
        }
        repaint(hwnd);
        return;
    }
    let mut launch: Option<usize> = None;
    let mut persist: Option<(String, Vec<String>)> = None;
    {
        let mut s = state().lock().unwrap();
        match s.drag_mode {
            DragMode::Reflow => {
                if let Some(sort) = s.sort.take() {
                    let batch = s.batch.clone();
                    apply_reorder(&mut s.items, &batch, sort.to);
                    persist = Some((
                        s.box_id.clone(),
                        s.items.iter().map(|it| it.file.clone()).collect(),
                    ));
                }
                s.drag_mode = DragMode::Idle;
                s.press_idx = None;
                s.batch.clear();
            }
            DragMode::Out => {
                s.drag_mode = DragMode::Idle;
                s.press_idx = None;
                s.batch.clear();
            }
            DragMode::Idle => {
                if let Some(m) = s.marquee.take() {
                    let hit = items_in_rect(&s.items, s.cols, &m);
                    if s.ctrl_at_press {
                        for i in hit {
                            if !s.sel.contains(&i) {
                                s.sel.push(i);
                            }
                        }
                    } else {
                        s.sel = hit;
                    }
                } else if let Some(i) = s.press_idx {
                    if s.ctrl_at_press {
                        // Ctrl + 点击：切选中
                        if let Some(p) = s.sel.iter().position(|&x| x == i) {
                            s.sel.remove(p);
                        } else {
                            s.sel.push(i);
                        }
                    } else {
                        s.sel = vec![i];
                        launch = Some(i);
                    }
                }
                s.press_idx = None;
            }
        }
    }
    if let Some((id, files)) = persist {
        let mut g = app().cfg.lock().unwrap();
        config::save_order(&mut g, &id, &files);
    }
    if let Some(i) = launch {
        // 启动成功时面板会被关掉，这时候再往那个窗口上重绘就是往已销毁的句柄上画
        launch_item(i);
        return;
    }
    repaint(hwnd);
}

/// 右键：头部和空白处都出「⋯」那个菜单；图标上对选中集生效
fn on_right_click(x: i32, y: i32) {
    let head_or_blank = y < head_h() || item_at(x, y).is_none();
    if head_or_blank {
        // 空白处（图标网格的空位）和标题栏走同一套动作：排序 / 解散，
        // 免得用户在面板里点右键什么都没有。
        let id = state().lock().unwrap().box_id.clone();
        crate::menu::panel_header_menu(&id);
        return;
    }
    let Some(i) = item_at(x, y) else { return };
    let batch: Vec<Item> = {
        let mut s = state().lock().unwrap();
        // 和资源管理器一致：右键点到未选中的图标上 = 只选中它；
        // 点到已选中的图标上 = 保持整个选中集，菜单对整批生效
        let idxs: Vec<usize> = if s.sel.len() > 1 && s.sel.contains(&i) {
            s.sel.clone()
        } else {
            vec![i]
        };
        s.sel = idxs.clone();
        idxs.iter().filter_map(|&b| s.items.get(b).cloned()).collect()
    };
    if !batch.is_empty() {
        crate::menu::items_menu(&batch);
    }
}

/// 打开某个盒子的面板。`seg_x` / `seg_w` 是被点分段在长条里的横向位置和宽度，
/// 面板以它为中心左右展开，并优先出现在长条**正上方**（和任务栏上沿留个小间隙）；
/// 上方放不下才翻到长条下方，两边都不许盖住长条本身。
pub fn open(box_id: &str, seg_x: i32, seg_w: i32, bar: &RECT) {
    let cfg = app().cfg.lock().unwrap().clone();
    let Some(b) = cfg.boxes.iter().find(|b| b.id == box_id) else {
        log::info(&format!("panel::open: 找不到盒子 id={box_id}"));
        return;
    };
    let items = config::list_items(&b.id, &b.order);

    // 已经开着同一个盒子 → 收起来
    {
        let s = state().lock().unwrap();
        if s.hwnd.is_some() && s.box_id == box_id {
            drop(s);
            close();
            return;
        }
    }
    close();

    let n = items.len();
    let anchor_x = bar.left + seg_x + seg_w / 2;
    let mon = geom::work_area_at(anchor_x, bar.bottom);
    // 面板的尺寸跟着长条所在显示器的缩放走
    set_scale(mon.scale as f32 / 96.0);
    let (cols, rows, win_w, win_h) = grid_size(n, mon.work_height() - 40);
    let (x, y) = place(seg_x, seg_w, bar, win_w, win_h, &mon.work);

    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            log::info(&format!("创建面板窗口失败：{e}"));
            return;
        }
    };
    // 毛玻璃底图必须在窗口挪到新位置**之前**截：此刻面板还挂在旧位置（或者还没露脸），
    // 目标那块是干净的桌面；等挪过去了再截，截到的就是面板自己。
    let acrylic = app().cfg.lock().unwrap().panel_acrylic;
    let backdrop = if acrylic {
        crate::glass::backdrop(x, y, win_w, win_h)
    } else {
        None
    };
    {
        let mut s = state().lock().unwrap();
        s.hwnd = Some(hwnd);
        s.box_id = box_id.to_string();
        s.items = items;
        s.cols = cols;
        s.rows = rows;
        s.bar_rect = *bar;
        s.seg_x = seg_x;
        s.seg_w = seg_w;
        s.origin = POINT { x, y };
        s.win_w = win_w;
        s.win_h = win_h;
        s.hover = None;
        s.head_hover = None;
        s.head_press = None;
        s.press_idx = None;
        s.sel.clear();
        s.sort = None;
        s.marquee = None;
        s.drag_mode = DragMode::Idle;
        s.batch.clear();
        s.backdrop = backdrop;
    }
    unsafe {
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0).ok();
        state().lock().unwrap().hook = hook;
        // 键盘钩子：面板不激活，收不到 WM_KEYDOWN，Ctrl+A 得靠它
        let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0).ok();
        state().lock().unwrap().kb_hook = kb;
        let _ = SetWindowPos(
            hwnd,
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOP),
            x,
            y,
            win_w,
            win_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    // 面板也是放置目标：把桌面上的图标拖进来就收进这个盒子
    let dt = crate::dnd::register_drop_target(hwnd, crate::dnd::DropMode::Panel);
    state().lock().unwrap().drop_target = dt;
    log::info(&format!(
        "面板就绪：{win_w}x{win_h} @ ({x},{y})，{n} 个图标"
    ));
    repaint(hwnd);
}

fn create_window() -> Result<HWND, String> {
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
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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
        .map_err(|e| e.to_string())
    }
}

/// 关掉面板
pub fn close() {
    let (hwnd, hook, kb_hook, dt) = {
        let mut s = state().lock().unwrap();
        let h = s.hwnd.take();
        let k = s.hook.take();
        let kb = s.kb_hook.take();
        let d = s.drop_target.take();
        s.head_hover = None;
        s.head_press = None;
        (h, k, kb, d)
    };
    unsafe {
        if let Some(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        if let Some(h) = kb_hook {
            let _ = UnhookWindowsHookEx(h);
        }
        if let Some(h) = hwnd {
            // 窗口销毁前必须先撤销放置目标
            crate::dnd::unregister_drop_target(h);
            let _ = DestroyWindow(h);
        }
    }
    drop(dt);
    // 改名窗是面板的附属窗口，面板没了它也该收掉
    crate::rename::close();
}

/// 启动某个图标
fn launch_item(idx: usize) {
    let s = state().lock().unwrap();
    let Some(it) = s.items.get(idx) else { return };
    let path = it.path.clone();
    drop(s);
    // 走 shortcut 模块：.lnk 会先解析出目标程序和工作目录再启动，
    // 否则依赖 cwd / 环境变量的程序（Electron 一类）会秒退，表现为「点了没反应」。
    match crate::shortcut::open(&path) {
        Ok(()) => {
            // 启动成功就收起面板：启动器类工具的常规交互，
            // 也免得启动的程序抢了前台后面板还杵在那儿。
            close();
        }
        Err(e) => log::info(&format!("启动失败：{path}（{e}）")),
    }
}

/// 把面板里的图标往外拖（OLE 拖放）。拖到桌面上放手，桌面会把它移回去。
fn begin_drag_out(hwnd: HWND, batch: Vec<usize>) {
    let paths: Vec<std::path::PathBuf> = {
        let s = state().lock().unwrap();
        batch
            .iter()
            .filter_map(|&i| s.items.get(i).map(|it| std::path::PathBuf::from(&it.path)))
            .collect()
    };
    if paths.is_empty() {
        return;
    }
    // 鼠标要交给 OLE 自己管
    unsafe {
        let _ = ReleaseCapture();
    }
    OUT_DRAG.store(true, Ordering::Relaxed);
    let effect = crate::dnd::start_drag_out(paths);
    OUT_DRAG.store(false, Ordering::Relaxed);
    {
        let mut s = state().lock().unwrap();
        s.drag_mode = DragMode::Idle;
        s.press_idx = None;
        s.batch.clear();
        s.sort = None;
    }
    if effect == DROPEFFECT_MOVE {
        crate::bar::refresh();
        reload();
    }
    let _ = hwnd;
}

/// 面板当前打开的是哪个盒子（拖放目标要靠它决定收进哪儿）
pub fn current_box_id() -> Option<String> {
    let s = state().lock().unwrap();
    if s.box_id.is_empty() {
        None
    } else {
        Some(s.box_id.clone())
    }
}

/// 面板现在开着这个盒子吗（长条上再点一次同一个盒子 = 收起来）
pub fn is_open_for(box_id: &str) -> bool {
    let s = state().lock().unwrap();
    s.hwnd.is_some() && s.box_id == box_id
}

/// 重画面板（改名窗开了又关，标题旁边那个数字牌要跟着回来）
pub fn refresh_head() {
    // 锁得先放开再重画：`if let` 的临时守卫会活到整个语句结束，
    // 那样 repaint() 里再锁同一把锁就是自己等自己 —— 面板当场卡死。
    let h = { state().lock().unwrap().hwnd };
    if let Some(h) = h {
        repaint(h);
    }
}

fn repaint(hwnd: HWND) {
    let (w, h, items, cols, hover, sel, sort, marquee, drag, box_id, head_hover) = {
        let s = state().lock().unwrap();
        (
            s.win_w,
            s.win_h,
            s.items.clone(),
            s.cols,
            s.hover,
            s.sel.clone(),
            s.sort.clone(),
            s.marquee,
            s.batch.clone(),
            s.box_id.clone(),
            s.head_hover,
        )
    };
    if w <= 0 || h <= 0 {
        return;
    }
    let cfg = app().cfg.lock().unwrap().clone();
    let bg = crate::util::parse_hex(&cfg.panel_bg).unwrap_or((0x5A, 0x60, 0x6B));
    // 文字颜色永远由背景色算出来（theme 的硬规则）：用户把面板调成浅色，
    // 标题和图标名就得跟着变深，不然看不清。
    let fg = crate::theme::text_on(bg);
    let muted = crate::theme::muted_on(bg);
    // 透明度百分比：0 = 完全不透明，50 = 最透。
    // 开了毛玻璃底板本来就更透（底下是玻璃，不是实色），老版就是这么算的。
    let t = cfg.panel_transparency.clamp(0.0, 100.0) / 100.0;
    let base = if cfg.panel_acrylic {
        0.64 - t * 0.28
    } else {
        1.0 - t
    };
    let alpha = (255.0 * base).round().clamp(0.0, 255.0) as u8;

    let mut canvas = Canvas::new(w, h);
    let radius = if cfg.panel_radius { 12.0 } else { 0.0 };

    // 毛玻璃：先把面板底下那块屏幕截下来糊掉当底，再压一层带透明度的底色。
    // 顺序很重要 —— 玻璃图在底下，底色（半透明）盖在上面，才有「磨砂」的层次。
    if cfg.panel_acrylic {
        let backdrop = state().lock().unwrap().backdrop.clone();
        if let Some(b) = backdrop {
            let scaled = crate::glass::scale_up(&b, w, h);
            canvas.blend_bitmap_round(0, 0, &scaled, radius);
        }
    }
    canvas.fill_round_rect(0, 0, w, h, radius, bg, alpha);

    // —— 头部 ——
    let title = cfg
        .boxes
        .iter()
        .find(|b| b.id == box_id)
        .map(|b| b.name.clone())
        .unwrap_or_else(|| "LightBox".into());
    // 改名热区只包住名字这几个字 —— 记下文字宽度，命中判定要用
    let title_w = crate::render::text(&title, px(13.0), true)
        .map(|t| t.w)
        .unwrap_or(0);
    state().lock().unwrap().title_w = title_w;
    draw_head(&mut canvas, w, &title, items.len(), fg, muted, head_hover);

    // 拖动中的那批图标：先从原位「抽掉」，剩下的按落点重排 ——
    // 这样预览和松手之后 apply_reorder 的结果一致
    let batch: Vec<usize> = if sort.is_some() { drag } else { Vec::new() };
    let at = sort.as_ref().map(|t| t.to).unwrap_or(0);
    let batch_len = batch.len();

    let mut slot = 0usize;
    for (i, it) in items.iter().enumerate() {
        if batch.contains(&i) {
            continue; // 被拖的那批最后单独画
        }
        let final_slot = if slot < at { slot } else { slot + batch_len };
        let (cx, cy) = cell_origin(final_slot, cols);
        let selected = sel.contains(&i);
        let hovered = hover == Some(i) && sort.is_none();
        draw_cell(&mut canvas, it, cx, cy, selected, hovered, fg);
        slot += 1;
    }

    // 被拖动的图标跟着指针走
    if let Some(t) = &sort {
        for &b in &batch {
            if let Some(it) = items.get(b) {
                let (cx, cy) = cell_origin(b, cols);
                draw_cell(&mut canvas, it, cx + t.dx, cy + t.dy, true, false, fg);
            }
        }
        // 落点提示：把要落进去的那几格点亮
        for k in 0..batch_len {
            let (tx, ty) = cell_origin(at + k, cols);
            canvas.fill_round_rect(tx, ty, cell_w(), cell_h(), 12.0, (255, 255, 255), 46);
        }
    }

    // 框选矩形
    if let Some(m) = &marquee {
        let mw = (m.right - m.left).max(1);
        let mh = (m.bottom - m.top).max(1);
        canvas.fill_round_rect(m.left, m.top, mw, mh, 0.0, SEL_COLOR, 34);
        stroke_rect(&mut canvas, m, SEL_COLOR, 180);
    }

    // 空盒子：一行提示，告诉用户往哪儿拖
    if items.is_empty() {
        draw_empty_hint(&mut canvas, w, h, muted);
    }

    // 防撞色描边：设置页和弹窗底色是同一个值，叠在一起时分不清哪块在前。
    // 沿形状内侧描 1px 由底色推导出来的对比色（浅底配深线、深底配浅线），
    // 两块同色窗口挨着也能一眼看出边界。画在最后，别被头部那条分隔线盖掉。
    let edge = crate::theme::mix(bg, fg, 0.32);
    if radius > 0.0 {
        canvas.stroke_round_rect(1, 1, w - 2, h - 2, radius - 1.0, edge, alpha);
    } else {
        canvas.stroke_round_rect(0, 0, w, h, 0.0, edge, alpha);
    }

    crate::render::push_layered(hwnd, &canvas);
}

/// 文字位图里真正有墨迹的范围，返回 **(x, y, 宽, 高)**（都是位图内坐标）。
/// GDI 位图带 ascent 空白，四周还留了抗锯齿余量；按 measure 的宽高或整张
/// 位图去居中，图标看着就会偏下 / 偏右。按墨迹框居中才真正落在按钮正中。
///
/// 注意别返回 (left, top, right, bottom)：调用方要算的是「墨迹中心」=
/// `(x + w/2, y + h/2)`，用右边界当宽会多算一个 `left/2` / `top/2`，
/// 图标就会整体偏左偏上（右上角 ⋯ / ✕ 曾经落在按钮中上部就是这个原因）。
fn ink_rect(bmp: &crate::render::TextBitmap) -> Option<(i32, i32, i32, i32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (bmp.w, bmp.h, -1, -1);
    for y in 0..bmp.h {
        for x in 0..bmp.w {
            if bmp.coverage[(y * bmp.w + x) as usize] > 8 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    (x1 >= 0).then_some((x0, y0, x1 + 1 - x0, y1 + 1 - y0))
}

fn draw_head(
    canvas: &mut Canvas,
    w: i32,
    title: &str,
    count: usize,
    fg: (u8, u8, u8),
    muted: (u8, u8, u8),
    hover: Option<HeadHit>,
) {
    let (tr, mr, cr) = head_hits(w);

    // 盒子名
    let title_txt = crate::render::text(title, px(13.0), true);
    if let Some(txt) = &title_txt {
        canvas.blend_text(tr.left, (head_h() - txt.h) / 2, &txt.bmp, fg);
    }

    // 图标个数：标题右边一个圆角小牌（改名时藏起来 —— 改名窗正好盖在那儿）
    if !crate::rename::is_open() {
        let cx = tr.left + title_txt.map(|t| t.w).unwrap_or(0) + px(8.0);
        // 粗体：11px 常规字在这个小圆角牌里笔画太细，缩略图尺度下几乎糊成灰点，
        // 换成 700 后数字轮廊才立得住。
        if let Some(txt) = crate::render::text(&count.to_string(), px(11.0), true) {
            let bw = txt.w + px(14.0);
            let bh = px(17.0);
            let by = (head_h() - bh) / 2;
            canvas.fill_round_rect(cx, by, bw, bh, bh as f32 / 2.0, fg, 30);
            canvas.blend_text(cx + px(7.0), by + px(1.0), &txt.bmp, muted);
        }
    }

    // 「有更新」：蓝底白字小牌，紧挨「⋯」左边。
    // 底色写死不用主题色 —— 面板底色可能很深，主题强调色配白字会看不清。
    if let Some(ur) = update_hit(w) {
        let bw = ur.right - ur.left;
        let bh = ur.bottom - ur.top;
        let base = if hover == Some(HeadHit::Update) {
            (66, 133, 244)
        } else {
            (47, 111, 235)
        };
        canvas.fill_round_rect(ur.left, ur.top, bw, bh, 6.0 * scale(), base, 255);
        if let Some(txt) = crate::render::text("有更新", px(10.0), true) {
            let (ix, iy, iw, ih) =
                ink_rect(&txt.bmp).unwrap_or((0, 0, txt.bmp.w, txt.bmp.h));
            canvas.blend_text(
                ur.left + bw / 2 - (ix + iw / 2),
                ur.top + bh / 2 - (iy + ih / 2),
                &txt.bmp,
                (255, 255, 255),
            );
        }
    }

    // 「⋯」和「✕」：悬停时给个底。字号比原来放大一档，
    // 并按「墨迹框」居中 —— 之前用 measure 的宽高去摆整张位图，图标会偏下偏右。
    let buttons = [
        (mr, "···", px(16.0), HeadHit::More),
        (cr, "×", px(18.0), HeadHit::Close),
    ];
    for (r, glyph, size, id) in buttons {
        let bw = r.right - r.left;
        let bh = r.bottom - r.top;
        if hover == Some(id) {
            canvas.fill_round_rect(r.left, r.top, bw, bh, 8.0, fg, 34);
        }
        if let Some(txt) = crate::render::text(glyph, size, false) {
            let (ix, iy, iw, ih) =
                ink_rect(&txt.bmp).unwrap_or((0, 0, txt.bmp.w, txt.bmp.h));
            // 让墨迹框中心落在按钮中心，而不是让位图左上角对齐
            canvas.blend_text(
                r.left + bw / 2 - (ix + iw / 2),
                r.top + bh / 2 - (iy + ih / 2),
                &txt.bmp,
                muted,
            );
        }
    }

    // 头部和网格之间的分隔线
    canvas.fill_round_rect(0, head_h(), w, px(D_HEAD_LINE), 0.0, fg, 26);
}

/// 空盒子的提示文字
fn draw_empty_hint(canvas: &mut Canvas, w: i32, h: i32, muted: (u8, u8, u8)) {
    let lines = ["把桌面上的快捷方式", "拖到这个盒子里"];
    let top = grid_top() + px(24.0);
    for (i, line) in lines.iter().enumerate() {
        if let Some(txt) = crate::render::text(line, px(12.0), false) {
            canvas.blend_text(
                (w - txt.w) / 2,
                top + i as i32 * (txt.h + px(4.0)),
                &txt.bmp,
                muted,
            );
        }
    }
    let _ = h;
}

/// 画一个格子（图标 + 名字）
fn draw_cell(
    canvas: &mut Canvas,
    it: &Item,
    cx: i32,
    cy: i32,
    selected: bool,
    hovered: bool,
    fg: (u8, u8, u8),
) {
    if selected {
        canvas.fill_round_rect(cx, cy, cell_w(), cell_h(), 12.0, SEL_COLOR, 80);
    } else if hovered {
        canvas.fill_round_rect(cx, cy, cell_w(), cell_h(), 12.0, (255, 255, 255), 28);
    }
    let size = icon_size();
    let icon_x = cx + (cell_w() - size) / 2;
    let icon_y = cy + px(D_CELL_PAD);
    if let Some(img) = &it.icon {
        let tmp = fit(img, size);
        canvas.blend_icon(icon_x + (size - tmp.w) / 2, icon_y, &tmp);
    }
    // 名字：按像素宽度截断，中文一个字宽、英文半个字宽，
    // 所以中文能显示 6 个、英文能显示十来个，比原来多不少。
    let max_w = cell_w() - px(8.0);
    if let Some(txt) = fit_label(&it.name, max_w) {
        canvas.blend_text(
            cx + (cell_w() - txt.w) / 2,
            icon_y + size + px(D_CELL_GAP),
            &txt.bmp,
            fg,
        );
    }
}

/// 图标名：按像素宽度截断（超了就加省略号）
fn fit_label(name: &str, max_w: i32) -> Option<crate::render::Text> {
    let mut s: String = name.chars().take(24).collect();
    if let Some(t) = crate::render::text(&s, label_px(), false) {
        if t.w <= max_w {
            return Some(t);
        }
    }
    // 放不下就一个字一个字地砍，末尾补省略号
    while !s.is_empty() {
        s.pop();
        let with = format!("{s}…");
        if let Some(t) = crate::render::text(&with, label_px(), false) {
            if t.w <= max_w {
                return Some(t);
            }
        }
    }
    crate::render::text("…", label_px(), false)
}

/// 画一个 1 像素的边框
fn stroke_rect(canvas: &mut Canvas, r: &RECT, color: (u8, u8, u8), alpha: u8) {
    let w = (r.right - r.left).max(1);
    let h = (r.bottom - r.top).max(1);
    canvas.fill_round_rect(r.left, r.top, w, 1, 0.0, color, alpha);
    canvas.fill_round_rect(r.left, r.bottom - 1, w, 1, 0.0, color, alpha);
    canvas.fill_round_rect(r.left, r.top, 1, h, 0.0, color, alpha);
    canvas.fill_round_rect(r.right - 1, r.top, 1, h, 0.0, color, alpha);
}

/// 等比缩放到 size×size
fn fit(src: &IconBmp, size: i32) -> IconBmp {
    if src.w == size && src.h == size {
        return src.clone();
    }
    let scale = size as f32 / src.w.max(src.h) as f32;
    let dw = ((src.w as f32 * scale).round() as i32).max(1);
    let dh = ((src.h as f32 * scale).round() as i32).max(1);
    let mut out = IconBmp::empty(dw, dh);
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

/// 外观变了（底色 / 透明度 / 毛玻璃 / 圆角）：把开着的面板重画一遍
pub fn apply_theme() {
    let (hwnd, x, y, win_w, win_h) = {
        let s = state().lock().unwrap();
        (s.hwnd, s.origin.x, s.origin.y, s.win_w, s.win_h)
    };
    let Some(h) = hwnd else { return };
    let cfg = app().cfg.lock().unwrap().clone();
    if cfg.panel_acrylic {
        // 面板正亮着，直接截会把它自己截进去（还带自己的半透明），
        // 所以先藏起来截一张，再放回来。ShowWindow 在本线程是同步的，
        // 中间那一帧用户看不见。
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
        let shot = crate::glass::backdrop(x, y, win_w, win_h);
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
        }
        state().lock().unwrap().backdrop = shot;
    } else {
        state().lock().unwrap().backdrop = None;
    }
    repaint(h);
}

/// 面板开着的时候，刷新内容（收纳/删除后调用）
pub fn reload() {
    // 锁的顺序统一成「先 cfg 后 state」
    let order = {
        let cfg = app().cfg.lock().unwrap();
        let id = state().lock().unwrap().box_id.clone();
        cfg.boxes
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.order.clone())
            .unwrap_or_default()
    };
    let (hwnd, id, bar, seg_x, seg_w) = {
        let s = state().lock().unwrap();
        (
            s.hwnd,
            s.box_id.clone(),
            s.bar_rect,
            s.seg_x,
            s.seg_w,
        )
    };
    let Some(h) = hwnd else { return };
    if id.is_empty() {
        return;
    }
    let items = config::list_items(&id, &order);
    // 文件被拖走/删掉后，顺序表里对应的条目要跟着清掉
    {
        let mut g = app().cfg.lock().unwrap();
        if config::prune_order(&mut g, &id) {
            config::save(&g);
        }
    }
    // 图标增减之后列数/行数会变，面板要重新排一遍
    let mon = geom::work_area_at(bar.left + seg_x + seg_w / 2, bar.bottom);
    let (cols, rows, win_w, win_h) = grid_size(items.len(), mon.work_height() - 40);
    let (x, y) = place(seg_x, seg_w, &bar, win_w, win_h, &mon.work);
    {
        let mut s = state().lock().unwrap();
        s.items = items;
        s.cols = cols;
        s.rows = rows;
        s.origin = POINT { x, y };
        s.win_w = win_w;
        s.win_h = win_h;
        s.sel.clear();
        s.sort = None;
        s.marquee = None;
        s.hover = None;
    }
    unsafe {
        let _ = SetWindowPos(
            h,
            None,
            x,
            y,
            win_w,
            win_h,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
        );
    }
    repaint(h);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize) -> Vec<Item> {
        (0..n)
            .map(|i| Item {
                name: format!("图标{i}"),
                file: format!("f{i}"),
                path: format!("C:\\f{i}"),
                icon: None,
            })
            .collect()
    }

    fn names(v: &[Item]) -> Vec<&str> {
        v.iter().map(|i| i.name.as_str()).collect()
    }

    /// 墨迹框必须返回 (x, y, 宽, 高)。曾经错返回 (left, top, right, bottom)，
    /// 调用方拿 right/bottom 当宽高算中心，⋯ / ✕ 就整体偏到按钮中上部。
    #[test]
    fn 墨迹框返回宽高而不是右下边界() {
        let mut bmp = crate::render::TextBitmap {
            w: 8,
            h: 7,
            coverage: vec![0u8; 8 * 7],
        };
        // 墨迹落在位图内部偏右下的地方，最能暴露「边界 vs 宽高」的区别
        bmp.coverage[3 * 8 + 4] = 255;
        bmp.coverage[3 * 8 + 5] = 255;
        bmp.coverage[4 * 8 + 4] = 255;
        assert_eq!(ink_rect(&bmp), Some((4, 3, 2, 2)));
    }

    #[test]
    fn 单个图标往后拖() {
        let mut v = items(4);
        apply_reorder(&mut v, &[1], 3);
        assert_eq!(names(&v), ["图标0", "图标2", "图标3", "图标1"]);
    }

    #[test]
    fn 第一个图标拖到最后一格() {
        // 旧版是 splice(from,1) 之后 splice(to,0,moved)：结果和这里一致
        let mut v = items(4);
        apply_reorder(&mut v, &[0], 3);
        assert_eq!(names(&v), ["图标1", "图标2", "图标3", "图标0"]);
    }

    #[test]
    fn 单个图标往前拖() {
        let mut v = items(4);
        apply_reorder(&mut v, &[3], 0);
        assert_eq!(names(&v), ["图标3", "图标0", "图标1", "图标2"]);
    }

    #[test]
    fn 原地不动() {
        let mut v = items(4);
        apply_reorder(&mut v, &[2], 2);
        assert_eq!(names(&v), ["图标0", "图标1", "图标2", "图标3"]);
    }

    #[test]
    fn 多个一起拖保持相对顺序() {
        let mut v = items(5);
        apply_reorder(&mut v, &[0, 2], 2);
        // 抽走 0、2 后剩下 [1,3,4]，把 [0,2] 插到第 2 位 -> [1,3,0,2,4]
        assert_eq!(names(&v), ["图标1", "图标3", "图标0", "图标2", "图标4"]);
    }

    #[test]
    fn 落点超出范围就贴到最后() {
        let mut v = items(3);
        apply_reorder(&mut v, &[0], 99);
        assert_eq!(names(&v), ["图标1", "图标2", "图标0"]);
    }

    #[test]
    fn 落点用的是原表序号() {
        // 单选：落点就是指针那一格（和旧版一致）
        assert_eq!(drop_at(pad(), grid_top(), 4, 4, &[0]), 0);
        let x = pad() + 3 * (cell_w() + gap());
        assert_eq!(drop_at(x, grid_top(), 4, 4, &[0]), 3);
    }

    #[test]
    fn 多选落点换算成抽掉之后的序号() {
        // 拖走 0 和 2，原第 4 格(下标 3)上的图标在剩下的列表里是第 1 位
        let x = pad() + 3 * (cell_w() + gap());
        assert_eq!(drop_at(x, grid_top(), 6, 6, &[0, 2]), 1);
    }

    #[test]
    fn 格子坐标() {
        // 格子之间留了缝，所以第 n 列 / 第 n 行都要把缝算进去
        assert_eq!(cell_origin(0, 4), (pad(), grid_top()));
        assert_eq!(
            cell_origin(3, 4),
            (pad() + 3 * (cell_w() + gap()), grid_top())
        );
        assert_eq!(
            cell_origin(4, 4),
            (pad(), grid_top() + cell_h() + gap())
        );
    }

    #[test]
    fn 框选能圈到跨排的图标() {
        let v = items(6);
        // 3 列 2 行；矩形右边界正好压在第 3 列的边上，所以第 3 列不算命中
        let r = RECT {
            left: pad(),
            top: grid_top(),
            right: pad() + cell_w() * 2,
            bottom: grid_top() + cell_h() * 2,
        };
        let hit = items_in_rect(&v, 3, &r);
        assert_eq!(hit, vec![0, 1, 3, 4]);
    }

    #[test]
    fn 框选空白处选不到() {
        let v = items(4);
        let r = RECT {
            left: pad() + cell_w() * 4,
            top: grid_top() + cell_h() * 3,
            right: pad() + cell_w() * 5,
            bottom: grid_top() + cell_h() * 4,
        };
        assert!(items_in_rect(&v, 4, &r).is_empty());
    }

    #[test]
    fn 头部三个区域不重叠() {
        let (title, more, close) = head_hits(300);
        assert!(title.right <= more.left);
        assert!(more.right <= close.left);
        // 都在头部高度内
        assert!(close.bottom <= head_h());
    }

    #[test]
    fn 头部命中判定() {
        let w = 300;
        let (title, more, close) = head_hits(w);
        let mid = |r: &RECT| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        assert_eq!(head_hit_at(mid(&close).0, mid(&close).1, w, 0), Some(HeadHit::Close));
        assert_eq!(head_hit_at(mid(&more).0, mid(&more).1, w, 0), Some(HeadHit::More));
        // 网格区域不算头部
        assert_eq!(head_hit_at(mid(&title).0, grid_top() + 4, w, 0), None);
    }

    #[test]
    fn 改名热区只包住名字文字() {
        let w = 300;
        let (title, _, _) = head_hits(w);
        let name_w = 60;
        let tr = title_text_rect(w, name_w);
        // 热区就是名字那几个字
        assert_eq!(tr.left, title.left);
        assert_eq!(tr.right, title.left + name_w);
        // 点在名字上才算点标题
        assert_eq!(
            head_hit_at(tr.left + 2, (tr.top + tr.bottom) / 2, w, name_w),
            Some(HeadHit::Title)
        );
        // 名字右边的空白点了没反应
        assert_eq!(
            head_hit_at(tr.right + 20, (tr.top + tr.bottom) / 2, w, name_w),
            None
        );
    }
}
