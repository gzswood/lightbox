mod autostart;
mod desktop;
mod dragsrc;
mod glass;
mod http;
mod iconpick;
mod icons;
mod label;
mod pick;
mod shortcut;
mod store;
mod updater;
mod win32;

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use store::{BoxConfig, BoxView, Config, Item, State};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};

/// 系统托盘图标（进程内唯一）。
///
/// 放静态里而不是 `mem::forget`：TrayIcon 的 Drop 会调
/// `Shell_NotifyIcon(NIM_DELETE)` 把图标从托盘里删掉。forget 掉的话进程退出时
/// Explorer 只会留一个幽灵图标，要等鼠标晃过才回收 —— 退出前必须显式 drop。
static TRAY: Mutex<Option<TrayIcon>> = Mutex::new(None);

/// 退出前调用：取出托盘图标并 drop，立刻从系统托盘移除。
fn destroy_tray() {
    if let Ok(mut g) = TRAY.lock() {
        if let Some(t) = g.take() {
            drop(t);
        }
    }
}

// 尺寸一律用**逻辑像素**（= CSS px）。真正落到窗口上时，`desktop::strip_chrome` /
// `reclip` 会乘上窗口所在显示器的 DPI 系数（100% / 125% / 150% …）。
const BOX_W: f64 = 80.0;
const BOX_RADIUS: i32 = 18;
/// 拖动松手后的吸附（逻辑像素，用的时候乘 DPI 系数）：
/// - `SNAP_DIST`：离多近才吸过去（盒子之间 / 屏幕边缘都按这个判断）
/// - `SNAP_GAP`：盒子之间吸附后留的空隙。0 = **紧贴**（用户要的就是紧贴，
///   圆角自然会在接缝处留出一点视觉空隙，不需要再额外加）
/// - `SNAP_EDGE`：贴屏幕边缘的触发距离，比盒子间距小一点，免得「想靠边但没靠上」也被吸走
const SNAP_DIST: f64 = 18.0;
const SNAP_GAP: f64 = 0.0;
const SNAP_EDGE: f64 = 14.0;
const PANEL_W: f64 = 432.0;
const PANEL_H: f64 = 480.0;
/// 面板圆角。必须和 styles.css 里 `.panel` 的 border-radius 一致（都是逻辑像素），
/// 「方形窗口」开关关掉时传 0。
const PANEL_RADIUS: i32 = 20;
const MENU_W: f64 = 198.0;
const MENU_RADIUS: i32 = 14;
const SET_W: f64 = 372.0;
/// 设置窗口的初始高度。页面加载完后会调 `fit_settings` 按内容高度改一次，
/// 所以这个值只是「还没量到内容之前」的占位。
const SET_H: f64 = 460.0;
/// 设置窗口固定直角（0 = 不裁剪）。小窗口带圆角在这套「窗口区域裁剪」方案下
/// 只能得到 1 像素台阶，看着不如直角干净；面板的圆角由设置里的开关单独控制。
const SET_RADIUS: i32 = 0;
/// 更新提示窗口：比设置窗口宽一点（要放更新说明），尺寸固定、内容超出才滚动。
const UPD_W: f64 = 420.0;
const UPD_H: f64 = 440.0;
const UPD_RADIUS: i32 = 0;

/// 为什么所有窗口都要 `SetWindowRgn` 裁圆角（而不是交给 CSS）：
///
/// tao 的 `transparent(true)` 是 `DwmEnableBlurBehindWindow` + **空**模糊区域 ——
/// DWM 会给**整个窗口矩形**铺一层「玻璃」（背后画面的模糊 + 一层浅色）。
/// 页面在圆角之外是 alpha 0，露出来的就是那一整块玻璃，于是圆角边上会多出一圈
/// **矩形的亮色/直角**（用户说的「后面还有一层窗口，圆角处露出直角」就是它）。
/// 用窗口区域把矩形裁成圆角，玻璃也一起被裁掉，缝隙就没了。
/// 代价是边缘是 1 像素台阶（区域只有「在/不在」两种），这是 Win10 上想同时保住
/// 逐像素透明和干净圆角的唯一折中。
pub(crate) fn hwnd_of(w: &tauri::WebviewWindow) -> isize {
    #[cfg(target_os = "windows")]
    {
        w.hwnd().map(|h| h.0 as isize).unwrap_or(0)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = w;
        0
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect()
}

// ---------------- 盒子窗口的 label 与「首帧已就绪」 ----------------

/// 每个盒子窗口的 label 是 `box-{id}-{gen}`，gen 单调递增。
///
/// 为什么要带序号：切换「置顶」必须换一种窗口形态（Progman 子窗口 ↔ 顶层窗口），
/// 而就地改父窗口会毁掉圆角和合成层（见 set_box_topmost 的注释），只能重建。
/// 重建时新旧两个窗口要在屏幕上同时存在一小会儿（旧的盖住新的，等新的画完再销毁），
/// label 就必须不同。查找窗口时按 `box-{id}-` 前缀匹配，不依赖序号。
static BOX_GEN: AtomicU64 = AtomicU64::new(1);

/// 前端报告「我这个盒子页面已经画好了」的集合（按盒子 id）。
/// 切置顶时后端等这个标记，避免新窗口先露一个空白帧。
static BOX_READY: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 自愈线程正在重建窗口（避免 3 秒轮询和「自动排序」同时动手）
static HEALING: AtomicBool = AtomicBool::new(false);

fn next_box_label(id: &str) -> String {
    format!("box-{id}-{}", BOX_GEN.fetch_add(1, Ordering::Relaxed))
}

/// 按 id 找当前的盒子窗口（label 里的序号不参与匹配）
fn box_window(app: &AppHandle, id: &str) -> Option<tauri::WebviewWindow> {
    let prefix = format!("box-{id}-");
    let mut fallback = None;
    for (label, win) in app.webview_windows() {
        if !label.starts_with(&prefix) {
            continue;
        }
        let h = hwnd_of(&win);
        // **僵尸条目**：盒子是挂在 Progman 下面的子窗口，Explorer 重启（换壁纸、
        // 系统更新、explorer.exe 崩溃后被拉起）会把 Progman 连同它所有子窗口一起销毁，
        // 但 Tauri 的窗口表里还留着那条记录。这种「表里有、窗口没了」的条目不能当
        // 「盒子还在」用 —— 否则后面所有摆位、显示都落在一个不存在的窗口上，
        // 表现就是「盒子突然全都不见了，自动排序也救不回来」。
        if h == 0 || unsafe { win32::IsWindow(h) } == 0 {
            continue;
        }
        // 优先返回「真的显示着」的那个（切置顶时可能同时存在两个）
        if unsafe { win32::IsWindowVisible(h) } != 0 {
            return Some(win);
        }
        fallback = Some(win);
    }
    fallback
}

/// 这个盒子 id 名下**所有**还活着的窗口。
///
/// 正常只应该有一个；形态切换的竞态（两处同时触发整团切换）会留下两个，
/// 两个都摆在桌面上就是用户看到的「同一个收纳盒被复制了」。
/// `rebuild_box_flashless` 靠它把多余的收干净。
fn box_windows_all(app: &AppHandle, id: &str) -> Vec<tauri::WebviewWindow> {
    let prefix = format!("box-{id}-");
    app.webview_windows()
        .into_iter()
        .filter(|(label, win)| {
            if !label.starts_with(&prefix) {
                return false;
            }
            let h = hwnd_of(win);
            // 僵尸条目（Explorer 重启后 hwnd 已经没了）不算：销毁它会卡在窗口表上
            h != 0 && unsafe { win32::IsWindow(h) } != 0
        })
        .map(|(_, w)| w)
        .collect()
}

fn mark_box_not_ready(id: &str) {
    BOX_READY.lock().unwrap().retain(|x| x != id);
}

fn wait_box_ready(id: &str, timeout_ms: u64) -> bool {
    let t0 = std::time::Instant::now();
    loop {
        if BOX_READY.lock().unwrap().iter().any(|x| x == id) {
            return true;
        }
        if t0.elapsed().as_millis() as u64 >= timeout_ms {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

/// 盒子页面画完首帧后由前端调用
#[tauri::command]
fn box_ready(id: String) {
    let mut v = BOX_READY.lock().unwrap();
    if !v.iter().any(|x| *x == id) {
        v.push(id);
    }
}

/// 右键菜单高度（逻辑像素）。结构对应 MenuView.tsx：
/// padding 6×2 + 头 28 + 边框 2 + 固定 7 项（新建/重命名/设置图标/置顶/
/// 设置.../删除收纳盒/退出）× 30 + 两处 sep × 9 + 2px 余量。
/// 非空盒子多一行提示；自定义过图标的多一项「恢复默认图标」。
///
/// 这只是**首次显示的估值**——菜单内容条件渲染复杂，估值难免有偏差，
/// 前端渲染后会测量真实高度回报 `fit_menu` 做修正，两者配合才不会裁掉底部。
fn menu_height(show_hint: bool, has_icon: bool, group_row: bool) -> f64 {
    // 基准含：标题 / 新建 / 重命名 / 设置图标 / 置顶 / 嵌入任务栏 / 设置...
    // + 两条分割线 + 删除 + 退出。前端渲染完还会回报真实高度修正（fit_menu）。
    let mut h = 302.0;
    if show_hint {
        h += 22.0;
    }
    if has_icon {
        h += 30.0;
    }
    if group_row {
        h += 30.0;
    }
    h
}

/// 单实例：多开会让每个进程各自建一套盒子窗口，互相重叠、互相抢桌面层父窗口，
/// 表现出来就是「盒子有重影」+「操作起来像卡死」。这里用命名互斥体直接挡掉第二个实例。
fn acquire_single_instance() -> bool {
    unsafe {
        let name = win32::wide("Local\\LightBox.SingleInstance.v1");
        let h = win32::CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        // 句柄故意不关闭：进程退出时由系统回收，锁也就随之释放
        !(h != 0 && win32::GetLastError() == win32::ERROR_ALREADY_EXISTS)
    }
}

/// 找一个不与现有盒子重叠的落点（按网格扫，留 24px 间隙）
/// 面板圆角：配置里关掉「圆角」时返回 0（方形窗口）。
fn panel_radius(app: &AppHandle) -> i32 {
    let round = app.state::<State>().cfg.lock().unwrap().panel_radius;
    if round {
        PANEL_RADIUS
    } else {
        0
    }
}

/// 主屏（或程序所在屏）的缩放系数。新建盒子要按物理像素排布时用。
/// **主显示器**的缩放系数（新建盒子的落点、避让网格都按它换算）。
///
/// 之前用 `app.primary_monitor()`：混合缩放的多屏下它不一定返回系统设定的主屏，
/// 结果「自动排序」把盒子排到了笔记本屏而不是主屏。现在直接问 Windows
/// 主显示器（坐标原点所在那块），并且用显示器自己的 DPI 而不是窗口的。
fn app_scale(_app: &AppHandle) -> f64 {
    desktop::primary_monitor().map(|m| m.scale).unwrap_or(1.0)
}

/// 盒子是否「浮在顶层」：用户手动置顶，或因为它压在任务栏上被自动浮起。
fn is_floating(b: &BoxConfig) -> bool {
    // Win11 的「嵌入任务栏」不是子窗口，而是置顶顶层窗口（见 desktop::win11()）
    b.topmost || (b.in_bar && desktop::win11())
}

/// 位置 (x, y)、边长 size（物理像素）的盒子是否「压在任务栏上」。
///
/// 判据和拖动吸附一致：下边界越进任务栏的深度**超过任务栏厚度的一半**。
/// 任务栏不在底部（顶部/侧边/自动隐藏）时恒为 false，这条功能自动失效。
fn on_taskbar(x: i32, y: i32, size: i32) -> bool {
    let (cx, cy) = (x + size / 2, y + size / 2);
    let bar = desktop::bottom_bar_at(cx, cy);
    if bar <= 0 {
        return false;
    }
    let work_bottom = match desktop::monitor_at(cx, cy) {
        Some(m) => m.work.y + m.work.h,
        None => return false,
    };
    (y + size) - work_bottom > bar / 2
}

/// 任务栏矩形（屏幕坐标，物理像素）。只支持「贴在屏幕底部、横跨整屏」的经典布局：
/// 自动隐藏、放在屏幕侧边、副屏任务栏都返回 None —— 那几种情况下「嵌入任务栏」没有意义。
fn taskbar_rect() -> Option<Rect> {
    let tb = desktop::taskbar();
    if tb == 0 {
        return None;
    }
    let r = desktop::window_rect(tb)?;
    let m = desktop::monitor_at(r.x + r.w / 2, r.y + r.h / 2)?;
    let full_bottom = m.full.y + m.full.h;
    let work_bottom = m.work.y + m.work.h;
    // ① 贴屏幕底（自动隐藏时任务栏会被推到屏幕外，这里就不成立）
    if (r.y + r.h - full_bottom).abs() > 1 {
        return None;
    }
    // ② 上沿正好是工作区下沿（确实是占了屏幕底部那一条）
    if (r.y - work_bottom).abs() > 1 {
        return None;
    }
    // ③ 横跨整屏（侧边任务栏宽度不对）
    if r.w < (m.full.w as f64 * 0.9) as i32 {
        return None;
    }
    // Win11：`Shell_TrayWnd` 的窗口矩形比它**画出来的那条**高 1px —— 顶上那一行是
    // 任务栏自己的描边（实测：任务栏 71..118，其中 71 是 1px 灰描边，72..118 才是底）。
    // 贴条压到描边行上，就会在任务栏上沿露出一条、看着「比任务栏高一点点」。
    // 所以把贴条的可用带整体下移 1px（高度也少 1px，底边不动）。Win10 的任务栏没有
    // 这行描边，保持原样。
    let d = if desktop::win11() { 1 } else { 0 };
    Some(Rect { x: r.x, y: r.y + d, w: r.w, h: r.h - d })
}

/// 任务栏厚度（物理像素）—— 嵌入任务栏后盒子的高度。
fn bar_size() -> i32 {
    taskbar_rect().map(|r| r.h).unwrap_or(0)
}

/// 「嵌入任务栏时隐藏名称」的全局镜像。
///
/// 正常应该从 `State` 里的配置读，但 `desktop::heal_bar_box`（任务栏守护线程）拿不到
/// State，而它必须算出「期望宽度」来纠偏 —— 于是把这一位同步成全局原子量：
/// 启动读配置时写一次，设置里改开关时写一次。
static BAR_HIDE_LABEL: AtomicBool = AtomicBool::new(false);

fn bar_hide_label() -> bool {
    BAR_HIDE_LABEL.load(Ordering::Relaxed)
}

fn set_bar_hide_label(v: bool) {
    BAR_HIDE_LABEL.store(v, Ordering::Relaxed);
}

/// 嵌入任务栏的盒子物理尺寸 `(宽, 高)`。
///
/// 高 = 任务栏厚度；宽 = 厚度 + 右侧两个汉字的位置（见 `desktop::bar_box_w`），
/// 所以它是个**横向的胶囊**，不是正方形 —— 图标在左，名字在右。
/// 设置里打开「嵌入任务栏时隐藏名称」后就变成正方形（只放图标）。
fn bar_box_size() -> (i32, i32) {
    let h = bar_size().max(16);
    (desktop::bar_box_w(h, bar_hide_label()), h)
}

/// 贴条的实际落位：横坐标夹进任务栏，**纵坐标一律取当前任务栏上沿**。
///
/// 配置里的 `y` 是上一次摆放时记下来的，改缩放、改任务栏高度、Explorer 重启、
/// 从别的屏拖过来之后都会差几像素。Win10 上贴条是任务栏的**子窗口**，高出那几像素
/// 会被父窗口的客户区裁掉，所以看不出来；Win11 上贴条是**顶层窗口**，没人裁它，
/// 于是那 2~4px 就明晃晃地露在任务栏上沿 —— 用户看到的「比任务栏稍微高一点点」。
///
/// 所以贴条的 y 永远不信任配置，每次摆位现取任务栏上沿。
fn bar_place_xy(b: &BoxConfig, bw: i32) -> (i32, i32) {
    match taskbar_rect() {
        Some(bar) => (
            clamp_i(b.x, bar.x, (bar.right() - bw).max(bar.x)),
            bar.y,
        ),
        None => (b.x, b.y),
    }
}

/// 把盒子拉回屏幕：按盒子中心找到它现在所在的显示器（不在任何屏上就是**最近的那块**），
/// 再把它整个夹进那块屏的工作区。已经在工作区里的一动不动。
///
/// 两种情况共用：
///  - 启动时修历史脏坐标（以前是「扔到一个固定网格点」，盒子会莫名其妙跑到屏幕中间）；
///  - 显示器配置变了以后重新落位（拔掉副屏、切「仅第二屏幕」、改分辨率/缩放/旋转）。
///
/// 顺带说明为什么必须重落位：盒子是 Progman 的**子窗口**，窗口坐标是父客户区坐标，
/// 而 Progman 的客户区原点会跟着虚拟屏幕原点跑（主屏在右边时是 -1920）。显示配置一变，
/// 原点到处跑，所有子窗口跟着整体平移 —— 这就是「切单屏后盒子全跑出屏幕」「切回双屏
/// 又跑到左边那块屏」的根因。配置里存的始终是屏幕坐标，重摆一遍就回来了。
fn clamp_box_into_screen(b: &mut BoxConfig) -> bool {
    // 嵌入任务栏的盒子：y 恒等于任务栏上沿、x 夹在任务栏范围内，不再按工作区算。
    if b.in_bar {
        let bar = match taskbar_rect() {
            Some(r) => r,
            None => {
                // 任务栏不可用（自动隐藏 / 挪到侧边 / 副屏）：退回普通盒子，
                // 否则它会停在屏幕外或者半悬空。
                b.in_bar = false;
                return true;
            }
        };
        let width = bar_box_size().0;
        let nx = clamp_i(b.x, bar.x, (bar.right() - width).max(bar.x));
        let ny = bar.y;
        if nx == b.x && ny == b.y {
            return false;
        }
        b.x = nx;
        b.y = ny;
        return true;
    }
    let sc = desktop::scale_at(b.x + BOX_W as i32 / 2, b.y + BOX_W as i32 / 2);
    let size = (BOX_W * sc).round() as i32;
    let (cx, cy) = (b.x + size / 2, b.y + size / 2);
    let m = match desktop::monitor_at(cx, cy) {
        Some(m) => m,
        None => return false,
    };
    // 用户**故意**把盒子压在任务栏上（下边界越进任务栏超过一半）：这次不要把它拽回工作区，
    // 下边界按整屏算，否则每次启动、每次切显示模式都会被拉上去，看起来像「位置没保存」。
    let bar = (m.full.y + m.full.h - (m.work.y + m.work.h)).max(0);
    let over_bar = bar > 0 && (b.y + size) - (m.work.y + m.work.h) > bar / 2;
    let bottom = if over_bar {
        m.full.y + m.full.h
    } else {
        m.work.y + m.work.h
    };
    let nx = clamp_i(b.x, m.work.x, (m.work.x + m.work.w - size).max(m.work.x));
    let ny = clamp_i(b.y, m.work.y, (bottom - size).max(m.work.y));
    if nx == b.x && ny == b.y {
        return false;
    }
    b.x = nx;
    b.y = ny;
    true
}

/// 首次启动（没有配置文件）时 N 个默认盒子的位置：主屏**工作区底部居中、
/// 水平依次排列**，间距 24 逻辑像素。
///
/// 工作区已经排除任务栏，所以底部那排永远露得全；多屏时落在主屏。
fn default_row(_app: &AppHandle, n: usize) -> Vec<(i32, i32)> {
    // 一次查全 **主显示器**：工作区 + 它自己的缩放。
    // （不用窗口/tauri 的 monitor API —— 混合缩放多屏下它们会给出另一块屏的数据，
    //  表现就是「自动排序把盒子排到了笔记本屏而不是系统主屏」。）
    let pm = desktop::primary_monitor();
    let scale = pm.map(|m| m.scale).unwrap_or(1.0);
    let size = (BOX_W * scale).round() as i32;
    // 盒子之间**紧贴**（0 间隙）：一排看起来是连续的一条，不重叠。
    // 圆角本身会留出视觉缝隙，所以不需要再额外加间距。
    let gap = 0;
    let area = pm
        .map(|m| m.work)
        .unwrap_or(Rect { x: 0, y: 0, w: 1920, h: 1080 });
    let n = n.max(1) as i32;
    let total = n * size + n.saturating_sub(1) * gap;
    let x0 = area.x + (area.w - total).max(0) / 2;
    let y = area.y + area.h - size;
    (0..n).map(|i| (x0 + i * (size + gap), y)).collect()
}

/// 找一个空位（`dedupe` 挪重叠盒子时用）：从主屏左上角起按网格扫，避开已有盒子。
fn free_spot(boxes: &[BoxConfig], scale: f64) -> (i32, i32) {
    // 坐标是物理像素，盒子尺寸是逻辑像素 —— 排布间距必须乘缩放系数，
    // 否则 125% 下 120px 的盒子会按 96px 的间距摆，互相贴住。
    let size = (BOX_W * scale).round() as i32;
    let step = size + (24.0 * scale).round() as i32;
    let (x0, y0) = ((280.0 * scale).round() as i32, (200.0 * scale).round() as i32);
    let (max_x, max_y) = ((1600.0 * scale).round() as i32, (900.0 * scale).round() as i32);
    for row in 0..8 {
        for col in 0..10 {
            let x = x0 + col * step;
            let y = y0 + row * step;
            if x + size > max_x || y + size > max_y {
                continue;
            }
            let clash = boxes
                .iter()
                .any(|b| (b.x - x).abs() < size + 8 && (b.y - y).abs() < size + 8);
            if !clash {
                return (x, y);
            }
        }
    }
    (x0, y0)
}

/// 启动时清理历史脏数据：早期版本多开会让每个进程各建一个「新收纳盒」，
/// 坐标只差 16px 完全叠在一起 —— 视觉上就是「圆角外多出一层半透明直角」。
///
/// **只挪不删（v0.9.19 起）**：以前这里会把「位置重叠且目录为空」的盒子整条删掉
/// （`remove_dir_all` + 从配置里摘除）。多屏那次坐标系 bug 会让新盒子落到别人身上，
/// 于是用户第二天启动就莫名少几个盒子 —— 这种「程序自作主张删用户的东西」不能再留。
/// 现在重叠的一律挪到空位，要删哪个盒子交给用户在右键菜单里决定。
///
/// 对应的兜底：启动时把配置快照成 `config.backup.json`（见 `store::snapshot`）。
fn dedupe(boxes: &mut Vec<BoxConfig>, scale: f64) -> bool {
    let mut changed = false;
    let mut kept: Vec<BoxConfig> = Vec::new();
    for mut b in boxes.drain(..) {
        // 嵌在任务栏里的盒子有自己的位置（贴任务栏上沿），既不能被挪走，也不参与去重
        if b.in_bar {
            kept.push(b);
            continue;
        }
        let dup = kept
            .iter()
            .filter(|k| !k.in_bar)
            .any(|k| (k.x - b.x).abs() < 32 && (k.y - b.y).abs() < 32);
        if dup {
            let (nx, ny) = free_spot(&kept, scale);
            b.x = nx;
            b.y = ny;
            changed = true;
        }
        kept.push(b);
    }
    *boxes = kept;
    changed
}

/// 显示器布局的「指纹」：只要它变了，就说明用户动了显示设置
/// （拔插屏、切换扩展/复制/仅某屏、改分辨率、改缩放、旋转屏幕）。
fn layout_sig() -> (i32, i32, i32, i32, i32) {
    unsafe {
        (
            win32::GetSystemMetrics(win32::SM_XVIRTUALSCREEN),
            win32::GetSystemMetrics(win32::SM_YVIRTUALSCREEN),
            win32::GetSystemMetrics(win32::SM_CXVIRTUALSCREEN),
            win32::GetSystemMetrics(win32::SM_CYVIRTUALSCREEN),
            win32::GetSystemMetrics(win32::SM_CMONITORS),
        )
    }
}

/// 显示配置变化后：把所有盒子按配置里的**屏幕坐标**重新摆一遍。
///
/// 为什么不只改坐标就行：盒子是 Progman 的子窗口，虚拟屏幕原点一变（例如切到
/// 「仅屏幕2」，原点从 -1920 变成 0），所有子窗口的屏幕位置就整体平移了。所以必须
/// 用 `strip_chrome` 重新换算一次父客户区坐标，盒子才会回到原来的位置。
///
/// 注意坐标**只夹不存**：夹出来的位置只用于「这次显示」，配置文件里仍是用户摆的原坐标。
/// 这样拔掉副屏时盒子临时落到主屏上，副屏插回来它会自己回到原来那块屏；
/// 如果这里直接落盘，一次临时拔线就把用户的布局永久改掉了。
fn reflow_boxes(app: &AppHandle) {
    let st = app.state::<State>();
    let boxes = {
        let cfg = st.cfg.lock().unwrap();
        cfg.boxes
            .iter()
            .map(|b| {
                let mut t = b.clone();
                clamp_box_into_screen(&mut t);
                t
            })
            .collect::<Vec<BoxConfig>>()
    };
    for b in &boxes {
        let w = match box_window(app, &b.id) {
            Some(w) => w,
            None => continue,
        };
        let hwnd = hwnd_of(&w);
        let scale = if b.in_bar {
            // 任务栏盒子：按新任务栏厚度重摆（圆角/尺寸都走 place_bar_box），
            // 这里**不能**用 reinforce_region —— 它按普通盒子的 80px 算区域。
            let (bw, bh) = bar_box_size();
            desktop::place_bar_box(hwnd, b.x, b.y, bw, bh, true);
            desktop::scale_at(b.x + bw / 2, b.y + bh / 2)
        } else {
            desktop::strip_chrome(hwnd, b.x, b.y, BOX_W as i32, BOX_W as i32, BOX_RADIUS, true);
            reinforce_region(hwnd);
            desktop::scale_at(b.x + BOX_W as i32 / 2, b.y + BOX_W as i32 / 2)
        };
        let _ = w.set_skip_taskbar(true);
        // 换屏（缩放不同）时页面要按新屏的缩放重排
        let _ = app.emit(
            "box:scale",
            BoxScale {
                id: b.id.clone(),
                scale,
            },
        );
    }
}

/// 盯住显示配置，一变就把盒子摆回去。
///
/// 用轮询而不是 `WM_DISPLAYCHANGE`：盒子/面板挂的是 Progman 子窗口、面板也不是我们
/// 想为收消息而长期占用的窗口，为一条消息单起一个消息循环窗口不划算。60ms 查一次
/// `GetSystemMetrics`（纯用户态、无锁）开销可以忽略，检测延迟 ≤ 60ms，
/// 用户基本看不到「盒子飞出去再飞回来」。
fn start_layout_watch(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = layout_sig();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(60));
            let now = layout_sig();
            if now == last {
                continue;
            }
            // 立刻按新布局摆一次；系统切模式时可能连着改几次配置，900ms 后再兜一次
            reflow_boxes(&app);
            std::thread::sleep(std::time::Duration::from_millis(900));
            reflow_boxes(&app);
            last = layout_sig();
        }
    });
}

/// 建一个盒子窗口。`visible=false` 时只建好并裁好形状，等调用方自己显示
/// （切置顶要靠这个：新窗口先藏着建好，等画完首帧再露出来）。
fn spawn_box_window(
    app: &AppHandle,
    b: &BoxConfig,
    visible: bool,
) -> Result<tauri::WebviewWindow, String> {
    let label = next_box_label(&b.id);
    let url = format!("index.html?mode=box&id={}", b.id);
    // 任务栏盒子的尺寸是**物理**像素（任务栏厚度 × 加宽后的宽），换算成 tao 要的逻辑尺寸
    let sc = desktop::scale_at(b.x + 1, b.y + 1).max(0.5);
    let (logical_w, logical_h) = if b.in_bar {
        let (bw, bh) = bar_box_size();
        (
            (bw as f64 / sc).round(),
            (bh as f64 / sc).round(),
        )
    } else {
        (BOX_W, BOX_W)
    };
    let mut builder = WebviewWindowBuilder::new(app, label, WebviewUrl::App(url.into()))
        .title("LightBox")
        .inner_size(logical_w, logical_h)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .skip_taskbar(true)
        .always_on_top(is_floating(b))
        .visible(false);
    // 桌面盒子真透明必须走 `WS_EX_NOREDIRECTIONBITMAP` + DirectComposition。
    //
    // 默认的 `transparent(true)` 在 tao 里是 `DwmEnableBlurBehindWindow(空区域)`
    // （tao-0.37.1 window.rs:1204-1216）：DWM 会把这个窗口当成「玻璃窗」，
    // 于是**重新接管非客户区绘制**，在窗口顶部画一条约 20px 的标题栏
    // （浅灰底 + 窗口标题），页面背景一透明它就露出来了。
    // 这也是为什么 ExtendFrameIntoClientArea(0)、NCRENDERING_POLICY、
    // WM_NCCALCSIZE 全部无效 —— 它们都没碰 blur-behind 这条路。
    //
    // 带上这个标志后，tao 会跳过整个 blur-behind 分支，窗口没有重定向表面，
    // 由 WebView2 的 DComp 直接合成，alpha=0 的像素真正透出壁纸，且没有标题栏。
    // 只对桌面盒子用；任务栏胶囊是 Shell_TrayWnd 的子窗口，本来就没有非客户区。
    if !b.in_bar {
        builder = builder.no_redirection_bitmap(true);
    }
    let win = builder.build().map_err(|e| e.to_string())?;

    let hwnd = hwnd_of(&win);
    if b.in_bar {
        // 真嵌入：挂成 Shell_TrayWnd 的子窗口。任务栏提层、重绘都盖不住它，
        // 因为它和父窗口在同一个 z 带里。
        desktop::pin_to_taskbar(hwnd);
        let (bw, bh) = bar_box_size();
        let (x, y) = bar_place_xy(b, bw);
        desktop::place_bar_box(hwnd, x, y, bw, bh, false);
    } else {
        // 浮起来的盒子（手动置顶 / 压在任务栏上）是**顶层窗口**，不进桌面层 ——
        // 进了桌面层就会被任务栏盖住，压在任务栏上也看不见。
        if !is_floating(b) && !desktop::pin_to_desktop(hwnd) {
            // Progman 这会儿拿不到（Explorer 正在重启）。盒子此刻是个普通顶层窗口，
            // 会浮在所有窗口之上 —— 先沉到最底藏起来，守护线程拿到 owner 后会把它提回来。
            unsafe {
                win32::SetWindowPos(
                    hwnd,
                    win32::HWND_BOTTOM,
                    0,
                    0,
                    0,
                    0,
                    win32::SWP_NOMOVE | win32::SWP_NOSIZE | win32::SWP_NOACTIVATE,
                );
            }
        }
        // 先规范一次（去掉标题栏/边框、定尺寸、裁圆角）
        desktop::strip_chrome(hwnd, b.x, b.y, BOX_W as i32, BOX_W as i32, BOX_RADIUS, false);
    }
    if visible {
        show_box(&win, b);
    }
    Ok(win)
}

/// 显示盒子窗口。
///
/// **不要用 tao 的 `win.show()`**：它在 Windows 上会重新套一遍窗口样式，
/// 把 `WS_CAPTION` 又加回来 —— 那一帧就是用户能「隐约看到的标题栏」。
/// 这里直接用 Win32 显示（strip_chrome / place_bar_box 里的 SWP_SHOWWINDOW），
/// 完全不碰 tao 的样式逻辑。
fn show_box(win: &tauri::WebviewWindow, b: &BoxConfig) {
    let hwnd = hwnd_of(win);
    if b.in_bar {
        let (bw, bh) = bar_box_size();
        // y 现取任务栏上沿，不用配置里的旧值（见 bar_place_xy 的注释）
        let (x, y) = bar_place_xy(b, bw);
        desktop::place_bar_box(hwnd, x, y, bw, bh, true);
        // tao 建窗之后的一小段时间里可能把样式/区域改回矩形（和 reinforce_region 同理），
        // 补一次就行 —— 任务栏里的盒子只要重摆，代价可以忽略。
        std::thread::spawn(move || {
            for delay in [260u64, 340u64] {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                if unsafe { win32::IsWindow(hwnd) } == 0 {
                    return;
                }
                desktop::place_bar_box(hwnd, x, y, bw, bh, true);
            }
        });
    } else {
        desktop::strip_chrome(hwnd, b.x, b.y, BOX_W as i32, BOX_W as i32, BOX_RADIUS, true);
        reinforce_region(hwnd);
    }
    let _ = win.set_skip_taskbar(true);
}

/// 延迟补圆角区域：不改位置、不改样式，只重新 SetWindowRgn。
fn reinforce_region(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    std::thread::spawn(move || {
        for delay in [60u64, 260u64, 340u64] {
            std::thread::sleep(std::time::Duration::from_millis(delay));
            if unsafe { win32::IsWindow(hwnd) } == 0 {
                return;
            }
            desktop::reclip(hwnd, BOX_W as i32, BOX_W as i32, BOX_RADIUS);
        }
    });
}

/// 把「配置里有、窗口已经没了」的盒子重新建出来。
///
/// 什么时候会发生：盒子是 **Progman 的子窗口**，explorer.exe 重启（系统更新、
/// 换壁纸设置、崩溃后被拉起、装某些软件）会销毁 Progman 并换一个新的 —— 挂在它
/// 下面的盒子窗口被系统一并销毁，Tauri 窗口表里留下一条僵尸记录。
/// 以前的表现：桌面上 5 个盒子**突然全部消失**，托盘「自动排序」也救不回来
/// （它只对还活着的窗口摆位），只能重启程序；而新建的盒子却正常出现。
///
/// 这里按配置里的 id 逐个补建：先全建好（隐藏），再统一等首帧、统一显示，
/// 免得 5 个盒子一个一个慢慢往外冒。
fn rebuild_missing_boxes(app: &AppHandle) {
    // 分两类：
    //  ① 窗口真的没了（Explorer 重启）→ 建一个新的；
    //  ② 窗口在、但「是半成品」→ 重建。半成品有两种：
    //      · 不可见（样式丢了 / 建了一半）；
    //      · 配置说不是任务栏盒子，窗口却还挂在任务栏下面（切换形态的中途被
    //        打断留下的）。这种最阴：它只能显示在任务栏那一条里，摆到工作区
    //        会被父窗口裁掉 —— 表现是「任务栏里看不到、桌面上也看不到，
    //        自动排序也没反应」（自动排序只处理非 in_bar 盒子，摆位也是往
    //        任务栏坐标系里摆，等于白摆）。
    let (missing, broken) = {
        let st = app.state::<State>();
        let cfg = st.cfg.lock().unwrap();
        let mut missing = Vec::new();
        let mut broken = Vec::new();
        for b in cfg.boxes.iter() {
            // 正在做形态切换的盒子不算异常（它本来就有一段新旧交替的窗口期）
            if switching(&b.id) {
                continue;
            }
            match box_window(app, &b.id) {
                None => missing.push(b.clone()),
                Some(w) => {
                    let h = hwnd_of(&w);
                    let visible = h != 0 && unsafe { win32::IsWindowVisible(h) } != 0;
                    // 普通盒子却还挂在任务栏下面 = 形态切换被打断留下的半成品。
                    // （Win11 贴条掉层级不用在这里管：`heal_bar_box` 每 2.5 秒会把它提回 topmost，
                    //  走重建太重，反而会闪。）
                    let wrong_parent = !b.in_bar && desktop::parent_is_taskbar(h);
                    if !visible || wrong_parent {
                        broken.push(b.clone());
                    }
                }
            }
        }
        (missing, broken)
    };
    if missing.is_empty() && broken.is_empty() {
        return;
    }
    // Explorer 正在重启的那一小段时间里 desktop_host() 是 0，这时候建出来也挂不上桌面，
    // 等下一轮（3 秒后）再试。
    if desktop::desktop_host() == 0 {
        return;
    }
    let mut built = Vec::new();
    for b in &missing {
        match spawn_box_window(app, b, false) {
            Ok(w) => built.push((b.clone(), w)),
            Err(e) => eprintln!("重建收纳盒窗口失败（{} / {}）：{e}", b.name, b.id),
        }
    }
    for (b, w) in built {
        // 等页面画完首帧再显示，避免先露一帧白底
        wait_box_ready(&b.id, 900);
        show_box(&w, &b);
    }
    // 半成品只能整个重建（改父窗口会把 WebView 的合成层弄坏，见 rebuild_box_flashless）
    for b in &broken {
        // 重建期间挂个牌子：这一刻的窗口移动不是用户拖动
        let _sg = switching_begin(&b.id);
        if let Err(e) = rebuild_box_flashless(app, b) {
            eprintln!("修复收纳盒窗口失败（{} / {}）：{e}", b.name, b.id);
        }
    }
}

/// 同一个盒子名下有**多个**窗口时，只留一个（其余销毁）。
///
/// 正常一个盒子只有一个窗口；形态切换被两处同时触发时会漏下一个，
/// 两个窗口都画着同一个盒子 = 用户看到的「收纳盒被复制了一个」。
/// 重建过程本身会短暂出现两个窗口（新的画好了才销毁旧的），所以正在切换的跳过。
fn prune_duplicate_boxes(app: &AppHandle) {
    let ids: Vec<String> = {
        let st = app.state::<State>();
        let cfg = st.cfg.lock().unwrap();
        cfg.boxes.iter().map(|b| b.id.clone()).collect()
    };
    for id in ids {
        if switching(&id) {
            continue;
        }
        let wins = box_windows_all(app, &id);
        if wins.len() < 2 {
            continue;
        }
        // 留「可见的那个」，没有可见的就留第一个
        let keep = wins
            .iter()
            .position(|w| unsafe { win32::IsWindowVisible(hwnd_of(w)) } != 0)
            .unwrap_or(0);
        for (i, w) in wins.iter().enumerate() {
            if i == keep {
                continue;
            }
            desktop::forget(hwnd_of(w));
            let _ = w.destroy();
            eprintln!("多余的收纳盒窗口已清理：{}", w.label());
        }
    }
}

/// 每 3 秒看一遍：配置里的盒子是不是都还有窗口，缺的就补建。
///
/// 只是「查一下有没有少」的成本极低（遍历几个字符串），真正建窗口只在缺失时才发生。
fn start_box_heal(app: AppHandle) {
    std::thread::spawn(move || {
        // 启动时 setup 刚建完所有窗口，先让它们安顿（也避开和 set_topmost 抢跑）
        std::thread::sleep(std::time::Duration::from_secs(10));
        loop {
            desktop::prune_dead();
            // 每轮都过一遍（遍历窗口表，成本可以忽略）：除了「窗口没了」，
            // 还要能发现「窗口在但是半成品」（不可见 / 父窗口挂错），
            // 否则那种盒子会一直「两边都看不见」且自动排序也救不回来。
            if !HEALING.swap(true, Ordering::SeqCst) {
                let a = app.clone();
                tauri::async_runtime::spawn(async move {
                    prune_duplicate_boxes(&a);
                    rebuild_missing_boxes(&a);
                    HEALING.store(false, Ordering::SeqCst);
                });
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    });
}

fn spawn_panel(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    let win = WebviewWindowBuilder::new(app, "panel", WebviewUrl::App("index.html?mode=panel".into()))
        .title("LightBox")
        .inner_size(PANEL_W, PANEL_H)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), 0, 0, PANEL_W as i32, PANEL_H as i32, PANEL_RADIUS, false);
    Ok(win)
}

/// 设置窗口：独立顶层窗口（不再叠在面板里）。
///
/// 放在面板中央弹出，视觉上仍然像「这个盒子的设置」，但它是自己的窗口 ——
/// 圆角、阴影、失焦关闭都按独立窗口处理，也不会把面板的内容挡住一半。
fn spawn_settings(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    let win = WebviewWindowBuilder::new(
        app,
        "settings",
        WebviewUrl::App("index.html?mode=settings".into()),
    )
    .title("LightBox 设置")
    .inner_size(SET_W, SET_H)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .skip_taskbar(true)
    .always_on_top(true)
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), 0, 0, SET_W as i32, SET_H as i32, SET_RADIUS, false);
    Ok(win)
}

/// 全局设置窗口（托盘「设置…」进的）：内容是和单盒设置共用的一套
/// SettingsDialog，但 URL 带 `g=1`，前端会隐藏「名称 / 置于顶层」两个单盒项。
fn spawn_gsettings(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    let win = WebviewWindowBuilder::new(
        app,
        "gsettings",
        WebviewUrl::App("index.html?mode=settings&g=1".into()),
    )
    .title("LightBox 设置")
    .inner_size(SET_W, SET_H)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .skip_taskbar(true)
    .always_on_top(true)
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), 0, 0, SET_W as i32, SET_H as i32, SET_RADIUS, false);
    Ok(win)
}

/// 更新提示窗口：独立小窗口，主屏正中弹出。
///
/// 和设置窗口一样启动时就建好（隐藏），第一次提醒直接显示，不用现建窗口等页面加载。
fn spawn_update(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    let win = WebviewWindowBuilder::new(
        app,
        "update",
        WebviewUrl::App("index.html?mode=update".into()),
    )
    .title("LightBox 更新")
    .inner_size(UPD_W, UPD_H)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .skip_taskbar(true)
    .always_on_top(true)
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), 0, 0, UPD_W as i32, UPD_H as i32, UPD_RADIUS, false);
    Ok(win)
}

/// 主屏工作区正中放一个 w×h（逻辑像素）的窗口，返回物理坐标
fn centered_origin(app: &AppHandle, w: f64, h: f64) -> (i32, i32) {
    let sc = app_scale(app);
    let area = app
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let wa = m.work_area();
            Rect {
                x: wa.position.x,
                y: wa.position.y,
                w: wa.size.width as i32,
                h: wa.size.height as i32,
            }
        })
        .unwrap_or(Rect { x: 0, y: 0, w: 1920, h: 1080 });
    let sw = (w * sc).round() as i32;
    let sh = (h * sc).round() as i32;
    (
        area.x + (area.w - sw).max(0) / 2,
        area.y + (area.h - sh).max(0) / 2,
    )
}

/// 打开（或重新聚焦）更新窗口，并把当前状态推给页面。
///
/// 页面是复用的：窗口只在启动时创建一次，所以状态一律走 `update:data` 事件推，
/// 页面自己也会主动拉一次（`update_state`），两条路都能拿到数据。
pub(crate) async fn open_update_window_inner(app: &AppHandle) -> Result<(), String> {
    let win = match app.get_webview_window("update") {
        Some(w) => w,
        None => spawn_update(app)?,
    };
    let (x, y) = centered_origin(app, UPD_W, UPD_H);
    win.set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), x, y, UPD_W as i32, UPD_H as i32, UPD_RADIUS, true);
    let _ = win.set_skip_taskbar(true);
    win.show().map_err(|e| e.to_string())?;
    let _ = win.set_focus();
    let _ = win.emit("update:data", updater::snapshot());
    Ok(())
}

/// 全局设置的摆放：主屏工作区正中（不锚定任何盒子 —— 它不属于某个盒子）。
fn gsettings_origin(app: &AppHandle, height: f64) -> (i32, i32) {
    let sc = app_scale(app);
    let area = app
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let wa = m.work_area();
            Rect {
                x: wa.position.x,
                y: wa.position.y,
                w: wa.size.width as i32,
                h: wa.size.height as i32,
            }
        })
        .unwrap_or(Rect { x: 0, y: 0, w: 1920, h: 1080 });
    let sw = (SET_W * sc).round() as i32;
    let sh = (height * sc).round() as i32;
    (
        area.x + (area.w - sw).max(0) / 2,
        area.y + (area.h - sh).max(0) / 2,
    )
}

/// 托盘「设置…」：打开全局设置窗口（所有收纳盒的公共项，不含单盒名称）。
async fn open_global_settings_inner(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<State>();
    let win = match app.get_webview_window("gsettings") {
        Some(w) => w,
        None => spawn_gsettings(app)?,
    };
    let h = *st.gsettings_height.lock().unwrap();
    let (x, y) = gsettings_origin(app, h);
    win.set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    win.show().map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), x, y, SET_W as i32, h as i32, SET_RADIUS, true);
    let _ = win.set_skip_taskbar(true);
    let _ = win.set_focus();
    // 设置窗口是复用的（页面只挂载一次），每次亮出来补推一次更新状态 ——
    // 否则后台检查到新版本后，这个页面还停在「没有更新」。
    let _ = app.emit("update:data", updater::snapshot());
    Ok(())
}

#[tauri::command]
async fn open_global_settings(app: AppHandle) -> Result<(), String> {
    open_global_settings_inner(&app).await
}

/// 「自动排序」也开一个命令口（前端/脚本以后可能用）；托盘直接调 inner。
#[tauri::command]
async fn auto_sort(app: AppHandle) -> Result<(), String> {
    auto_sort_boxes(&app).await
}

/// 系统托盘：图标 + 右键菜单（新建收纳盒 / 自动排序 / 设置… / 退出）。
///
/// 菜单事件跑在主线程，但建窗口、挪盒子都可能是重活 —— 统一
/// `async_runtime::spawn` 丢到 runtime 线程（和 async 命令同一条路，
/// 避免在主线程 IPC 回调里 build webview 死锁，见 create_box_inner 的说明）。
fn build_tray(app: &AppHandle) -> Result<(), String> {
    let mk = |id: &'static str, label: &'static str| {
        MenuItem::with_id(app, id, label, true, None::<&str>).map_err(|e| e.to_string())
    };
    let menu = Menu::with_items(
        app,
        &[
            &mk("tray_new", "新建收纳盒")?,
            &mk("tray_sort", "自动排序")?,
            &mk("tray_settings", "设置…")?,
            &mk("tray_update", "检查更新…")?,
            &PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?,
            &mk("tray_quit", "退出 LightBox")?,
        ],
    )
    .map_err(|e| e.to_string())?;
    // tauri 2.12 的 Image 只有 new/new_owned（吃原始 RGBA，没有 from_bytes），
    // 位图由 tools/make_tray_icon.py 生成（PNG 给人看，RGBA 给这里内嵌）
    let icon = Image::new_owned(include_bytes!("../icons/tray.rgba").to_vec(), 32, 32);
    let tray = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("LightBox 收纳盒")
        .menu(&menu)
        // 左键不弹菜单（那是右键的活）；左键暂时不绑动作
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let app = app.clone();
            // MenuId 是元组结构（.0 = String），2.12 没有 as_str()
            match event.id.0.as_str() {
                "tray_new" => {
                    tauri::async_runtime::spawn(async move { tray_new_box(&app).await });
                }
                "tray_sort" => {
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = auto_sort_boxes(&app).await {
                            eprintln!("自动排序失败: {e}");
                        }
                    });
                }
                "tray_settings" => {
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = open_global_settings_inner(&app).await {
                            eprintln!("打开全局设置失败: {e}");
                        }
                    });
                }
                "tray_update" => {
                    // 先把窗口亮出来（页面立刻显示「正在检查…」），再联网
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = open_update_window_inner(&app).await {
                            eprintln!("打开更新窗口失败: {e}");
                        }
                        let a = app.clone();
                        let _ = tauri::async_runtime::spawn_blocking(move || {
                            if let Err(e) = updater::check(&a, true) {
                                eprintln!("检查更新失败: {e}");
                            }
                        })
                        .await;
                    });
                }
                "tray_quit" => {
                    // 先删托盘图标再退出：否则 Explorer 会留一个幽灵图标，
                    // 要等鼠标晃过才回收（RunEvent::Exit 里还会再兜一次）
                    destroy_tray();
                    app.exit(0);
                }
                _ => {}
            }
        })
        .build(app)
        .map_err(|e| e.to_string())?;
    // 保活：TrayIcon 被 drop 图标就消失。放静态里（不是 forget）——
    // 退出时 destroy_tray() 能把它取出来 drop，Shell_NotifyIcon(NIM_DELETE)
    // 会把托盘图标立刻删掉，不会留下「退出了还在、鼠标晃一下才消失」的幽灵图标。
    *TRAY.lock().unwrap() = Some(tray);
    Ok(())
}

/// 右键菜单用独立窗口：盒子本体只有 96×96，还被 SetWindowRgn 裁成圆角，
/// 菜单画在盒子窗口里一定会被裁掉，所以菜单必须是自己的窗口。
/// 启动时就建好并隐藏，点击时只做「定位 + 显示」，避免每次右键都走一遍建窗流程。
fn spawn_menu(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    let win = WebviewWindowBuilder::new(app, "menu", WebviewUrl::App("index.html?mode=menu".into()))
        .title("LightBox")
        .inner_size(MENU_W, menu_height(false, false, false))
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), 0, 0, MENU_W as i32, menu_height(false, false, false) as i32, MENU_RADIUS, false);
    Ok(win)
}
/// 菜单贴着光标弹出，并保证整块留在光标所在那块屏幕里
fn place_menu(_app: &AppHandle, cx: i32, cy: i32, w: i32, h: i32) -> (i32, i32) {
    // 光标所在显示器的**工作区**（已排除任务栏）。直接问 Win32：
    // 混合缩放下 tauri 的 monitor 列表坐标会和实际对不上。
    let (ax, ay, aw, ah) = desktop::monitor_at(cx, cy)
        .map(|m| (m.work.x, m.work.y, m.work.w, m.work.h))
        .unwrap_or((0, 0, 1920, 1080));
    let mut x = cx + 2;
    let mut y = cy + 2;
    if x + w > ax + aw - 8 {
        x = (cx - w - 4).max(ax + 8);
    }
    if y + h > ay + ah - 8 {
        y = (cy - h - 4).max(ay + 8);
    }
    (x, y)
}

// ---------------- 开机自启（HKCU\...\Run，见 autostart.rs） ----------------

#[derive(Serialize, Clone)]
struct AppSettings {
    autostart: bool,
    exe: String,
    data_dir: String,
    version: String,
}

/// 面板当前应该展示哪个盒子 + 这一次显示的序号
#[derive(Serialize, Clone)]
struct PanelTarget {
    id: String,
    seq: u64,
}

/** 右键菜单要展示的数据 */
#[derive(Serialize, Clone)]
struct RenameTarget {
    id: String,
    name: String,
}

/// 盒子所在显示器的缩放系数（100% = 1.0，125% = 1.25）。
///
/// 为什么要发给页面：盒子是 Progman 的**子窗口**，它的窗口 DPI 永远是父窗口的（96），
/// 而 WebView 的 DPR 又是 tao 建窗口那一刻定下来的、之后不跟着换屏变 ——
/// 于是页面按「窗口 DPR」排版一定错位（125% 屏上内容溢出去被圆角区域裁掉）。
/// 页面拿到目标缩放后自己按 `scale / devicePixelRatio` 把 80×80 的舞台缩回来，
/// 物理像素就精确等于 `80 × scale`，和窗口尺寸、圆角区域三者对齐。
#[derive(Serialize, Clone)]
struct BoxScale {
    id: String,
    scale: f64,
}

#[derive(Serialize, Clone)]
struct MenuData {
    id: String,
    name: String,
    count: usize,
    topmost: bool,
    /// 是否已嵌入任务栏（菜单里显示「嵌入任务栏 / 移出任务栏」）
    in_bar: bool,
    /// 这个盒子是否设置了自定义图标（决定菜单里要不要出「恢复默认图标」）
    has_icon: bool,
    /// 已经在某个「胶囊团」里（菜单显示「解散胶囊团」）
    grouped: bool,
    /// 旁边有吸附在一起的邻居，可以组合（菜单显示「组合为胶囊团」）
    can_group: bool,
}

// ---------------- commands ----------------

/// 盒子内容变了：盒子窗口（数量角标）和面板（如果正显示这个盒子）都要刷新。
/// 之前只在「拖进来的那个 webview」里手动 refresh，另一个窗口就一直显示旧数据。
fn notify_box_changed(app: &AppHandle, id: &str) {
    let _ = app.emit("box:changed", id.to_string());
}

/// 上一次安排桌面刷新的时间戳（毫秒）：把同一批拖放的多次刷新合并成一次，
/// 避免连续拖入 5 个图标就对着桌面连按 5 次 F5。
static LAST_REFRESH_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 拖放结束后补一次「强制刷新」：落手 1 秒后让桌面的 DefView 走一遍 F5，
/// 并把盒子目录的图标缓存清掉，再广播一次让盒子角标 / 已打开的面板重画。
///
/// 背景：我们已经逐条发过 `SHCNE_DELETE`（通常 100ms 内桌面图标就消失），
/// 但 Explorer 有概率不处理这条通知（图标缓存、OneDrive/网络路径、落点在 Progman
/// 而不是 DefView 等情况），图标就一直赖在桌面上，用户只能自己按 F5。
/// 一次刷新很便宜（一次跨进程消息 + 一次目录广播），所以宁可多刷一次。
fn schedule_desktop_refresh(app: AppHandle, id: String, delay_ms: u64) {
    use std::sync::atomic::Ordering;
    let now = now_ms();
    let last = LAST_REFRESH_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 800 {
        return; // 800ms 内的连续拖放合并成一次（这一次刷新会覆盖到它们）
    }
    LAST_REFRESH_MS.store(now, Ordering::Relaxed);

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        desktop::refresh_desktop();
        icons::invalidate_dir(&store::box_dir(&id));
        notify_box_changed(&app, &id);

        // 再补一轮轻量广播：Explorer 处理 F5 有自己的节奏，个别机器上
        // 图标要等第二次目录变更通知才真的从桌面上抹掉。
        std::thread::sleep(std::time::Duration::from_millis(900));
        desktop::notify_desktop_changed();
        notify_box_changed(&app, &id);
    });
}

#[tauri::command]
fn get_config(state: tauri::State<State>) -> Config {
    state.cfg.lock().unwrap().clone()
}

/// 抽图标是纯 CPU + shell 调用，放到 async（runtime 线程）执行，
/// 免得一批图标把主线程按住，窗口看起来像卡了一拍。
#[tauri::command]
async fn get_box_view(id: String, state: tauri::State<'_, State>) -> Result<BoxView, String> {
    let (name, topmost, in_bar, icon, bx, by) = {
        let cfg = state.cfg.lock().unwrap();
        let b = cfg.boxes.iter().find(|b| b.id == id).ok_or("盒子不存在")?;
        (
            b.name.clone(),
            is_floating(b),
            b.in_bar,
            b.icon.clone(),
            b.x,
            b.y,
        )
    };
    // 舞台设计尺寸（逻辑像素）：任务栏盒子 = 任务栏尺寸 ÷ 该屏缩放，普通盒子 = 80×80
    let (w, h) = if in_bar {
        let sc = desktop::scale_at(bx.max(0) + 1, by + 1).max(0.5);
        let (bw, bh) = bar_box_size();
        ((bw as f64 / sc).round(), (bh as f64 / sc).round())
    } else {
        (BOX_W, BOX_W)
    };
    let (order, in_group) = {
        let cfg = state.cfg.lock().unwrap();
        match cfg.boxes.iter().find(|b| b.id == id) {
            Some(b) => (b.order.clone(), b.group.is_some()),
            None => (Vec::new(), false),
        }
    };
    let items = store::list_items(&id, &order);
    Ok(BoxView {
        id,
        name,
        count: items.len(),
        topmost,
        in_bar,
        w,
        h,
        bar_hide_label: bar_hide_label(),
        icon: icon,
        in_group,
    })
}

#[tauri::command]
async fn list_items(id: String, state: tauri::State<'_, State>) -> Result<Vec<Item>, String> {
    let order = {
        let cfg = state.cfg.lock().unwrap();
        cfg.boxes
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.order.clone())
            .unwrap_or_default()
    };
    Ok(store::list_items(&id, &order))
}

/// 保存面板里拖动后的图标顺序（文件名列表）。
/// 只认「当前确实在盒子里」的文件，顺手把已删除的旧记录清掉。
#[tauri::command]
fn reorder_items(state: tauri::State<State>, id: String, files: Vec<String>) -> Result<(), String> {
    let existing: Vec<String> = store::list_items(&id, &[])
        .into_iter()
        .map(|i| i.file)
        .collect();
    let mut cfg = state.cfg.lock().unwrap();
    let b = cfg.boxes.iter_mut().find(|b| b.id == id).ok_or("盒子不存在")?;
    let mut order: Vec<String> = files.into_iter().filter(|f| existing.contains(f)).collect();
    // 漏掉的文件（理论上不会有）补在后面，避免下次读出来乱序
    for f in existing {
        if !order.contains(&f) {
            order.push(f);
        }
    }
    b.order = order;
    store::save(&cfg);
    Ok(())
}

#[derive(Serialize)]
struct ImportReport {
    added: Vec<Item>,
    skipped: Vec<String>,
}

#[tauri::command]
async fn import_paths(app: AppHandle, id: String, paths: Vec<String>) -> ImportReport {
    let (added, skipped) = store::import(&id, &paths);
    if !added.is_empty() {
        notify_box_changed(&app, &id);
        // 拖入后 1 秒强制刷新桌面（详见 schedule_desktop_refresh 的注释）
        schedule_desktop_refresh(app.clone(), id.clone(), 1000);
    }
    ImportReport { added, skipped }
}

/// 打开盒子里的一个快捷方式。
///
/// 用 shortcut::open 而不是直接把 .lnk 丢给 ShellExecute：
/// 它会先把快捷方式解析成「目标 + 参数 + 工作目录」，显式指定工作目录再启动。
/// 依赖 cwd 的软件（Electron 系最常见，Motrix / MQTTX 就是）从别的目录拉起时
/// 会在初始化阶段直接退出，表现就是「点了没反应」——这一条就是治它的。
#[tauri::command]
async fn open_item(path: String) -> Result<(), String> {
    shortcut::open(&path)
}

/// 在资源管理器里定位这个图标并选中（面板右键「打开文件所在位置」）。
///
/// `.lnk` 跳它指向的程序所在目录，目标已卸载就退回盒子目录。
/// 解析 PIDL / 打开文件夹都会碰磁盘，扔进阻塞线程池，别占 runtime 的异步线程。
#[tauri::command]
async fn reveal_item(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || shortcut::reveal(&path))
        .await
        .map_err(|e| e.to_string())?
}

/// 弹出系统的「属性」对话框（面板右键「属性」）。
///
/// 属性框是模态的、内部会阻塞到用户关掉，所以 shortcut::properties 里已经
/// 丢到独立线程，这个命令立刻就能返回，不会冻住界面。
#[tauri::command]
async fn item_properties(app: AppHandle, path: String) -> Result<(), String> {
    // owner 用面板窗口：不传 owner 的话属性框可能被盒子窗口压在后面
    let owner = app
        .get_webview_window("panel")
        .map(|w| hwnd_of(&w))
        .unwrap_or(0);
    shortcut::properties(&path, owner)
}

#[tauri::command]
async fn restore_item(app: AppHandle, id: String, file: String) -> Result<(), String> {
    let r = store::restore_to_desktop(&id, &file);
    if r.is_ok() {
        notify_box_changed(&app, &id);
        // 反过来：图标回到桌面后也可能不立刻出现，同样补一次刷新
        schedule_desktop_refresh(app.clone(), id.clone(), 900);
    }
    r
}

#[tauri::command]
async fn remove_item(app: AppHandle, id: String, file: String) -> Result<(), String> {
    let r = store::remove_from_box(&id, &file);
    if r.is_ok() {
        notify_box_changed(&app, &id);
    }
    r
}

/// 开始把盒子里的快捷方式拖到桌面（由前端在拖动超过阈值时调用）。
///
/// `files` 支持多选：框选 / Ctrl 多选后拖动其中一个，选中的图标会作为**一批**
/// 一起拖出去（CF_HDROP 多文件，资源管理器按移动处理）。
/// 返回 "moved" / "copied" / "cancel"，前端据此刷新盒子内容。
#[tauri::command]
fn drag_item_out(app: AppHandle, id: String, files: Vec<String>) -> Result<String, String> {
    let mut srcs: Vec<std::path::PathBuf> = Vec::with_capacity(files.len());
    for f in &files {
        let src = store::box_dir(&id).join(f);
        if !src.exists() {
            return Err("文件不存在".into());
        }
        srcs.push(src);
    }
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    // DoDragDrop 必须跑在拥有消息循环的主线程：它会接管鼠标捕获并自己泵消息
    app.run_on_main_thread(move || {
        let effect = dragsrc::do_drag(&srcs);
        let _ = tx.send(effect);
    })
    .map_err(|e| e.to_string())?;

    let effect = rx
        .recv_timeout(std::time::Duration::from_secs(180))
        .unwrap_or(0);

    // 不管落点是「移动」还是「复制」，源盒子目录都可能已经变了（落到别的盒子时，
    // 是对方的 import 把文件 rename 走的），统一让盒子窗口重新读一次目录。
    notify_box_changed(&app, &id);

    if effect & win32::DROPEFFECT_MOVE != 0 || effect & win32::DROPEFFECT_COPY != 0 {
        // 落到桌面上时，桌面那边同样可能刷不出来，补一次延迟刷新
        schedule_desktop_refresh(app.clone(), id.clone(), 900);
    }

    if effect & win32::DROPEFFECT_MOVE != 0 {
        // 资源管理器是「搬走」源文件的：它自己完成移动，这里不再删源文件。
        // 有意不做兜底删除——万一落点就是本盒子自己，删除等于直接丢数据。
        Ok("moved".into())
    } else if effect & (win32::DROPEFFECT_COPY | win32::DROPEFFECT_LINK) != 0 {
        Ok("copied".into())
    } else {
        Ok("cancel".into())
    }
}

/// 批量「放回桌面」（面板多选后一次操作）。返回成功个数；
/// 桌面上有同名文件时自动加后缀（见 store::unique_target），不会互相覆盖。
#[tauri::command]
async fn restore_items(app: AppHandle, id: String, files: Vec<String>) -> Result<u32, String> {
    let mut ok = 0u32;
    let mut first_err: Option<String> = None;
    for f in &files {
        match store::restore_to_desktop(&id, f) {
            Ok(()) => ok += 1,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if ok > 0 {
        notify_box_changed(&app, &id);
        // 图标回到桌面后可能不立刻出现，补一次刷新
        schedule_desktop_refresh(app.clone(), id.clone(), 900);
    }
    if ok == 0 {
        if let Some(e) = first_err {
            return Err(e);
        }
    }
    Ok(ok)
}

/// 批量「从盒子移除」（直接删除文件，不放回桌面）。返回成功个数。
#[tauri::command]
async fn remove_items(app: AppHandle, id: String, files: Vec<String>) -> Result<u32, String> {
    let mut ok = 0u32;
    let mut first_err: Option<String> = None;
    for f in &files {
        match store::remove_from_box(&id, f) {
            Ok(()) => ok += 1,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if ok > 0 {
        notify_box_changed(&app, &id);
    }
    if ok == 0 {
        if let Some(e) = first_err {
            return Err(e);
        }
    }
    Ok(ok)
}

/// 「全部图标移回桌面」：把当前盒子里的图标一次性放回桌面（盒子清空后可删除）。
/// 盒子右键菜单、面板「⋯」菜单共用。
#[tauri::command]
async fn restore_all_items(app: AppHandle, id: String) -> Result<u32, String> {
    let files: Vec<String> = store::list_items(&id, &[])
        .into_iter()
        .map(|i| i.file)
        .collect();
    restore_items(app, id, files).await
}

/// 新建收纳盒的核心逻辑（命令、托盘菜单都走这里）。
///
/// 落点统一走 `spot_next_to_rightmost`：排在屏幕最右侧的盒子旁边（右侧优先，
/// 没位置走下 → 左 → 底部居中兜底），按工作区裁剪不出屏、不重叠。
/// 三个入口（盒子右键菜单、面板「＋」、托盘「新建收纳盒」）行为完全一致。
///
/// **必须 async**：同步命令是在主线程的 WebView2 IPC 回调里执行的，在里面 `build()` 新
/// webview 会死锁（wry #583：WebView2 创建要在嵌套消息循环里等自己，等不到）。
/// 之前「一点新建收纳盒就卡死」就是这个原因。改成 async 后命令跑在 runtime 线程，
/// 建窗请求会被投递回主线程事件循环执行，不会互相等待。
async fn create_box_inner(app: &AppHandle, name: String) -> Result<BoxConfig, String> {
    let st = app.state::<State>();
    let b = {
        let mut cfg = st.cfg.lock().unwrap();
        // 持锁调用安全：spot_next_to_rightmost 只读 monitor API，不碰 State
        let (x, y) = spot_next_to_rightmost(app, &cfg.boxes);
        let b = BoxConfig {
            id: new_id(),
            name: if name.trim().is_empty() { "新收纳盒".into() } else { name.trim().into() },
            x,
            y,
            topmost: false,
            in_bar: false,
            icon: None,
            order: Vec::new(),
            group: None,
        };
        cfg.boxes.push(b.clone());
        store::save(&cfg);
        b
    };
    store::box_dir(&b.id);
    spawn_box_window(app, &b, true)?;
    // 新窗口被 show 时会抢焦点，面板本来开着就会被「失焦自动收起」误收。
    // 这里把焦点还回面板（前端配合一个 grace 窗口，忽略这次失焦）。
    if let Some(p) = app.get_webview_window("panel") {
        if p.is_visible().unwrap_or(false) {
            let _ = p.set_focus();
        }
    }
    Ok(b)
}

#[tauri::command]
async fn create_box(app: AppHandle, name: String) -> Result<BoxConfig, String> {
    create_box_inner(&app, name).await
}

/// 「新建收纳盒」的落点（托盘 / 盒子右键菜单 / 面板「＋」共用）：排在**屏幕最右侧
/// 那个盒子的旁边**，且不出屏幕。
///
/// 优先级：右侧 → 下方 → 左侧 → 底部居中（最后一道兜底几乎用不上）。
/// 每一步都按所在显示器的工作区裁剪（任务栏已排除），保证不越界、不重叠。
fn spot_next_to_rightmost(app: &AppHandle, boxes: &[BoxConfig]) -> (i32, i32) {
    let scale = app_scale(app);
    let size = (BOX_W * scale).round() as i32;
    let gap = (24.0 * scale).round() as i32;
    // 只参考桌面上的普通盒子：任务栏里的盒子不占桌面空间，不能拿来当参照物
    let normals: Vec<&BoxConfig> = boxes.iter().filter(|b| !b.in_bar).collect();
    if normals.is_empty() {
        // 一个普通盒子都没有：按首启规则放（底部居中）
        return default_row(app, 1).into_iter().next().unwrap_or((0, 0));
    }
    // 最右的盒子：按右边缘取最大（盒子同尺寸，等价于 x 最大）
    let r = normals.iter().max_by_key(|b| b.x).unwrap();
    let area = work_area_at(app, r.x + size / 2, r.y + size / 2);
    let inside = |x: i32, y: i32| {
        x >= area.x && y >= area.y && x + size <= area.x + area.w && y + size <= area.y + area.h
    };
    let overlaps = |x: i32, y: i32| {
        boxes.iter().any(|b| {
            (b.x - x).abs() < size && (b.y - y).abs() < size
        })
    };
    let clamp = |x: i32, y: i32| {
        (
            x.max(area.x).min((area.x + area.w - size).max(area.x)),
            y.max(area.y).min((area.y + area.h - size).max(area.y)),
        )
    };
    // ① 右侧紧挨着（同一行）
    let (rx, ry) = (r.x + size + gap, r.y);
    if inside(rx, ry) && !overlaps(rx, ry) {
        return (rx, ry);
    }
    // ② 正下方（右侧没位置时往下接）
    let (bx, by) = clamp(r.x, r.y + size + gap);
    if inside(bx, by) && !overlaps(bx, by) {
        return (bx, by);
    }
    // ③ 左侧（下方也不行时往左接，一般发生在右下角）
    let (lx, ly) = clamp(r.x - size - gap, r.y);
    if inside(lx, ly) && !overlaps(lx, ly) {
        return (lx, ly);
    }
    // ④ 兜底：底部居中找一个不重叠的空位
    default_row(app, 1)
        .into_iter()
        .find(|(x, y)| !overlaps(*x, *y))
        .unwrap_or_else(|| clamp(r.x, r.y))
}

/// 托盘「新建收纳盒」：建一个盒子，排在屏幕最右的盒子旁边。
async fn tray_new_box(app: &AppHandle) {
    if let Err(e) = create_box_inner(app, "新收纳盒".into()).await {
        eprintln!("托盘新建收纳盒失败: {e}");
    }
}

/// 「自动排序」：把所有收纳盒放回初始默认位置 —— 屏幕下边缘中间（= 任务栏上沿）、
/// 水平依次排列、互不重叠、不被任务栏遮盖（工作区已排除任务栏）。按配置里的顺序从左到右排。
///
/// Win11 的贴条也一起收回来（见 `desktop::win11`）。那儿「嵌入任务栏」是**置顶顶层窗口**，
/// 用户点「自动排序」想要的显然是「全部归位」，留几个贴条杵在任务栏里反而像没排干净；
/// 而且真卡住的时候这也是最直观的救援动作。Win10 的胶囊是任务栏真子窗口，
/// 收拾它要重建窗口、代价大且行为本来正确 —— 保持「不参与排序」。
async fn auto_sort_boxes(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<State>();
    // 先把「配置里有、窗口已经没了」的盒子建回来 —— 否则自动排序只是把配置里的
    // 坐标改一遍，用户点完还是看不到任何盒子（Explorer 重启后就是这个症状）。
    rebuild_missing_boxes(app);

    // 自动排序会把所有盒子重新摆一遍，团内那套「固定偏移」随之失效 ——
    // 先把团全部解散（盒子位置不动，只是不再联动拖动），用户想接着整团拖就再组合一次。
    {
        let mut cfg = st.cfg.lock().unwrap();
        if cfg.boxes.iter().any(|b| b.group.is_some()) {
            for b in cfg.boxes.iter_mut() {
                b.group = None;
            }
            store::save(&cfg);
        }
    }

    // Win11：把任务栏里的贴条全部退回桌面。只改配置，窗口交给下面的重建处理
    // （顶层窗口 → 桌面层子窗口没法就地换，见 rebuild_box_flashless）。
    let pulled: Vec<String> = if desktop::win11() {
        let mut cfg = st.cfg.lock().unwrap();
        let ids: Vec<String> = cfg
            .boxes
            .iter_mut()
            .filter(|b| b.in_bar)
            .map(|b| {
                b.in_bar = false;
                b.topmost = false;
                b.id.clone()
            })
            .collect();
        if !ids.is_empty() {
            store::save(&cfg);
        }
        ids
    } else {
        Vec::new()
    };

    // 只排「普通盒子」；Win10 里还嵌在任务栏里的盒子有自己的位置规则（贴任务栏上沿），不参与排序
    let (spots, ids) = {
        let cfg = st.cfg.lock().unwrap();
        let normals: Vec<&BoxConfig> = cfg.boxes.iter().filter(|b| !b.in_bar).collect();
        if normals.is_empty() {
            return Ok(());
        }
        (
            default_row(app, normals.len()),
            normals.iter().map(|b| b.id.clone()).collect::<Vec<String>>(),
        )
    };
    {
        let mut cfg = st.cfg.lock().unwrap();
        for (id, (x, y)) in ids.iter().zip(spots.iter()) {
            if let Some(b) = cfg.boxes.iter_mut().find(|b| &b.id == id) {
                b.x = *x;
                b.y = *y;
            }
        }
        store::save(&cfg);
    }

    // 刚从任务栏收回来的那几个：坐标已经写好了，重建窗口即落到位上（丢后台，别卡 UI）
    if !pulled.is_empty() {
        let list: Vec<BoxConfig> = {
            let cfg = st.cfg.lock().unwrap();
            cfg.boxes
                .iter()
                .filter(|b| pulled.iter().any(|id| id == &b.id))
                .cloned()
                .collect()
        };
        let app2 = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let Some(_guard) = lock_relay(8000) else { return };
            for b in list {
                // 重建期间挂个牌子：这一刻的窗口移动不是用户拖动
                let _sg = switching_begin(&b.id);
                if let Err(e) = rebuild_box_flashless(&app2, &b) {
                    eprintln!("自动排序时把盒子移出任务栏失败（{} / {}）：{e}", b.name, b.id);
                }
                notify_box_changed(&app2, &b.id);
            }
        });
    }

    // 窗口跟着走：位置全变，统一 strip_chrome 一遍（和 show_box 同一条路，
    // 不碰 tao 的 show()，不会把标题栏带回来）
    let cfg = st.cfg.lock().unwrap().clone();
    for b in cfg
        .boxes
        .iter()
        .filter(|b| !b.in_bar && !pulled.iter().any(|id| id == &b.id))
    {
        if let Some(w) = box_window(app, &b.id) {
            let hwnd = hwnd_of(&w);
            desktop::strip_chrome(hwnd, b.x, b.y, BOX_W as i32, BOX_W as i32, BOX_RADIUS, true);
            let _ = w.set_skip_taskbar(true);
            reinforce_region(hwnd);
            // 排到别的屏上（缩放不同）时，页面要按新屏的缩放重排
            let _ = app.emit(
                "box:scale",
                BoxScale {
                    id: b.id.clone(),
                    scale: desktop::scale_at(b.x + BOX_W as i32 / 2, b.y + BOX_W as i32 / 2),
                },
            );
        }
    }
    Ok(())
}

#[tauri::command]
fn rename_box(app: AppHandle, state: tauri::State<State>, id: String, name: String) -> Result<(), String> {
    let mut cfg = state.cfg.lock().unwrap();
    let b = cfg.boxes.iter_mut().find(|b| b.id == id).ok_or("盒子不存在")?;
    b.name = name.trim().to_string();
    if b.name.is_empty() {
        b.name = "收纳盒".into();
    }
    store::save(&cfg);
    drop(cfg);
    notify_box_changed(&app, &id);
    Ok(())
}

/// 让盒子在桌面上直接进入改名状态（右键菜单「重命名」调它）。
///
/// 盒子是 Progman 的子窗口，平时**永远不是前台**（用户点的是桌面/别的程序），
/// 所以敲键盘根本到不了它 —— 这里要把前台和焦点都抢过来：
/// `SetForegroundWindow` 让本进程的输入队列成为「收到键盘的那个」，
/// `set_focus()` 再把这个 child hwnd 设成焦点窗口。两条都失败也不致命，
/// 前端会退回「弹窗里改名」的兜底（见 BoxView）。
#[tauri::command]
async fn begin_box_rename(
    app: AppHandle,
    state: tauri::State<'_, State>,
    id: String,
) -> Result<(), String> {
    let name = {
        let cfg = state.cfg.lock().unwrap();
        cfg.boxes.iter().find(|b| b.id == id).map(|b| b.name.clone()).ok_or("盒子不存在")?
    };
    let bw = box_window(&app, &id).ok_or("盒子不存在")?;
    // 盒子的矩形一律用 Win32 取（tao 的 outer_position 是父客户区坐标）
    let box_rect = rect_of(&bw).ok_or("读不到盒子位置")?;
    let (pos, sz) = (box_rect, box_rect);

    // 复用「右键菜单」那个窗口当输入框：它是顶层 always_on_top 窗口，能拿到键盘焦点。
    // 盒子本身是 Progman 的 child 窗口，永远不是前台，在里面放 input 是打不进字的
    // （实测：输入框出来了，敲键盘全被桌面吃掉）。
    let win = app.get_webview_window("menu").ok_or("菜单窗口不存在")?;
    // 输入条盖在盒子上 → 按盒子所在显示器换算尺寸
    let sc = desktop::scale_at(pos.x + sz.w / 2, pos.y + sz.h / 2);

    // 盖在盒子名字那一行上。布局是 styles.css 里的 `.box-tile`：上下居中，
    // 内容总高 = 3 + 46 + 2 + 14.6 ≈ 65.6 → 80 高时顶部余量 (80-65.6)/2 ≈ 7.2，
    // 名字行从 y≈58 开始。输入框高 22，取垂直中心对齐名字行 → y≈54.5。
    let (w, h) = (BOX_W, 22.0);
    let pw = (w * sc).round() as i32;
    let ph = (h * sc).round() as i32;
    let x = pos.x + (sz.w - pw) / 2;
    let y = pos.y + (54.5 * sc).round() as i32;

    win.set_size(PhysicalSize::new(pw as u32, ph as u32))
        .map_err(|e| e.to_string())?;
    win.show().map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), x, y, w as i32, h as i32, 8, true);
    let _ = win.set_skip_taskbar(true);
    // 先让页面切到「输入框形态」，再抢焦点，否则焦点会落在菜单的旧内容上
    let _ = win.emit("menu:rename", RenameTarget { id, name });
    let _ = win.set_focus();
    Ok(())
}

/// 删除收纳盒。**非空一律拒绝**：删盒子不能顺手把用户的快捷方式弄丢，
/// 也不做「自动放回桌面」这种自作主张的动作，交给用户自己先移出去。
#[tauri::command]
async fn delete_box(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<(), String> {
    let count = store::count_items(&id);
    if count > 0 {
        return Err(format!("收纳盒里还有 {count} 个快捷方式，请先把它们移出（放回桌面）再删除"));
    }
    {
        let mut cfg = state.cfg.lock().unwrap();
        cfg.boxes.retain(|b| b.id != id);
        // 删掉的盒子可能在一个团里：摘出去，别让剩下的人拖着一个不存在的成员
        drop_from_group(&mut cfg, &id);
        store::save(&cfg);
    }
    if let Some(w) = box_window(&app, &id) {
        desktop::forget(hwnd_of(&w));
        let _ = w.close();
    }
    // 面板正在显示这个盒子就一起收起来
    let showing = state.panel_target.lock().unwrap().clone() == id;
    if showing {
        if let Some(p) = app.get_webview_window("panel") {
            let _ = p.hide();
        }
        *state.panel_target.lock().unwrap() = String::new();
    }
    let dir = store::box_dir(&id);
    let _ = std::fs::remove_dir(&dir).or_else(|_| std::fs::remove_dir_all(&dir));
    Ok(())
}

/// 正在做「嵌入 / 移出任务栏」形态切换的盒子 id。
///
/// 切换 = 「改配置 + 重建窗口」，前后几百毫秒。这段时间里前端还会因为窗口被移动
/// 而调 `snap_box` / `snap_bar` / `move_box`，它们读到的是**一半新一半旧**的状态。
/// 最典型的事故：刚把胶囊移出任务栏（配置已是普通盒子、坐标已在工作区），紧接着
/// 一个迟到的 `snap_bar` 又按任务栏的口径把 y 写回任务栏上沿 —— 重建出来的窗口
/// 就卡在任务栏里只露出一条上边缘：任务栏里没有、桌面上也看不见、自动排序
/// （只处理非 in_bar 盒子，此时 b.in_bar 已是 false 但坐标被写坏）也救不回来。
///
/// 所以切换期间给这个 id 挂个牌子，snap / move 见到就直接跳过。
static SWITCHING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 牌子在离开作用域时自动摘掉（early return 也不会漏）。
struct SwitchGuard(String);

impl Drop for SwitchGuard {
    fn drop(&mut self) {
        SWITCHING.lock().unwrap().retain(|x| x != &self.0);
    }
}

/// 标记「这个盒子正在切换形态」。**不要嵌套**：内层 guard 先 drop 会把牌子摘掉。
fn switching_begin(id: &str) -> SwitchGuard {
    {
        let mut v = SWITCHING.lock().unwrap();
        if !v.iter().any(|x| x == id) {
            v.push(id.to_string());
        }
    }
    SwitchGuard(id.to_string())
}

fn switching(id: &str) -> bool {
    SWITCHING.lock().unwrap().iter().any(|x| x == id)
}

/// 兜底落盘：把盒子**当前真实位置**（屏幕坐标）写进配置。
///
/// 以前是前端把 `onMoved` 的 payload 原样传进来 —— 那个值是 **WM_MOVE 的 lParam**，
/// 也就是父客户区坐标，于是配置里存进了 2736 这种数，下次启动再按父客户区换算一次，
/// 盒子就多偏一块屏。现在前端只传 id，位置由后端用 Win32 取，坐标口径只有一套。
///
/// 鼠标还按着时什么都不做：拖动途中的坐标不是最终位置，交给 `snap_box` 松手后处理。
#[tauri::command]
fn move_box(app: AppHandle, state: tauri::State<State>, id: String) {
    // 正在切换形态（嵌入/移出任务栏）：窗口还停在旧位置，读到的坐标没有意义
    if switching(&id) {
        return;
    }
    if (unsafe { win32::GetAsyncKeyState(0x01) } as u16) & 0x8000 != 0 {
        return;
    }
    let r = match box_window(&app, &id).and_then(|w| rect_of(&w)) {
        Some(r) => r,
        None => return,
    };
    let mut cfg = state.cfg.lock().unwrap();
    if let Some(b) = cfg.boxes.iter_mut().find(|b| b.id == id) {
        // 普通盒子却停在任务栏区域：说明窗口是「刚移出任务栏、还没重建完」的旧窗口，
        // 这个 y 是任务栏坐标，写进配置会把盒子下次启动时送进任务栏里（看不见）。
        if !b.in_bar && on_taskbar(r.x, r.y, r.h.max(1)) {
            return;
        }
        // 任务栏盒子只认横坐标：纵坐标恒等于任务栏上沿，拖动途中的 y 不落盘
        let ny = if b.in_bar { b.y } else { r.y };
        if b.x != r.x || b.y != ny {
            b.x = r.x;
            b.y = ny;
            store::save(&cfg);
        }
    }
}

/// 拖动松手后的「吸附」：先吸别的盒子（并保持同排/同列对齐），没吸上再吸屏幕工作区边缘。
///
/// 为什么放后端：位置、尺寸、工作区、DPI 换算必须是同一套物理像素。前端算的话，
/// 125% 缩放下贴合处会差一两个像素、露出缝。
///
/// 返回值有三种：
///  - `None`     = 鼠标左键还按着（系统拖动没结束），**现在不能动窗口** ——
///    会和拖动循环抢位置，表现为抖动甚至拖不动。前端过 150ms 再来问。
///  - `Some(true)`  = 处理完，而且真的移动了（前端要重截背后的桌面）
///  - `Some(false)` = 处理完，但位置没变（已经吸好了 / 够不着不用吸）
///
/// 拖动期间收不到 pointerup，所以只能问系统 `GetAsyncKeyState(VK_LBUTTON)`。
#[tauri::command]
async fn snap_box(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<Option<bool>, String> {
    // 形态正在切换（嵌入 / 移出任务栏，窗口正在重建）：这一刻的坐标是旧的，别动
    if switching(&id) {
        return Ok(Some(false));
    }
    if (unsafe { win32::GetAsyncKeyState(0x01) } as u16) & 0x8000 != 0 {
        return Ok(None);
    }
    let bw = box_window(&app, &id).ok_or("盒子窗口不存在")?;
    // 位置/尺寸一律取 Win32 的屏幕坐标：tao 给的是父客户区坐标（见 rect_of）
    let br = rect_of(&bw).ok_or("读不到盒子位置")?;
    let (w, h) = (br.w, br.h);
    let (ox, oy) = (br.x, br.y);
    // 吸附阈值按**盒子所在显示器**的缩放换算（不能用窗口 DPI：盒子是 Progman 的子窗口，
    // 它的 DPI 永远跟着父窗口走）
    let sc = desktop::scale_at(ox + w / 2, oy + h / 2);
    let area = work_area_at(&app, ox + w / 2, oy + h / 2);

    let dist = (SNAP_DIST * sc).round() as i32; // 触发吸附的最大间距
    let gap = (SNAP_GAP * sc).round() as i32; // 吸附后留的空隙
    let edge = (SNAP_EDGE * sc).round() as i32; // 触发贴边的距离

    // 下边界可以贴到哪：默认贴任务栏上沿（工作区下边界）；盒子的下边界已经越进任务栏
    // 超过一半时，认定用户是想把盒子**压在任务栏上**，改贴整屏下沿（任务栏下沿）。
    // 任务栏不在底部（顶部/侧边/自动隐藏）时 bar = 0，这条规则自动失效。
    let bar = desktop::bottom_bar_at(ox + w / 2, oy + h / 2);
    let over_bar = bar > 0 && (oy + h) - area.bottom() > bar / 2;
    let bot_limit = if over_bar { area.bottom() + bar } else { area.bottom() };

    let others: Vec<Rect> = {
        let cfg = state.cfg.lock().unwrap();
        // 同一个团里的成员不参与吸附：它们跟着锚点整体走（偏移固定），
        // 如果还拿它们当吸附目标，吸一下就会把整团拉变形。
        let gid = cfg
            .boxes
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| b.group.clone());
        cfg.boxes
            .iter()
            .filter(|b| b.id != id && !(gid.is_some() && b.group == gid))
            .map(|b| Rect { x: b.x, y: b.y, w, h })
            .collect()
    };

    let (mut nx, mut ny) = (ox, oy);
    // 排序键：(优先级, 位移代价)。优先级 0 = 贴自己左边/上边那个邻居，1 = 贴右边/下边。
    //
    // 为什么需要优先级：左右两个邻居的间隙同时落在 `dist` 内时，只比「谁更近」的话
    // 两个候选往往只差一两像素；吸完会触发一次重排，这一回又选中另一边 —— 表现就是
    // 盒子在两个邻居之间来回跳（用户实测反馈）。固定优先左边（纵向优先上边），结果就稳定。
    let mut best: Option<(u8, i32)> = None;

    for o in &others {
        // ① 横向并排：两盒在竖直方向大致对齐（中心差不到半个盒子高）
        if (oy - o.y).abs() <= h / 2 {
            for (cand_x, cur_gap, prio) in [
                // 落到对方右边 = 贴在「我左边那个盒子」的右侧 → 优先
                (o.x + o.w + gap, ox - (o.x + o.w), 0u8),
                // 落到对方左边 = 贴在「我右边那个盒子」的左侧
                (o.x - gap - w, o.x - (ox + w), 1u8),
            ] {
                if cur_gap.abs() <= dist {
                    // 吸附时 y 对齐到对方 —— 这就是「水平对齐」
                    let cost = (cand_x - ox).abs() + (o.y - oy).abs();
                    if best.map_or(true, |(bp, bc)| prio < bp || (prio == bp && cost < bc)) {
                        best = Some((prio, cost));
                        nx = cand_x;
                        ny = o.y;
                    }
                }
            }
        }
        // ② 纵向堆叠：两盒在水平方向大致对齐（中心差不到半个盒子宽），同一套优先级
        if (ox - o.x).abs() <= w / 2 {
            for (cand_y, cur_gap, prio) in [
                // 落到对方下边 = 贴在「我上边那个盒子」的下侧 → 优先
                (o.y + o.h + gap, oy - (o.y + o.h), 0u8),
                // 落到对方上边 = 贴在「我下边那个盒子」的上侧
                (o.y - gap - h, o.y - (oy + h), 1u8),
            ] {
                if cur_gap.abs() <= dist {
                    let cost = (cand_y - oy).abs() + (o.x - ox).abs();
                    if best.map_or(true, |(bp, bc)| prio < bp || (prio == bp && cost < bc)) {
                        best = Some((prio, cost));
                        nx = o.x;
                        ny = cand_y;
                    }
                }
            }
        }
    }

    // 吸附到别的盒子以后如果会被顶出工作区（比如对方本身就贴着屏幕右边），
    // 这一次贴合就不算数 —— 退回「吸屏幕边缘」，总比压到对方身上好。
    let out_of_area = nx < area.x || ny < area.y || nx + w > area.right() || ny + h > bot_limit;
    if best.is_none() || out_of_area {
        nx = ox;
        ny = oy;
        // 四条边各自独立判断，可以同时贴两条边（角落）
        if ox - area.x <= edge {
            nx = area.x;
        } else if area.right() - (ox + w) <= edge {
            nx = area.right() - w;
        }
        if oy - area.y <= edge {
            ny = area.y;
        } else if over_bar {
            // 越过任务栏一半 → 吸整屏下沿（任务栏下沿），盒子可以压在任务栏上
            ny = bot_limit - h;
        } else if area.bottom() - (oy + h) <= edge {
            ny = area.bottom() - h;
        }
    }

    // 兜底：无论怎么吸，都不能把自己顶出屏幕（下边界按上面选定的那条线）
    nx = clamp_i(nx, area.x, area.right() - w);
    ny = clamp_i(ny, area.y, bot_limit - h);

    // ③ 绝不覆盖别的盒子：落点还压在人家身上 → 推去最近的空位。
    //
    // 光靠 ① 不够：盒子被直接扔到另一个盒子**身上**时，两个方向的缝隙都超出了
    // `dist`（重叠时缝隙算出来是负数，绝对值是半个盒子），于是谁都不吸，
    // 就这么叠着放下了（用户反馈「覆盖过多时不会吸附」）。
    // 这里的规则和任务栏胶囊一致：不允许重叠，就近找空位；
    // 左右（上下）差不多近时**优先往左**，这样夹在两个邻居中间时不会来回跳。
    let tol = (2.0 * sc).round() as i32; // 2 逻辑像素内的贴边不算覆盖
    let covered = |x: i32, y: i32| {
        others.iter().any(|o| {
            overlap_len(x, x + w, o.x, o.right()) > tol
                && overlap_len(y, y + h, o.y, o.bottom()) > tol
        })
    };
    if covered(nx, ny) {
        if let Some((fx, fy)) = nearest_free_spot(&others, &area, bot_limit, nx, ny, w, h, gap, dist) {
            nx = fx;
            ny = fy;
        }
    }

    let moved = nx != ox || ny != oy;
    // 跨屏拖动：盒子是 Progman 的子窗口，系统不会按新屏的 DPI 自动重设尺寸，
    // 圆角区域也还是旧屏那个尺寸 —— 必须按「现在所在显示器」重算一次。
    //
    // 这里**不做「尺寸没变就跳过」的优化**：tao 在换屏时会自己按 DPI 调一次窗口尺寸，
    // 但圆角区域不动，于是出现「窗口 100、区域 80」的错位，右下角被硬裁掉。
    // 拖完总是重走一遍 strip_chrome（设位置 + 尺寸 + 区域），一次 SetWindowPos 的成本可以忽略。
    let sc_now = desktop::scale_at(nx + w / 2, ny + h / 2);
    desktop::strip_chrome(hwnd_of(&bw), nx, ny, BOX_W as i32, BOX_W as i32, BOX_RADIUS, true);
    // 页面按这块屏的缩放重排（盒子窗口的 DPR 不可信，见 BoxView 的 box:scale）
    let _ = app.emit(
        "box:scale",
        BoxScale { id: id.clone(), scale: sc_now },
    );
    // 落盘：拖完的位置（吸附后的）总是要存，所以前端不用再调 move_box
    {
        let mut cfg = state.cfg.lock().unwrap();
        if let Some(b) = cfg.boxes.iter_mut().find(|b| b.id == id) {
            if b.x != nx || b.y != ny {
                b.x = nx;
                b.y = ny;
                store::save(&cfg);
            }
        }
    }
    // 整团对齐：锚点吸好了，其余成员按固定偏移跟过来并一起落盘。
    // 必须在下面 embed_after_snap 之前 —— 那个函数可能把这个盒子改成胶囊、换掉坐标系。
    group_settle(&app, &state, &id);
    // 压在任务栏上 / 从任务栏拖回来 → 后台切换窗口层级（顶层才盖得住任务栏）。
    // 必须放在落盘之后：重建窗口读的是配置里的位置。
    embed_after_snap(&app, id.clone());
    Ok(Some(moved))
}

/// 任务栏盒子的「吸附」：只能在任务栏这一条里移动。
///
/// 拖动时系统不管约束（盒子会短暂跑到工作区、甚至别的屏上），松手后由这里
/// 把横坐标夹进任务栏范围、纵坐标钉在任务栏上沿，再重摆一次窗口。
#[tauri::command]
async fn snap_bar(
    app: AppHandle,
    state: tauri::State<'_, State>,
    id: String,
) -> Result<Option<bool>, String> {
    // 形态正在切换（窗口正在重建）：这时的光标/窗口状态都是半成品，别判「拖出去」
    if switching(&id) {
        return Ok(Some(false));
    }
    if (unsafe { win32::GetAsyncKeyState(0x01) } as u16) & 0x8000 != 0 {
        return Ok(None);
    }
    let bw = box_window(&app, &id).ok_or("盒子窗口不存在")?;
    let bar = taskbar_rect().ok_or("任务栏不在屏幕底部（或已自动隐藏）")?;
    let (want_w, want_h) = bar_box_size();
    let br = rect_of(&bw).unwrap_or(Rect { x: bar.x, y: bar.y, w: want_w, h: want_h });

    // 反向操作：把胶囊从任务栏往上拖。
    // 两种信号都认（子窗口被任务栏夹住时，窗口矩形可能根本没离开任务栏）：
    //  ① 窗口中心越过任务栏上沿（真拖出去了）；
    //  ② 光标已经跑到任务栏上沿以上，**而且还在胶囊附近** —— 窗口动不了，
    //     但用户的方向很明确。
    //
    // ② 必须限定「光标贴着胶囊」：只按「光标在高处」判断的话，任何一次程序性
    // 移动（改设置、重建）都会因为光标不在任务栏而把胶囊踢出任务栏。
    let cursor = cursor_xy();
    // 团成员：拖动途中 `bar_hover` 记过「光标离开过任务栏」，松手就算拖出去
    let grouped = in_group(&*state, &id);
    let escaped = if grouped {
        let mut v = DRAG_ESCAPED.lock().unwrap();
        let had = v.iter().any(|x| x == &id);
        v.retain(|x| x != &id);
        had
    } else {
        false
    };
    let out_by_window = br.y + br.h / 2 < bar.y;
    let out_by_cursor = escaped
        || matches!(cursor, Some((cx, cy)) if {
            let above = cy < bar.y - BAR_UP_MIN && cy > bar.y - bar.h * 6;
            let near = (cx - (br.x + br.w / 2)).abs() <= br.w + 8;
            above && near
        });
    if out_by_window || out_by_cursor {
        let (cx, cy) = match cursor {
            Some((cx, cy)) => (cx, cy),
            None => (br.x + br.w / 2, br.y + br.h / 2),
        };
        // 团成员：整团一起退回桌面（不能只把这一个拿出来，那样整团会散架）
        if grouped {
            group_switch_mode(&app, &id, false, cx, cy, false);
        } else {
            unembed_at(&app, id.clone(), cx, cy);
        }
        return Ok(Some(true));
    }

    let (nx0, ny) = bar_slot(bar, br.x + br.w / 2);
    // 和旁边的胶囊排布：靠得近就贴住（0 缝），压到别人身上就挪到最近的空位。
    // 任务栏里所有胶囊同高同宽、都在同一条线上，所以只要处理横坐标。
    let nx = {
        let others: Vec<i32> = {
            let cfg = state.cfg.lock().unwrap();
            // 同团成员不参与「找空位」：它们跟着锚点走固定的横向偏移
            let gid = cfg
                .boxes
                .iter()
                .find(|b| b.id == id)
                .and_then(|b| b.group.clone());
            cfg.boxes
                .iter()
                .filter(|b| b.in_bar && b.id != id && !(gid.is_some() && b.group == gid))
                .map(|b| b.x)
                .collect()
        };
        let snap = (SNAP_DIST * desktop::scale_at(nx0 + want_w / 2, bar.y + want_h / 2)).round() as i32;
        bar_free_x(bar, want_w, nx0, &others, snap)
    };
    let moved = nx != br.x || ny != br.y || br.w != want_w || br.h != want_h;

    desktop::place_bar_box(hwnd_of(&bw), nx, ny, want_w, want_h, true);
    {
        let mut cfg = state.cfg.lock().unwrap();
        if let Some(b) = cfg.boxes.iter_mut().find(|b| b.id == id) {
            if b.x != nx || b.y != ny {
                b.x = nx;
                b.y = ny;
                store::save(&cfg);
            }
        }
    }
    // 胶囊团：其余成员按固定偏移跟到新位置并落盘
    group_settle(&app, &state, &id);
    let _ = app.emit(
        "box:scale",
        BoxScale {
            id: id.clone(),
            scale: desktop::scale_at(nx + want_w / 2, ny + want_h / 2),
        },
    );
    Ok(Some(moved))
}

/// 盒子所在显示器的缩放系数（100% = 1.0）。前端挂载时先拉一次，之后靠 `box:scale` 事件更新。
#[tauri::command]
fn box_scale(app: AppHandle, id: String) -> Result<f64, String> {
    let bw = box_window(&app, &id).ok_or("盒子窗口不存在")?;
    let r = rect_of(&bw).ok_or("读不到盒子位置")?;
    Ok(desktop::scale_at(r.x + r.w / 2, r.y + r.h / 2))
}

/// 盒子附近壁纸的亮度（0.0 纯黑 ~ 1.0 纯白），名字据此决定用白字还是深色字。
///
/// 没有底板之后，白字 + 深色描边在浅色壁纸上会糊成一团（用户实测白色壁纸下
/// 盒子名看不清），桌面图标是同一套问题的官方解法：字色跟着壁纸亮度走。
/// 判断放在前端（带滞回，避免亮度在阈值附近时来回跳），后端只负责报亮度。
#[tauri::command]
fn label_luma(app: AppHandle, id: String) -> Option<f64> {
    let bw = box_window(&app, &id)?;
    let r = rect_of(&bw)?;
    label::bg_luma(&r)
}

/// 等窗口真正从 Tauri 的窗口表里消失（destroy 是投递到主线程执行的）
fn wait_window_closed(app: &AppHandle, label: &str) -> bool {
    for _ in 0..80 {
        if app.get_webview_window(label).is_none() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    app.get_webview_window(label).is_none()
}

/// 按配置里的状态**重建**盒子窗口（无闪烁：新窗口先画好再露，旧的 140ms 后销毁）。
///
/// 为什么必须重建、不能就地把窗口在「桌面层 ↔ 顶层」之间换过去：
///
/// 实测（tools/probe_topmost.py）：
///  - 把已经挂到 Progman 的窗口 SetParent 回桌面之后，这个窗口的 WebView2 合成层
///    不再理会窗口区域（GetWindowRgn 直接报错，SetWindowRgn 也看不出任何效果），
///    四个角立刻变成直角；
///  - 再切回来时窗口与区域的换算对不上，圆角半径就变小了。
/// 所以只能「按新状态另建一个窗口」。
///
/// 而直接「销毁旧的 → 建新的」会闪：中间那一下屏幕上是空的，新窗口还会先露白底/标题栏。
/// 现在的顺序是：
///   1. 新窗口先**隐藏**建好（同坐标、同尺寸、已裁好圆角，tao 的 show 完全不参与）；
///   2. 等前端回报「首帧画完了」（box_ready）；
///   3. 用 Win32 显示新窗口 —— 两个窗口此刻在同一位置、画面一模一样；
///   4. 再等 140ms 让新窗口稳定出帧，最后销毁旧窗口。
/// 于是切换过程里屏幕上一直有个一模一样的盒子盖着，看不出闪烁。
fn rebuild_box_flashless(app: &AppHandle, b: &BoxConfig) -> Result<(), String> {
    mark_box_not_ready(&b.id);
    // 这个 id 名下**所有**还活着的窗口都要收掉，不只是「第一个」。
    // 整团切换被两处同时触发时（松手吸附 + 某个成员自己的 snap_bar），会各建一个窗口，
    // 只销毁一个就漏下一个 —— 用户看到的就是「同一个收纳盒被复制了一个」。
    let old = box_windows_all(app, &b.id);
    let new_win = spawn_box_window(app, b, false)?;
    let keep = new_win.label().to_string();
    wait_box_ready(&b.id, 900);
    show_box(&new_win, b);

    std::thread::sleep(std::time::Duration::from_millis(140));

    let mut stale: Vec<String> = Vec::new();
    for w in old {
        if w.label() == keep {
            continue;
        }
        // 先摘掉守护登记：窗口马上销毁，别让守护线程再去碰它
        desktop::forget(hwnd_of(&w));
        stale.push(w.label().to_string());
        let _ = w.destroy();
    }
    // 等它们真的从窗口表里消失，否则下一次 `box_window` 可能拿到即将销毁的那个
    for l in stale {
        wait_window_closed(app, &l);
    }
    // 重建窗口会抢焦点，面板本来开着就会被「失焦自动收起」误收，把焦点还回去
    if let Some(p) = app.get_webview_window("panel") {
        if p.is_visible().unwrap_or(false) {
            let _ = p.set_focus();
        }
    }
    Ok(())
}

#[tauri::command]
async fn set_box_topmost(
    app: AppHandle,
    state: tauri::State<'_, State>,
    id: String,
    on_top: bool,
) -> Result<(), String> {
    // 重建期间同样别让 snap 把旧坐标写回配置
    let _sg = switching_begin(&id);
    let b = {
        let mut cfg = state.cfg.lock().unwrap();
        let b = cfg.boxes.iter_mut().find(|b| b.id == id).ok_or("盒子不存在")?;
        if b.in_bar {
            return Err("这个盒子已嵌入任务栏，先移出任务栏再设置置顶".into());
        }
        b.topmost = on_top;
        // 关掉置顶时，如果盒子正压在任务栏上，它会退回桌面层、被任务栏盖住看不见 ——
        // 看起来就像「盒子突然没了」。这里顺手把它拉回工作区内。
        if !on_top {
            let size = (BOX_W * desktop::scale_at(b.x + BOX_W as i32 / 2, b.y + BOX_W as i32 / 2))
                .round() as i32;
            if on_taskbar(b.x, b.y, size) {
                if let Some(m) = desktop::monitor_at(b.x + size / 2, b.y + size / 2) {
                    b.y = (m.work.y + m.work.h - size).max(m.work.y);
                }
            }
        }
        let b = b.clone();
        store::save(&cfg);
        b
    };

    rebuild_box_flashless(&app, &b)?;
    notify_box_changed(&app, &id);
    Ok(())
}

/// 「嵌入任务栏 / 移出任务栏」。
///
/// 嵌入 = 把盒子窗口 `SetParent` 成 `Shell_TrayWnd` 的子窗口：
///  - 高 = 任务栏厚度，宽 = 厚度 + 右侧两个汉字的位置（横向胶囊）；
///  - 和任务栏同一个 z 带 → 点任务栏、任务栏重绘都盖不住它；
///  - 只能横向移动，纵坐标钉在任务栏上沿（`snap_bar` 负责）。
///
/// 移出 = 退回普通盒子（桌面层），位置摆到任务栏正上方的桌面区域，
/// 免得落在工作区外看不见。
///
/// 用户还有两条不用菜单的路：拖到任务栏上一半以上会自动嵌入（`embed_after_snap`），
/// 把胶囊往桌面拖出来会自动还原（`snap_bar` → `unembed_after_snap`）。
#[tauri::command]
async fn set_box_in_bar(
    app: AppHandle,
    state: tauri::State<'_, State>,
    id: String,
    on: bool,
) -> Result<(), String> {
    // 胶囊团成员：整团一起移入 / 移出。
    //
    // 团是一个整体，菜单里这一项点的是「团」，不是「这一个成员」：
    // 只把点中的那个切过去，队友还留在原形态里，团当场散架（用户实测反馈）。
    // 整团切换要重建 N 个窗口，走 `group_switch_mode` 那条路（它自带互斥闸）。
    {
        let in_group = {
            let cfg = state.cfg.lock().unwrap();
            group_members_of(&cfg, &id).is_some()
        };
        if in_group {
            let r = {
                let cfg = state.cfg.lock().unwrap();
                cfg.boxes.iter().find(|b| b.id == id).map(|b| live_rect(&app, b))
            };
            let Some(r) = r else {
                return Err("盒子不存在".into());
            };
            group_switch_mode(&app, &id, on, r.x + r.w / 2, r.y + r.h / 2, on);
            return Ok(());
        }
    }
    // 整个嵌入/移出过程（改配置 + 重建窗口）都别让 snap / move_box 插进来
    let _sg = switching_begin(&id);
    let b = {
        let mut cfg = state.cfg.lock().unwrap();
        let others: Vec<i32> = cfg
            .boxes
            .iter()
            .filter(|o| o.in_bar && o.id != id)
            .map(|o| o.x)
            .collect();
        let b = cfg.boxes.iter_mut().find(|b| b.id == id).ok_or("盒子不存在")?;
        if on {
            let bar = taskbar_rect()
                .ok_or("检测不到任务栏（需要任务栏在屏幕底部、未自动隐藏）")?;
            // 排到现有胶囊队列的末尾（队尾放不下就排队首），**不按盒子在桌面上的
            // 水平位置落位** —— 那样会正好压住盒子正下方的任务栏按钮（用户反馈）。
            // 也绝不越过托盘左沿，托盘图标 / 时钟不能被盖住。见 `bar_append_x`。
            let size = (BOX_W * desktop::scale_at(b.x + BOX_W as i32 / 2, b.y + BOX_W as i32 / 2))
                .round() as i32;
            let (want_w, _) = bar_box_size();
            let snap = (SNAP_DIST * desktop::scale_at(b.x + size / 2, bar.y + bar.h / 2)).round() as i32;
            let x = bar_append_x(bar, want_w, want_w, &others).unwrap_or_else(|| {
                bar_free_x(bar, want_w, b.x + size / 2 - want_w / 2, &others, snap)
            });
            b.x = x;
            b.y = bar.y;
            b.in_bar = true;
            // 任务栏盒子是任务栏的子窗口，置顶没有意义
            b.topmost = false;
        } else {
            let center_x = b.x + bar_box_size().0 / 2;
            b.in_bar = false;
            if let Some(m) = desktop::monitor_at(center_x, b.y) {
                let sc = m.scale;
                let size = (BOX_W * sc).round() as i32;
                // 以原来的按钮中心为基准摆到任务栏正上方
                let nx = clamp_i(
                    center_x - size / 2,
                    m.work.x,
                    (m.work.x + m.work.w - size).max(m.work.x),
                );
                b.x = nx;
                b.y = (m.work.y + m.work.h - size).max(m.work.y);
            }
        }
        let b = b.clone();
        // 换成另一种形态（桌面盒 ↔ 任务栏胶囊）就没法和原来的团保持同一个坐标系了，
        // 把它从团里摘出去（剩下的人继续是一个团）。
        drop_from_group(&mut cfg, &id);
        store::save(&cfg);
        b
    };

    rebuild_box_flashless(&app, &b)?;
    notify_box_changed(&app, &id);
    Ok(())
}
/// 重建串行闸：同一时刻只允许一个盒子在重建（重建要 300~500ms）。
static RELAY_LOCK: Mutex<()> = Mutex::new(());

/// 拿重建闸，**最多等 `ms` 毫秒**。
///
/// 原来一律用 `lock()`：万一某个重建线程卡死（比如被迫在系统拖动循环里建窗口，
/// 主线程一直不回到消息泵），锁就永远回不来 —— 之后所有「嵌入 / 移出任务栏」
/// 全部排队等死，用户看到的是「整个程序卡死」。这里改成有超时：
/// 等不到就放弃这次切换（盒子留在原形态），至少别的盒子还能用。
fn lock_relay(ms: u64) -> Option<std::sync::MutexGuard<'static, ()>> {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        if let Ok(g) = RELAY_LOCK.try_lock() {
            return Some(g);
        }
        if std::time::Instant::now() >= until {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// 把屏幕坐标 (x, y) 处、边长 size 的盒子换算成「嵌入任务栏」后的位置。
///
/// 横坐标保持用户松手时的**水平中心**（看着哪就嵌哪），再夹进任务栏范围；
/// 纵坐标恒等于任务栏上沿。
fn bar_slot(bar: Rect, center_x: i32) -> (i32, i32) {
    let (w, _) = bar_box_size();
    let x = clamp_i(center_x - w / 2, bar.x, (bar.right() - w).max(bar.x));
    (x, bar.y)
}

/// 在任务栏里给「宽 `want_w` 的胶囊」找一个落点：靠得近就贴住邻居，否则停在
/// 离 `nx0` 最近的**空位**上，**绝不压住任何已有胶囊**。
///
/// `others` 是其它胶囊的横坐标（同宽同高，所以只看横坐标）。
///
/// 为什么不写成「压住谁就推到谁旁边、循环几轮」：胶囊排成一排时，推到旁边往往
/// 又压住下一个，一路推下去就会走出整排胶囊，最后停在整个胶囊群的**最边上** ——
/// 用户明明把它放在群体右侧，它却飞到了最左边（实测反馈）。
///
/// 这里改成「枚举所有可行落点，取离目标最近的」：可行落点 = 目标位置本身、每个
/// 邻居的左右两侧、任务栏两端。因为要挑「最近的可行点」，结果天然就是
/// 「离自己最近的那块空位」，既不会覆盖别人，也不会跨过别人跑到另一头。
fn bar_free_x(bar: Rect, want_w: i32, nx0: i32, others: &[i32], snap: i32) -> i32 {
    let lo = bar.x;
    let hi = (bar.right() - want_w).max(bar.x);
    let target = clamp_i(nx0, lo, hi);

    let mut cands: Vec<i32> = Vec::with_capacity(others.len() * 2 + 3);
    cands.push(target);
    for &ox in others {
        cands.push(clamp_i(ox - want_w, lo, hi));
        cands.push(clamp_i(ox + want_w, lo, hi));
    }
    cands.push(lo);
    cands.push(hi);
    cands.sort_unstable();
    cands.dedup();

    // 整个胶囊都不和任何邻居重叠才算「可行」
    let free = |x: i32| others.iter().all(|&ox| x + want_w <= ox || x >= ox + want_w);

    // ① 距邻居够近（<= snap）就贴上去，不留缝。
    //
    // **左侧优先**：左右两个邻居都够得着时固定贴左边那个（用户要求）。原来把所有
    // 候选混在一起按「离目标最近」选，两侧间隙接近时会在两个邻居之间来回跳。
    let mut left: Option<(i32, i32)> = None; // (距离, x) 贴在我左边那个邻居的右侧
    let mut right: Option<(i32, i32)> = None; // (距离, x) 贴在我右边那个邻居的左侧
    for &ox in others {
        let x = clamp_i(ox + want_w, lo, hi);
        if free(x) {
            let d = (x - target).abs();
            if d <= snap && left.map_or(true, |(bd, _)| d < bd) {
                left = Some((d, x));
            }
        }
        let x = clamp_i(ox - want_w, lo, hi);
        if free(x) {
            let d = (x - target).abs();
            if d <= snap && right.map_or(true, |(bd, _)| d < bd) {
                right = Some((d, x));
            }
        }
    }
    if let Some((_, x)) = left.or(right) {
        return x;
    }

    // ② 没有邻居可贴，但离任务栏两端够近 → 贴着那一端
    for x in [lo, hi] {
        if free(x) && (x - target).abs() <= snap {
            return x;
        }
    }

    // ③ 否则取最近的可行点（目标本身可行时距离为 0，原地不动）
    let mut best: Option<(i32, i32)> = None;
    for &x in &cands {
        if !free(x) {
            continue;
        }
        let d = (x - target).abs();
        if best.map_or(true, |(bd, _)| d < bd) {
            best = Some((d, x));
        }
    }
    // ④ 整条任务栏都塞满了（极罕见）：只能原地放着，总比飞到别处强
    best.map(|(_, x)| x).unwrap_or(target)
}

/// 「嵌入任务栏」这个**动作**该把胶囊插到哪：排到现有胶囊队列的末尾。
///
/// 用户反馈：以前按盒子在桌面上的水平位置落位，盒子在哪儿，胶囊就落在它正下方的
/// 任务栏坐标上 —— 那一片往往是任务按钮的位置，等于把正在用的程序按钮盖住了。
///
/// 规则（用户给定）：
///  1. 优先贴到**队尾** —— 现有胶囊里最靠右那个的右边，紧挨着排；
///  2. 队尾放不下（越过托盘左边界）就贴到**队首** —— 最靠左那个的左边；
///  3. 一个胶囊都没有时，直接贴到任务栏右端（托盘左边），也就是「队尾」该在的地方；
///  4. 两头都放不下才退回 `bar_free_x` 的「找最近空位」，绝不重叠。
///
/// 右边界一律取 `TrayNotifyWnd` 的左沿：托盘图标、输入法、时钟、显示桌面按钮
/// 全在那一块里，压上去就是遮住它们。留 4px 缝隙，和任务栏自己的按钮间距对得上。
///
/// `span_w` 是整排的宽度（单个胶囊 = `unit_w`；整团 = `unit_w × 成员数`），
/// `others` 是**队外**胶囊的横坐标（同宽同高，只看 x）。
///
/// 返回 `None` = 队尾、队首都被占满（极罕见），调用方自己去「找最近空位」兜底。
fn bar_append_x(bar: Rect, span_w: i32, unit_w: i32, others: &[i32]) -> Option<i32> {
    let lo = bar.x;
    let right_limit = desktop::taskbar_tray_left().unwrap_or(bar.right()) - 4;
    // 「最右的合法起点」：再往右就压到托盘了
    let max_start = (right_limit - span_w).max(lo);

    let mut sorted: Vec<i32> = others.to_vec();
    sorted.sort_unstable();

    // ① 队尾：紧贴最右那个胶囊
    if let Some(&max_x) = sorted.last() {
        let x = max_x + unit_w;
        if x >= lo && x <= max_start {
            return Some(x);
        }
    }
    // ② 队首：整排塞进最左那个胶囊的左边
    if let Some(&min_x) = sorted.first() {
        let x = min_x - span_w;
        if x >= lo {
            return Some(x);
        }
    }
    // ③ 一个胶囊都没有：直接贴右端（就是「队尾」该在的位置）
    if sorted.is_empty() {
        return Some(max_start);
    }
    None
}

/// 桌面盒子的「找空位」：盒子落到别人**身上**时，推去离它最近的、不压任何人的位置。
///
/// 和 `bar_free_x` 同一套思路（枚举可行落点，取离目标最近的），区别是桌面是二维的：
/// 候选点 = 目标位置本身 + 每个邻居的左右上下四侧 + 工作区四角（夹过边界之后）。
///
/// **左右 / 上下差不多近时优先「左边、上边」**：盒子夹在两个邻居中间时，
/// 两侧的空位往往只差一两像素，只按距离选会在两边来回跳（用户明确要求左优先）。
/// 所以距离差不超过 `pref`（吸附阈值）时，优先取左边/上边那个候选。
fn nearest_free_spot(
    others: &[Rect],
    area: &Rect,
    bot_limit: i32,
    ox: i32,
    oy: i32,
    w: i32,
    h: i32,
    gap: i32,
    pref: i32,
) -> Option<(i32, i32)> {
    let lo_x = area.x;
    let hi_x = (area.right() - w).max(area.x);
    let lo_y = area.y;
    let hi_y = (bot_limit - h).max(area.y);
    // 优先级：0 = 让到邻居的左边 / 上边（用户要求优先），1 = 右边 / 下边
    let mut cands: Vec<(i32, i32, u8)> = Vec::with_capacity(others.len() * 4 + 1);
    cands.push((clamp_i(ox, lo_x, hi_x), clamp_i(oy, lo_y, hi_y), 0));
    for o in others {
        let y = clamp_i(o.y, lo_y, hi_y);
        let x = clamp_i(o.x, lo_x, hi_x);
        // 邻居左边 / 上边（优先）
        cands.push((clamp_i(o.x - gap - w, lo_x, hi_x), y, 0));
        cands.push((x, clamp_i(o.y - gap - h, lo_y, hi_y), 0));
        // 邻居右边 / 下边
        cands.push((clamp_i(o.x + o.w + gap, lo_x, hi_x), y, 1));
        cands.push((x, clamp_i(o.y + o.h + gap, lo_y, hi_y), 1));
    }
    let free = |x: i32, y: i32| {
        let r = Rect { x, y, w, h };
        !others.iter().any(|o| r.hits(o))
    };
    let mut scored: Vec<(i32, u8, i32, i32)> = Vec::with_capacity(cands.len());
    for (x, y, prio) in cands {
        if !free(x, y) {
            continue;
        }
        scored.push(((x - ox).abs() + (y - oy).abs(), prio, x, y));
    }
    if scored.is_empty() {
        return None;
    }
    let best_d = scored.iter().map(|s| s.0).min().unwrap_or(0);
    // 距离相近的候选里，左/上优先；同一优先级里再比距离
    let pick = scored
        .iter()
        .filter(|s| s.0 <= best_d + pref)
        .min_by_key(|s| (s.1, s.0))
        .copied()?;
    Some((pick.2, pick.3))
}

// ---------------- 胶囊团（多个吸附在一起的盒子当整体拖） ----------------
/// 一次「胶囊团拖动」会话。
///
/// 只在拖动期间存在：按下时按窗口真实位置算出「其它成员相对锚点的偏移」，
/// 之后由一个**独立线程**每 8ms 读一次锚点窗口的真实位置，把成员摆到「锚点 + 偏移」，
/// 松手落位后清空。
///
/// 为什么用后台线程而不是「前端每次 onMoved 发一次 invoke」：
/// 拖动期间主线程待在系统的窗口拖动模态循环里，任何一次跨线程 `SetWindowPos`
/// （不带 `SWP_ASYNCWINDOWPOS`）都可能永远等不到派发 —— 用户实测「一拖动就卡死」。
/// 线程里只做 `GetWindowRect` + 异步 `SetWindowPos`，不碰 Tauri 的窗口表、不发 IPC。
#[derive(Clone)]
pub(crate) struct GroupDrag {
    /// 正在被用户拖的那个盒子（其余成员跟着它走）
    pub anchor: String,
    /// 锚点窗口句柄（线程里直接用它读位置，不去查 Tauri 的窗口表）
    pub anchor_hwnd: isize,
    /// (成员 id, dx, dy)：dy 对任务栏胶囊没有意义（y 恒等于任务栏上沿）。
    /// 收尾落盘还在用，所以要留着。
    pub members: Vec<(String, i32, i32)>,
    /// 拖动途中真正要摆的窗口（纯 Win32 数据，线程只认这个）
    pub chips: Vec<FollowChip>,
    /// 锚点所在的那条任务栏（团在任务栏里拖动时要横向夹住；在桌面上是 None）
    pub bar: Option<Rect>,
    /// 胶囊尺寸（任务栏模式下的宽高）
    pub bar_size: (i32, i32),
    /// 会话代号：新的一次拖动会自增，旧线程看见代号变了就自己退出
    pub gen: u64,
}

/// 跟随线程要摆的一个窗口。
#[derive(Clone)]
pub(crate) struct FollowChip {
    pub hwnd: isize,
    /// 相对锚点左上角的屏幕坐标偏移
    pub dx: i32,
    pub dy: i32,
    /// 屏幕坐标 → 父客户区坐标 的固定偏移（`place = screen + off`），开始拖时算一次
    pub off: (i32, i32),
    /// 胶囊：纵坐标恒为任务栏上沿、横坐标夹在任务栏范围内
    pub in_bar: bool,
}

/// 两块区间 `[a0,a1)` 与 `[b0,b1)` 的重叠长度
fn overlap_len(a0: i32, a1: i32, b0: i32, b1: i32) -> i32 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

/// 两个盒子是否「吸附在一起」—— 组成胶囊团的判据。
///
/// 和 `snap_box` 的吸附条件同源：一个方向上重叠过半、另一个方向上缝隙不超过吸附阈值。
/// 吸好的盒子之间缝隙是 0，但用户可能只吸住一部分（比如拖到一半没吸上），
/// 这里同样按 `SNAP_DIST` 放宽，判定结果才和用户眼睛看到的一致。
fn boxes_touch(a: Rect, b: Rect, snap: i32) -> bool {
    let ov_x = overlap_len(a.x, a.right(), b.x, b.right());
    let ov_y = overlap_len(a.y, a.bottom(), b.y, b.bottom());
    // 分离时的缝隙（重叠时为 0）
    let gap_x = (b.x - a.right()).max(a.x - b.right()).max(0);
    let gap_y = (b.y - a.bottom()).max(a.y - b.bottom()).max(0);
    // 横向并排：竖直方向重叠过半 + 水平缝隙够小
    if ov_y * 2 >= a.h.min(b.h) && gap_x <= snap {
        return true;
    }
    // 纵向堆叠：水平方向重叠过半 + 竖直缝隙够小
    ov_x * 2 >= a.w.min(b.w) && gap_y <= snap
}

/// 盒子**当前**的屏幕矩形（物理像素）。
///
/// 有窗口就问 Win32（配置里的坐标可能还没落盘）；窗口没了（Explorer 重启留下的
/// 僵尸记录）就按配置和形态估算 —— 至少不会把整团判断成「孤零零一个」。
fn live_rect(app: &AppHandle, b: &BoxConfig) -> Rect {
    if let Some(w) = box_window(app, &b.id) {
        if let Some(r) = rect_of(&w) {
            return r;
        }
    }
    if b.in_bar {
        let (w, h) = bar_box_size();
        let (x, y) = bar_place_xy(b, w);
        Rect { x, y, w, h }
    } else {
        let s = (BOX_W * desktop::scale_at(b.x + 1, b.y + 1)).round() as i32;
        Rect { x: b.x, y: b.y, w: s, h: s }
    }
}

/// 从 `id` 出发，把「吸附连成一片」的盒子全部找出来（含自己）。
///
/// 只连**同一形态**的盒子：桌面盒和任务栏胶囊不在一个空间里，混在一团里
/// 拖动会互相拉扯（一个要钉在任务栏上沿、一个要待在桌面）。
fn cluster_of(app: &AppHandle, cfg: &Config, id: &str) -> Vec<String> {
    let Some(start) = cfg.boxes.iter().find(|b| b.id == id) else {
        return Vec::new();
    };
    let mode = start.in_bar;
    let snap = (SNAP_DIST * desktop::scale_at(start.x + 1, start.y + 1)).round() as i32;
    let nodes: Vec<(String, Rect)> = cfg
        .boxes
        .iter()
        .filter(|b| b.in_bar == mode)
        .map(|b| (b.id.clone(), live_rect(app, b)))
        .collect();

    let mut out: Vec<String> = vec![id.to_string()];
    let mut frontier: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, (nid, _))| nid == id)
        .map(|(i, _)| i)
        .collect();
    while let Some(i) = frontier.pop() {
        for j in 0..nodes.len() {
            if out.iter().any(|x| x == &nodes[j].0) {
                continue;
            }
            if boxes_touch(nodes[i].1, nodes[j].1, snap) {
                out.push(nodes[j].0.clone());
                frontier.push(j);
            }
        }
    }
    out
}

/// 把盒子从所属的团里摘出来；摘完只剩一个成员的团就地解散（一个人的「团」没有意义）。
fn drop_from_group(cfg: &mut Config, id: &str) {
    let Some(b) = cfg.boxes.iter_mut().find(|b| b.id == id) else {
        return;
    };
    let Some(gid) = b.group.take() else {
        return;
    };
    let left = cfg
        .boxes
        .iter()
        .filter(|b| b.group.as_deref() == Some(gid.as_str()))
        .count();
    if left < 2 {
        for b in cfg.boxes.iter_mut() {
            if b.group.as_deref() == Some(gid.as_str()) {
                b.group = None;
            }
        }
    }
}

/// 把成员摆到「锚点当前位置 + 偏移」。`gd` 是本次拖动的快照（不读 `state.group_drag`）。
///
/// `full = false` 时只挪位置（拖动途中每帧都在摆，重的摆位会闪）；
/// `full = true` 走完整的摆位（尺寸/圆角区域/DPI 一次对齐），只在收尾时调一次。
fn group_follow_with(app: &AppHandle, state: &State, gd: &GroupDrag, full: bool) -> bool {
    let id = gd.anchor.as_str();
    let Some(aw) = box_window(app, id) else {
        return false;
    };
    let Some(ar) = rect_of(&aw) else {
        return false;
    };
    // 成员是桌面盒还是任务栏胶囊，决定用哪套摆位函数；
    // 顺带核一遍它还在这团里（拖动途中被摘出去的成员就别再拽了）
    let modes: Vec<Option<bool>> = {
        let cfg = state.cfg.lock().unwrap();
        let gid = cfg
            .boxes
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| b.group.clone());
        gd.members
            .iter()
            .map(|(mid, _, _)| {
                cfg.boxes
                    .iter()
                    .find(|b| &b.id == mid)
                    .filter(|b| gid.is_some() && b.group == gid)
                    .map(|b| b.in_bar)
            })
            .collect()
    };
    let bar = taskbar_rect();
    for (i, (mid, dx, dy)) in gd.members.iter().enumerate() {
        let Some(in_bar) = modes[i] else { continue };
        // 正在重建（整团切形态）的成员别碰：那个 hwnd 随时会失效
        if switching(mid) {
            continue;
        }
        let Some(w) = box_window(app, mid) else {
            continue;
        };
        let hwnd = hwnd_of(&w);
        if in_bar {
            let (ww, wh) = bar_box_size();
            let Some(bar) = bar else { continue };
            let nx = clamp_i(ar.x + dx, bar.x, (bar.right() - ww).max(bar.x));
            if full {
                desktop::place_bar_box(hwnd, nx, bar.y, ww, wh, true);
            } else {
                desktop::move_box_window(hwnd, nx, bar.y);
            }
        } else if full {
            // 桌面盒尺寸随屏缩放，strip_chrome 收的是**逻辑**尺寸（内部按目标位置换算）
            desktop::strip_chrome(hwnd, ar.x + dx, ar.y + dy, BOX_W as i32, BOX_W as i32, BOX_RADIUS, true);
        } else {
            desktop::move_box_window(hwnd, ar.x + dx, ar.y + dy);
        }
    }
    true
}

/// 拖动结束（吸附已完成）后：把成员对齐到锚点最终位置，并把**所有**成员的新坐标落盘。
///
/// 必须在锚点自己落盘、且吸附算完之后调 —— 否则成员会跟着一个还没修正的坐标走，
/// 出现「吸好了又散开」。跨屏拖动时顺带按新位置重发一次页面缩放。
fn group_settle(app: &AppHandle, state: &State, id: &str) {
    // 会话先摘下来：跟随线程一旦发现会话没了就停手，收尾期间不会再有窗口被它挪
    // （否则「对齐完又被拖走一帧」，落盘的位置还是旧的）。不是自己的会话就别动它。
    let gd = {
        let mut g = state.group_drag.lock().unwrap();
        match g.as_ref() {
            Some(s) if s.anchor == id => g.take(),
            _ => None,
        }
    };
    let Some(gd) = gd else { return };
    // 拖动途中形态被切了（比如从任务栏拖出来，`unembed_at` 把它摘出了团）：
    // 这次的偏移已经失效，直接丢弃会话，别把老队友拽到新位置去。
    let still_grouped = {
        let cfg = state.cfg.lock().unwrap();
        cfg.boxes.iter().find(|b| b.id == id).and_then(|b| b.group.clone()).is_some()
    };
    if !still_grouped {
        return;
    }
    // 收尾这一下走完整摆位：拖动途中只挪了位置，尺寸/圆角区域/跨屏 DPI 在这里一次对齐
    if !group_follow_with(app, state, &gd, true) {
        return;
    }
    let members = gd.members;
    let mut changed = false;
    {
        let mut cfg = state.cfg.lock().unwrap();
        for (mid, _, _) in &members {
            let Some(w) = box_window(app, mid) else { continue };
            let Some(r) = rect_of(&w) else { continue };
            if let Some(b) = cfg.boxes.iter_mut().find(|b| &b.id == mid) {
                // 和 move_box 一个口径：胶囊只认横坐标，纵坐标恒等于任务栏上沿
                let ny = if b.in_bar { b.y } else { r.y };
                if b.x != r.x || b.y != ny {
                    b.x = r.x;
                    b.y = ny;
                    changed = true;
                }
            }
            let _ = app.emit(
                "box:scale",
                BoxScale {
                    id: mid.clone(),
                    scale: desktop::scale_at(r.x + r.w / 2, r.y + r.h / 2),
                },
            );
        }
        if changed {
            store::save(&cfg);
        }
    }
}

/// 「组合为胶囊团」：把当前吸附成一片的盒子（`cluster_of`）打上同一个团 id。
///
/// 单个盒子（周围没有吸附的邻居）不允许组合 —— 菜单里也不会出现这一项。
#[tauri::command]
async fn group_boxes(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<usize, String> {
    let members = {
        let cfg = state.cfg.lock().unwrap();
        cluster_of(&app, &cfg, &id)
    };
    if members.len() < 2 {
        return Err("附近没有吸附在一起的收纳盒，不用组合".into());
    }
    let gid = format!("g{}", now_ms());
    {
        let mut cfg = state.cfg.lock().unwrap();
        for b in cfg.boxes.iter_mut() {
            if members.iter().any(|m| m == &b.id) {
                b.group = Some(gid.clone());
            }
        }
        store::save(&cfg);
    }
    for m in &members {
        notify_box_changed(&app, m);
    }
    Ok(members.len())
}

/// 「解散胶囊团」：把整个团的 `group` 清掉，成员恢复成可以单独拖动。
#[tauri::command]
async fn ungroup_box(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<usize, String> {
    let ids: Vec<String> = {
        let mut cfg = state.cfg.lock().unwrap();
        let gid = cfg.boxes.iter().find(|b| b.id == id).and_then(|b| b.group.clone());
        let Some(gid) = gid else {
            return Ok(0);
        };
        let mut ids = Vec::new();
        for b in cfg.boxes.iter_mut() {
            if b.group.as_deref() == Some(gid.as_str()) {
                b.group = None;
                ids.push(b.id.clone());
            }
        }
        store::save(&cfg);
        ids
    };
    for i in &ids {
        notify_box_changed(&app, i);
    }
    Ok(ids.len())
}

/// 这个盒子所属团的**全部成员**（含自己，按横坐标从左到右排好）。
///
/// 不在团里、或团里只剩自己一个 → `None`（一个人的「团」和没组队没区别）。
fn group_members_of(cfg: &Config, id: &str) -> Option<Vec<String>> {
    let gid = cfg.boxes.iter().find(|b| b.id == id)?.group.clone()?;
    let mut ids: Vec<String> = cfg
        .boxes
        .iter()
        .filter(|b| b.group.as_deref() == Some(gid.as_str()))
        .map(|b| b.id.clone())
        .collect();
    if ids.len() < 2 {
        return None;
    }
    // 左右顺序按当前横坐标定：切换形态后「谁在谁旁边」和用户看到的一致
    ids.sort_by_key(|i| cfg.boxes.iter().find(|b| &b.id == i).map(|b| b.x).unwrap_or(0));
    Some(ids)
}

/// `group_members_of` 的加锁版：这个盒子是不是某个团的成员。
fn in_group(state: &State, id: &str) -> bool {
    let cfg = state.cfg.lock().unwrap();
    group_members_of(&cfg, id).is_some()
}

/// 在任务栏里给「连续 `n` 个胶囊」找一段不压别人的起始横坐标。
///
/// 和 `bar_free_x` 同一套思路（枚举可行起点，取离目标最近的），区别只在于这里
/// 比较的是**一整排**的宽度 —— 团必须连在一起，不能拆开各找各的空位。
fn bar_free_span(bar: Rect, unit_w: i32, n: i32, nx0: i32, others: &[i32]) -> i32 {
    let span = unit_w * n.max(1);
    let lo = bar.x;
    let hi = (bar.right() - span).max(bar.x);
    let target = clamp_i(nx0, lo, hi);
    let free = |x: i32| others.iter().all(|&ox| x + span <= ox || x >= ox + unit_w);
    let mut cands: Vec<i32> = Vec::with_capacity(others.len() * 2 + 3);
    cands.push(target);
    for &ox in others {
        cands.push(clamp_i(ox - span, lo, hi));
        cands.push(clamp_i(ox + unit_w, lo, hi));
    }
    cands.push(lo);
    cands.push(hi);
    let mut best: Option<(i32, i32)> = None;
    for x in cands {
        if !free(x) {
            continue;
        }
        let d = (x - target).abs();
        if best.map_or(true, |(bd, _)| d < bd) {
            best = Some((d, x));
        }
    }
    best.map(|(_, x)| x).unwrap_or(target)
}

/// 整团在「桌面盒 ↔ 任务栏胶囊」之间切换形态。
///
/// 团是**一个整体**，形态也必须整团一起换 —— 原来只把被拖的那一个切过去，
/// 队友留在任务栏里，整团当场散架，接着程序就卡死了（用户实测）。
///
/// `anchor` 是被用户拖动的那个盒子，`cx/cy` 是落点（屏幕坐标，物理像素）：
/// 它落到光标处，其余成员按原来的左右顺序紧挨着排在它旁边。
///
/// `append` = 这次切换是**菜单「嵌入任务栏」触发的**，不是拖过去的：位置不再
/// 由落点决定，而是整排排到现有胶囊的队尾（见 `bar_append_x`）。两条路的区别在
/// 用户意图上 —— 拖过去是「我要它待在这儿」，点菜单是「收进任务栏去」。
///
/// 只能在**松手之后**调：一次要重建 N 个窗口（每个几百毫秒），手还按着的时候做
/// 这件事既慢又容易和系统拖动循环互锁。
fn group_switch_mode(
    app: &AppHandle,
    anchor: &str,
    to_bar: bool,
    cx: i32,
    cy: i32,
    append: bool,
) {
    // 同一个团同一时刻只允许一次切换。
    //
    // 一次「拖出去松手」会被两处同时看到：`snap_bar` 的松手分支，以及被打上
    // `DRAG_ESCAPED` 之后某个成员自己那一轮的 `snap_bar`。两处各起一个线程，
    // 就会把同一批窗口重建两遍，中间那一下会留下两个同 id 的窗口 ——
    // 用户看到的就是「收纳盒被复制了一个」。这里用团 id 加一道闸，第二次直接忽略。
    let gid = {
        let st = app.state::<State>();
        let cfg = st.cfg.lock().unwrap();
        cfg.boxes
            .iter()
            .find(|b| b.id == anchor)
            .and_then(|b| b.group.clone())
    };
    let Some(gid) = gid else { return };
    {
        let mut list = GROUP_SWITCHING.lock().unwrap();
        if list.iter().any(|g| g == &gid) {
            return;
        }
        list.push(gid.clone());
    }
    let app = app.clone();
    let anchor = anchor.to_string();
    std::thread::spawn(move || {
        do_group_switch(&app, &anchor, to_bar, cx, cy, append);
        GROUP_SWITCHING.lock().unwrap().retain(|g| g != &gid);
    });
}

/// 正在切换形态的团 id（`group_switch_mode` 的互斥表），见那里的说明。
static GROUP_SWITCHING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 整团**一次性**换形态：先把所有新窗口隐藏着建好、等首帧，再同一轮全部显示，
/// 最后统一销毁旧窗口。
///
/// 为什么不直接循环调 `rebuild_box_flashless`：那是一个盒子一个盒子地
/// 「建 → 等首帧 → 显示 → 睡 140ms → 销毁旧的」，一个成员 300~500ms，
/// 三个成员就要一秒多 —— 用户看到的就是「一个一个挪进/挪出任务栏」。
/// 整团的意义是一次动完，所以把等待合并：新窗口并行建、并行等首帧，
/// 顺序只影响建窗口那几十毫秒，显示和销毁都落在同一帧里。
fn rebuild_group_flashless(app: &AppHandle, plan: &[BoxConfig]) {
    if plan.is_empty() {
        return;
    }
    // 新旧交替这段时间里，这一批盒子都不许被 snap / move_box / 守护线程碰。
    // 不同 id 的 guard 互不干扰（见 switching_begin 的说明），放一个 Vec 里一起活着。
    let _guards: Vec<SwitchGuard> = plan.iter().map(|b| switching_begin(&b.id)).collect();

    let mut olds: Vec<Vec<tauri::WebviewWindow>> = Vec::with_capacity(plan.len());
    let mut news: Vec<(BoxConfig, tauri::WebviewWindow)> = Vec::with_capacity(plan.len());
    for b in plan {
        mark_box_not_ready(&b.id);
        // 同一 id 名下所有旧窗口都要收掉（整团切换被两处触发过会留下两个）
        olds.push(box_windows_all(app, &b.id));
        match spawn_box_window(app, b, false) {
            Ok(w) => news.push((b.clone(), w)),
            Err(e) => eprintln!("胶囊团切换形态失败（{}）：{e}", b.id),
        }
    }
    if news.is_empty() {
        return;
    }
    // 新窗口是并行画的：第一个等得久一点，后面的基本已经画完了，瞬间返回
    for (b, _) in &news {
        wait_box_ready(&b.id, 900);
    }
    // 同一轮里全部露出来 —— 这一步就是「整体」的观感来源
    for (b, w) in &news {
        show_box(w, b);
    }
    // 等新窗口都稳定出帧，屏幕上已经有整排新盒子盖在旧盒子上面了
    std::thread::sleep(std::time::Duration::from_millis(140));
    let mut stale: Vec<String> = Vec::new();
    for ws in olds {
        for w in ws {
            if news.iter().any(|(_, n)| n.label() == w.label()) {
                continue;
            }
            // 先摘掉守护登记：窗口马上销毁，别让守护线程再去碰它
            desktop::forget(hwnd_of(&w));
            stale.push(w.label().to_string());
            let _ = w.destroy();
        }
    }
    for l in stale {
        wait_window_closed(app, &l);
    }
    // 重建窗口会抢焦点，面板本来开着就会被「失焦自动收起」误收，把焦点还回去
    if let Some(p) = app.get_webview_window("panel") {
        if p.is_visible().unwrap_or(false) {
            let _ = p.set_focus();
        }
    }
    for b in plan {
        notify_box_changed(app, &b.id);
    }
}

fn do_group_switch(
    app: &AppHandle,
    anchor: &str,
    to_bar: bool,
    cx: i32,
    cy: i32,
    append: bool,
) {
    let Some(_guard) = lock_relay(8000) else { return };
    let st = app.state::<State>();
    // 拖动留下的偏移会话到此为止：坐标要整体重排，旧偏移没有意义
    *st.group_drag.lock().unwrap() = None;

    let (order, already) = {
        let cfg = st.cfg.lock().unwrap();
        match group_members_of(&cfg, anchor) {
            // 整团**已经都是目标形态**：只要把位置摆正，不用重建窗口。
            // 这条守住了「同一次切换被触发两遍」的第二遍 —— 第二遍到这里发现
            // 形态早就对了，直接摆位收工，不会再建一批窗口。
            Some(ids) => {
                let already = ids.iter().all(|i| {
                    cfg.boxes
                        .iter()
                        .find(|b| &b.id == i)
                        .map(|b| b.in_bar == to_bar)
                        .unwrap_or(false)
                });
                (ids, already)
            }
            None => return,
        }
    };
    let ai = match order.iter().position(|i| i == anchor) {
        Some(i) => i,
        None => return,
    };
    let mon = desktop::monitor_at(cx, cy).or_else(desktop::primary_monitor);
    let Some(mon) = mon else { return };
    let size = (BOX_W * mon.scale).round() as i32;
    let work = mon.work;

    let mut plan: Vec<BoxConfig> = Vec::with_capacity(order.len());
    {
        let mut cfg = st.cfg.lock().unwrap();
        // 团外、已经在任务栏里的胶囊：整排落位时要避开它们
        let taken: Vec<i32> = cfg
            .boxes
            .iter()
            .filter(|o| o.in_bar && !order.iter().any(|i| &o.id == i))
            .map(|o| o.x)
            .collect();
        let bar = taskbar_rect();
        if to_bar && bar.is_none() {
            return; // 任务栏不在底部（或自动隐藏）：没法嵌，保持原样
        }
        let (x0, y0, step) = if to_bar {
            let bar = bar.unwrap();
            let (unit_w, _) = bar_box_size();
            let n = order.len() as i32;
            let span = unit_w * n;
            let x = if append {
                // 菜单嵌入：整排排到队尾（放不下就队首），不理会落点
                bar_append_x(bar, span, unit_w, &taken)
                    .unwrap_or_else(|| bar_free_span(bar, unit_w, n, bar.right(), &taken))
            } else {
                // 拖进任务栏：锚点中心对着光标，队友按原顺序排在两边
                let want = cx - unit_w * (ai as i32) - unit_w / 2;
                bar_free_span(bar, unit_w, n, want, &taken)
            };
            (x, bar.y, unit_w)
        } else {
            // 桌面盒之间是无缝贴紧（SNAP_GAP = 0），整排夹进工作区
            let n = order.len() as i32;
            let span = size * n;
            let want = cx - size * (ai as i32) - size / 2;
            let x = clamp_i(want, work.x, (work.right() - span).max(work.x));
            let y = clamp_i(cy - size / 2, work.y, (work.bottom() - size).max(work.y));
            (x, y, size)
        };
        for (i, id) in order.iter().enumerate() {
            let Some(b) = cfg.boxes.iter_mut().find(|b| &b.id == id) else {
                continue;
            };
            b.x = x0 + i as i32 * step;
            b.y = y0;
            b.in_bar = to_bar;
            // 层级由形态决定：胶囊靠父子关系、桌面盒默认进桌面层，
            // 手动置顶的标记留着只会在切回来之后莫名其妙浮在最上层
            b.topmost = false;
            plan.push(b.clone());
        }
        store::save(&cfg);
    }
    if plan.is_empty() {
        return;
    }
    if already {
        // 形态没变：只把窗口挪到位（不进桌面层/任务栏，不需要重建）
        let bar = taskbar_rect();
        for b in &plan {
            let _sg = switching_begin(&b.id);
            let Some(w) = box_window(app, &b.id) else { continue };
            let hwnd = hwnd_of(&w);
            if to_bar {
                let Some(bar) = bar else { continue };
                let (bw, bh) = bar_box_size();
                desktop::place_bar_box(hwnd, b.x, bar.y, bw, bh, true);
            } else {
                desktop::strip_chrome(
                    hwnd,
                    b.x,
                    b.y,
                    BOX_W as i32,
                    BOX_W as i32,
                    BOX_RADIUS,
                    true,
                );
            }
            let _ = app.emit(
                "box:scale",
                BoxScale {
                    id: b.id.clone(),
                    scale: desktop::scale_at(b.x + 1, b.y + 1),
                },
            );
            notify_box_changed(app, &b.id);
        }
        return;
    }
    // 整团一次换完：所有成员的新窗口一起建、一起显示、一起销毁旧的。
    // （逐个 rebuild 会让用户看到「一个一个挪进去」，见 rebuild_group_flashless）
    rebuild_group_flashless(app, &plan);
}

/// 跟随线程的会话代号：新的一次拖动自增，旧线程看到代号变了就自行退出。
static GROUP_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 拖动开始前调一次：把整团的偏移算好，并起一个**后台线程**负责跟手。
///
/// 必须**在** `start_dragging()` 之前完成（那个调用会进系统拖动循环，之后前端
/// 再发指令也进不来了）。不是团成员就返回 false，前端照常单拖。
///
/// 为什么跟手放在独立线程里，而不是让前端每次 `onMoved` 发一次 invoke：
/// 拖动期间主线程待在系统的窗口拖动模态循环里，这时从别处 `SetWindowPos` 到
/// 主线程的窗口（不带 `SWP_ASYNCWINDOWPOS`）要 SendMessage 等它派发 —— 模态循环
/// 只派发它关心的那几个窗口，请求永远等不到回应，整条链路吊死（用户实测：
/// 「组合为胶囊团后一拖动就卡死」）。线程里只做 `GetWindowRect` + 异步 `SetWindowPos`，
/// 既不碰 Tauri 的窗口表，也不发 IPC。
#[tauri::command]
async fn group_drag_begin(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<bool, String> {
    // 上一次拖动留下的「光标逃逸」标记不作数
    DRAG_ESCAPED.lock().unwrap().retain(|x| x != &id);
    let bar = taskbar_rect();
    let bar_size = bar_box_size();
    // (id, dx, dy) + 真正要摆的窗口
    let mut members: Vec<(String, i32, i32)> = Vec::new();
    let mut chips: Vec<FollowChip> = Vec::new();
    {
        let cfg = state.cfg.lock().unwrap();
        let me = cfg.boxes.iter().find(|b| b.id == id).ok_or("盒子不存在")?;
        let Some(gid) = me.group.clone() else {
            *state.group_drag.lock().unwrap() = None;
            return Ok(false);
        };
        let ar = live_rect(&app, me);
        let ah = box_window(&app, &id).map(|w| hwnd_of(&w)).unwrap_or(0);
        if ah != 0 {
            for b in cfg.boxes.iter().filter(|b| b.id != id && b.group.as_deref() == Some(gid.as_str())) {
                let r = live_rect(&app, b);
                members.push((b.id.clone(), r.x - ar.x, r.y - ar.y));
                // 窗口没了（Explorer 重启的僵尸）就先记下偏移，等收尾时按配置补位；
                // 拖动途中没有 hwnd 可摆，跳过即可
                if let Some(w) = box_window(&app, &b.id) {
                    let h = hwnd_of(&w);
                    if h != 0 {
                        chips.push(FollowChip {
                            hwnd: h,
                            dx: r.x - ar.x,
                            dy: r.y - ar.y,
                            off: desktop::place_offset(h),
                            in_bar: b.in_bar,
                        });
                    }
                }
            }
            if !members.is_empty() {
                let gen = GROUP_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *state.group_drag.lock().unwrap() = Some(GroupDrag {
                    anchor: id.clone(),
                    anchor_hwnd: ah,
                    members: members.clone(),
                    chips,
                    bar,
                    bar_size,
                    gen,
                });
                spawn_group_follow(app.clone(), gen);
                return Ok(true);
            }
        }
    }
    *state.group_drag.lock().unwrap() = None;
    Ok(false)
}

/// 拖动途中的跟手线程：每 8ms 读一次锚点窗口的真实位置，把成员摆到「锚点 + 偏移」。
///
/// 全程只用 Win32（`GetWindowRect` / 异步 `SetWindowPos`），不查 Tauri 的窗口表、
/// 不锁 `cfg`、不发 IPC —— 这些都会和拖动模态循环抢主线程。松手（左键抬起）或会话
/// 被收尾/替换后自行退出；真正的对齐与落盘由 `group_settle` 负责。
fn spawn_group_follow(app: AppHandle, gen: u64) {
    std::thread::spawn(move || {
        let state = app.state::<State>();
        // 会话在拖动期间是不变的，开局拷一份，循环里只核一下代号
        let gd = {
            let g = state.group_drag.lock().unwrap();
            match g.as_ref() {
                Some(s) if s.gen == gen => s.clone(),
                _ => return,
            }
        };
        loop {
            std::thread::sleep(std::time::Duration::from_millis(8));
            // 会话被收尾/替换了 → 立刻停手（此时窗口归 group_settle 摆）
            {
                let g = state.group_drag.lock().unwrap();
                if !matches!(g.as_ref(), Some(s) if s.gen == gen) {
                    return;
                }
            }
            let Some(ar) = desktop::window_rect(gd.anchor_hwnd) else {
                // 锚点窗口没了（Explorer 重启、被销毁）：会话作废，别再摆剩下的
                let mut g = state.group_drag.lock().unwrap();
                if matches!(g.as_ref(), Some(s) if s.gen == gen) {
                    *g = None;
                }
                return;
            };
            for c in &gd.chips {
                let (x, y) = if c.in_bar {
                    let Some(bar) = gd.bar else { continue };
                    let (ww, _) = gd.bar_size;
                    (
                        clamp_i(ar.x + c.dx, bar.x, (bar.right() - ww).max(bar.x)),
                        bar.y,
                    )
                } else {
                    (ar.x + c.dx, ar.y + c.dy)
                };
                // 屏幕坐标 → 父客户区坐标（偏移是开始拖时算的固定值）
                desktop::move_box_window_place(c.hwnd, x + c.off.0, y + c.off.1);
            }
            if !left_down() {
                // 松手了：剩下的对齐/落盘交给 group_settle（吸附那条路）或 group_end
                return;
            }
        }
    });
}

/// 拖动途中把成员摆一次（备用入口：前端不再逐帧调它，跟手走线程）。
#[tauri::command]
fn group_follow(app: AppHandle, state: tauri::State<'_, State>, id: String) -> bool {
    let gd = {
        let g = state.group_drag.lock().unwrap();
        match g.as_ref() {
            Some(s) if s.anchor == id => Some(s.clone()),
            _ => None,
        }
    };
    let Some(gd) = gd else { return false };
    group_follow_with(&app, &state, &gd, false)
}

/// 「这次拖动没走吸附」的兜底。
///
/// 不能立刻收尾：松手后 100~400ms 才轮到 `snap_box` / `snap_bar`，而它们会先修正
/// **锚点**的位置再调 `group_settle`。这里要是马上收尾，就会拿一个还没吸附的锚点位置
/// 去对齐成员、把会话清掉，等锚点真跳过去时成员已经不在团里了（表现：吸好了但
/// 整团散开一截）。所以延后 600ms：正常路径早就收过尾（会话没了 → 空操作），
/// 只有「压根没吸附」时才轮到它。
#[tauri::command]
fn group_end(app: AppHandle, state: tauri::State<'_, State>, id: String) {
    let gen = {
        let g = state.group_drag.lock().unwrap();
        match g.as_ref() {
            Some(s) if s.anchor == id => Some(s.gen),
            _ => None,
        }
    };
    let Some(gen) = gen else { return };
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        let state = app.state::<State>();
        // 600ms 内用户又拖了一次（会话换代）→ 别把新的那次提前收掉
        let same = {
            let g = state.group_drag.lock().unwrap();
            matches!(g.as_ref(), Some(s) if s.gen == gen)
        };
        if same {
            group_settle(&app, &state, &id);
        }
    });
}

/// 左 Ctrl 是否按着（物理键状态）。
///
/// 给面板的「Ctrl+点击多选」兜底：WebView2 在部分窗口状态下不给鼠标事件带
/// 修饰键，`e.ctrlKey` 会是 false，`Ctrl+点击` 就退化成「点一下 = 打开程序」。
/// 后端读的是系统级键状态，不受窗口焦点影响。
fn ctrl_down_raw() -> bool {
    let down = |vk: i32| unsafe { (win32::GetAsyncKeyState(vk) as u16) & 0x8000 != 0 };
    // VK_CONTROL(0x11) 覆盖左右两个 Ctrl；顺带认 Windows 键（习惯用 Win 的少见，但无害）
    down(0x11) || down(0x5B)
}

#[tauri::command]
fn ctrl_down() -> bool {
    ctrl_down_raw()
}

/// 把 Ctrl 的物理键状态**主动推**给前端（只在变化时发，40ms 一轮）。
///
/// 为什么不能只靠前端自己记：面板的多选要在 `pointerdown` 那一刻**同步**决定
/// 「打开」还是「切选」，等一下 IPC 就来不及了。而 WebView2 在部分窗口状态下
/// 鼠标事件不带修饰键（`e.ctrlKey` 恒 false），窗口没焦点时又收不到 keydown/keyup。
/// 这里补上第三路：后端轮询系统级键状态并广播，前端拿到的就是一个随时可同步读的镜像。
///
/// 开销：每 40ms 两次 `GetAsyncKeyState`，只在状态翻转时发一次事件。
fn start_ctrl_watch(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = ctrl_down_raw();
        let _ = app.emit("ctrl:changed", last);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(40));
            let now = ctrl_down_raw();
            if now != last {
                last = now;
                let _ = app.emit("ctrl:changed", now);
            }
        }
    });
}

/// 松手后按「是否压在任务栏上」决定要不要**自动嵌入任务栏**。
///
/// 判据和拖动吸附一致：下边界越进任务栏的深度超过厚度的一半 → 认定用户想把它
/// 放进任务栏，于是切成任务栏的子窗口（`in_bar = true`）。
/// 反过来（从任务栏拖回桌面）由 `snap_bar` 处理。
///
/// 换层级只能重建窗口，而重建要几百毫秒，不能卡在 `snap_box` 里（前端还等着返回值），
/// 所以丢到后台线程；没命中条件时线程立刻退出，不会白重建。
fn embed_after_snap(app: &AppHandle, id: String) {
    let app = app.clone();
    std::thread::spawn(move || {
        // 团成员：整团一起嵌进去。判据和单个盒子一致，只是位置按锚点算，
        // 落位交给 `group_switch_mode` 排成一整排（松手后调用，此时没有拖动循环）
        let (grouped, anchor) = {
            let st = app.state::<State>();
            let cfg = st.cfg.lock().unwrap();
            let grouped = group_members_of(&cfg, &id).is_some();
            let anchor = cfg.boxes.iter().find(|b| b.id == id).map(|b| {
                let size = (BOX_W
                    * desktop::scale_at(b.x + BOX_W as i32 / 2, b.y + BOX_W as i32 / 2))
                .round() as i32;
                (b.x, b.y, size)
            });
            (grouped, anchor)
        };
        if grouped {
            if let Some((bx, by, size)) = anchor {
                if on_taskbar(bx, by, size) {
                    group_switch_mode(&app, &id, true, bx + size / 2, by + size / 2, false);
                }
            }
            return;
        }
        let Some(_guard) = lock_relay(8000) else { return };
        let _sg = switching_begin(&id);
        let st = app.state::<State>();
        let b = {
            let mut cfg = st.cfg.lock().unwrap();
            // 先收集已在任务栏里的胶囊横坐标（借用在下面取 b 之前就要结束）
            let others: Vec<i32> = cfg
                .boxes
                .iter()
                .filter(|o| o.in_bar && o.id != id)
                .map(|o| o.x)
                .collect();
            let b = match cfg.boxes.iter_mut().find(|b| b.id == id) {
                Some(b) => b,
                None => return,
            };
            // 已经嵌进任务栏的盒子不走这条路（它本来就是任务栏的子窗口）
            if b.in_bar {
                return;
            }
            let size = (BOX_W * desktop::scale_at(b.x + BOX_W as i32 / 2, b.y + BOX_W as i32 / 2))
                .round() as i32;
            if !on_taskbar(b.x, b.y, size) {
                return;
            }
            // 任务栏不支持（自动隐藏 / 侧边）就别强行嵌，盒子留在原地当普通盒子
            let bar = match taskbar_rect() {
                Some(r) => r,
                None => return,
            };
            // 落位规则和任务栏里拖动松手完全一致：优先贴着邻居，压到别人身上
            // 就挪到**离自己最近**的空位 —— 不能覆盖已有的胶囊。
            let (want_w, _) = bar_box_size();
            let snap =
                (SNAP_DIST * desktop::scale_at(b.x + size / 2, bar.y + bar.h / 2)).round() as i32;
            let x = bar_free_x(bar, want_w, b.x + size / 2 - want_w / 2, &others, snap);
            b.x = x;
            b.y = bar.y;
            b.in_bar = true;
            // 置顶的盒子一样可以嵌：层级由「任务栏子窗口」决定，置顶位没有意义，
            // 顺手清掉 —— 否则移出任务栏后它会莫名其妙还是浮在最上层。
            b.topmost = false;
            let b = b.clone();
            // 形态变了，脱离原来的团（见 set_box_in_bar 的同款处理）
            drop_from_group(&mut cfg, &id);
            store::save(&cfg);
            b
        };
        if let Err(e) = rebuild_box_flashless(&app, &b) {
            eprintln!("自动嵌入任务栏失败：{e}");
        }
        notify_box_changed(&app, &id);
    });
}

/// 「从任务栏拖回桌面」：把任务栏胶囊还原成普通盒子。
///
/// `cx/cy` 是用户拖到的那一点（屏幕坐标，物理像素）：松手时给窗口中心，
/// 拖动途中给光标位置 —— 位置按它夹进工作区，所以拖多少就落多少，不会跳到别处。
/// 只能重建窗口，丢后台线程做。
fn unembed_at(app: &AppHandle, id: String, cx: i32, cy: i32) {
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(_guard) = lock_relay(8000) else { return };
        // 从现在起这个盒子的坐标/层级都在变，snap / move_box 一律跳过
        let _sg = switching_begin(&id);
        let st = app.state::<State>();
        let b = {
            let mut cfg = st.cfg.lock().unwrap();
            let b = match cfg.boxes.iter_mut().find(|b| b.id == id) {
                Some(b) => b,
                None => return,
            };
            if !b.in_bar {
                return;
            }
            b.in_bar = false;
            // 按「拖到的那块屏」算尺寸，新位置整个夹进工作区（不压任务栏）
            if let Some(m) = desktop::monitor_at(cx, cy) {
                let size = (BOX_W * m.scale).round() as i32;
                b.x = clamp_i(cx - size / 2, m.work.x, (m.work.x + m.work.w - size).max(m.work.x));
                b.y = clamp_i(cy - size / 2, m.work.y, (m.work.y + m.work.h - size).max(m.work.y));
            }
            let b = b.clone();
            drop_from_group(&mut cfg, &id);
            store::save(&cfg);
            b
        };
        if let Err(e) = rebuild_box_flashless(&app, &b) {
            eprintln!("移出任务栏失败：{e}");
        }
        notify_box_changed(&app, &id);
    });
}

/// 启动时的一次性迁移：上一版「压在任务栏上」的盒子是**浮在顶层**的（靠 `auto_top`
/// 推导），现在这个位置代表「嵌入任务栏」。不迁移的话它退回桌面层会被任务栏盖住，
/// 看起来就像盒子丢了。手动置顶的盒子跳过（那是用户显式选的层级）。
fn migrate_pressed_into_bar(boxes: &mut [BoxConfig]) -> bool {
    let bar = match taskbar_rect() {
        Some(r) => r,
        None => return false,
    };
    let mut changed = false;
    // 已迁移出来的胶囊横坐标：后面迁的盒子要避开它们（不能叠在一起）
    let mut taken: Vec<i32> = boxes.iter().filter(|b| b.in_bar).map(|b| b.x).collect();
    let (want_w, _) = bar_box_size();
    for b in boxes.iter_mut() {
        if b.in_bar || b.topmost {
            continue;
        }
        let size = (BOX_W * desktop::scale_at(b.x + BOX_W as i32 / 2, b.y + BOX_W as i32 / 2))
            .round() as i32;
        if !on_taskbar(b.x, b.y, size) {
            continue;
        }
        let snap = (SNAP_DIST * desktop::scale_at(b.x + size / 2, bar.y + bar.h / 2)).round() as i32;
        let x = bar_free_x(bar, want_w, b.x + size / 2 - want_w / 2, &taken, snap);
        taken.push(x);
        b.x = x;
        b.y = bar.y;
        b.in_bar = true;
        changed = true;
    }
    changed
}

// ---------------- 拖动途中的悬停切换（不用松手） ----------------
//
// 用户要的两件事：
//  ① 桌面上的盒子拖进任务栏、过半之后**停住 HOVER_MS**，就直接嵌入，不用等松手；
//  ② 任务栏里的胶囊往桌面方向顶出去、顶住 HOVER_MS，就直接退回桌面。
//
// 为什么判断依据是**光标**而不是窗口矩形：胶囊是 `Shell_TrayWnd` 的子窗口，系统拖动
// 循环把它限制在任务栏那一条里 —— 窗口矩形永远出不去，光看它永远看不出「用户想拖出去」。
// 光标不受这个限制，一直往上走就会离开任务栏上沿，那才是「在使劲往外拖」的信号。

/// 悬停多久才切换（毫秒）。太短会误触（一扫而过就变了），太长用户觉得没反应。
///
/// 最初定的是 500ms，实测「延迟感有点大」→ 改成 300ms（用户要求试效果）。
/// 拖入和拖出共用这一个值，要调就只改这里。
const HOVER_MS: u128 = 300;
/// 光标离任务栏上沿多少像素以内算「顶到上沿了」（防手抖，也给被裁剪的光标留余量）
const BAR_UP_MARGIN: i32 = 2;
/// 桌面盒子判定时，光标允许比任务栏上沿高多少（防止「盒子压着任务栏、光标却在屏幕上」误判）
const BAR_UP_MIN: i32 = 8;

/// 一次拖动期间的悬停状态。`since` = 「条件从这一刻起一直成立」，条件一断就清空。
struct HoverState {
    id: String,
    since: Option<std::time::Instant>,
}

static HOVER: Mutex<Option<HoverState>> = Mutex::new(None);

/// 推进「同一条悬停条件已经持续多久」，到了 `HOVER_MS` 就返回 true。
///
/// `key` 区分是谁在悬停：单个盒子用盒子 id，胶囊团用 `group:<团id>` ——
/// 团里哪个成员在轮询都算同一件事，不然成员之间会互相把计时清掉。
/// 条件一断就清空计时（光标一荡出去，计时从头算）。
fn hover_reached(key: &str, cond: bool) -> bool {
    let mut hover = HOVER.lock().unwrap();
    match hover.as_ref() {
        Some(h) if h.id == key => {}
        _ => {
            *hover = Some(HoverState {
                id: key.to_string(),
                since: None,
            });
        }
    }
    if !cond {
        if let Some(h) = hover.as_mut() {
            h.since = None;
        }
        return false;
    }
    let since = match hover.as_ref().and_then(|h| h.since) {
        Some(t) => t,
        None => {
            if let Some(h) = hover.as_mut() {
                h.since = Some(std::time::Instant::now());
            }
            return false;
        }
    };
    if since.elapsed().as_millis() < HOVER_MS {
        return false;
    }
    *hover = None;
    true
}

/// 拖动期间「光标真的离开过任务栏上沿」的盒子。
///
/// 只在 `bar_hover` 的拖动轮询里写入 —— 那是唯一能确定「用户正用手拖它」的时刻，
/// 程序性摆窗口（改设置、重建、任务栏纠偏）不会污染它。
///
/// 用途：胶囊团的**整团**退出任务栏要等松手（拖动途中切形态会卡死），
/// 而 `snap_bar` 里原来那套「光标在任务栏上方、且离胶囊不远」的判据范围很窄
/// （只容得下往上顶一小段）；用户把团拖到屏幕中间时早就超了，会被误判成
/// 「只是在任务栏里挪一下」。所以在拖动途中先把信号记下来，松手时直接认。
static DRAG_ESCAPED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn cursor_xy() -> Option<(i32, i32)> {
    let mut pt = win32::POINT::default();
    if unsafe { win32::GetCursorPos(&mut pt) } == 0 {
        return None;
    }
    Some((pt.x, pt.y))
}

fn left_down() -> bool {
    (unsafe { win32::GetAsyncKeyState(0x01) } as u16) & 0x8000 != 0
}

/// 结束这个窗口上正在进行的系统拖动。
///
/// 拖动是 `SendMessage(WM_NCLBUTTONDOWN, HTCAPTION)` 进去的模态循环，它占着主线程 ——
/// 不结束它就没法重建窗口（新建窗口、销毁旧窗口都要主线程）。
/// `WM_CANCELMODE` 是取消模态状态的正规做法；少数情况下它不起作用，再补一个伪造的
/// `WM_LBUTTONUP` 把循环顶出来（真按钮还按着，但循环只认消息）。
///
/// 返回是否确认结束了。返回 false 时调用方**不要**动窗口：宁可这次不切形态，
/// 等用户松手走原来的吸附逻辑，也不能在拖动循环里拆窗口（会崩）。
fn end_drag(hwnd: isize) -> bool {
    if hwnd == 0 || unsafe { win32::IsWindow(hwnd) } == 0 {
        return true;
    }
    // 「还在拖吗」只能问窗口所在线程的 GUI 状态：GetCapture() 是按调用线程算的，
    // 我们这轮询命令跑在别的线程上，直接查永远是 0。
    let in_movesize = || -> bool {
        let tid = unsafe { win32::GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
        if tid == 0 {
            return false;
        }
        let mut gi = win32::GUITHREADINFO {
            cbSize: std::mem::size_of::<win32::GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if unsafe { win32::GetGUIThreadInfo(tid, &mut gi) } == 0 {
            return false;
        }
        // 三个信号取「或」：系统拖动/缩放循环进行时 `hwndMoveSize` 就是被拖的窗口，
        // 这比 flags 更直接；`hwndCapture` 是拖动循环必有的鼠标捕获。
        // 原来只看 GUI_INMOVESIZE，个别环境拿不到这一位 → 误判「已经结束」，
        // 于是主线程还卡在拖动循环里就去建新窗口，整个程序跟着一起卡死（用户实测）。
        gi.flags & win32::GUI_INMOVESIZE != 0
            || gi.hwndMoveSize != 0
            || (gi.hwndCapture != 0 && gi.hwndCapture == hwnd)
    };
    let wait = |ms: u64| {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < until {
            if !in_movesize() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        !in_movesize()
    };
    if !left_down() {
        return true;
    }
    let moving = in_movesize();
    unsafe { win32::PostMessageW(hwnd, win32::WM_CANCELMODE, 0, 0) };
    if !moving {
        // 查不到「正在拖动」的标志（个别环境 GetGUIThreadInfo 不给 GUI_INMOVESIZE）：
        // 不能死等，但也别立刻动窗口 —— 给消息队列一拍再说。
        std::thread::sleep(std::time::Duration::from_millis(80));
        return true;
    }
    if wait(200) {
        // 循环结束到主线程回到消息泵还有一点点时间，给一拍再动窗口
        std::thread::sleep(std::time::Duration::from_millis(40));
        return true;
    }
    unsafe { win32::PostMessageW(hwnd, win32::WM_LBUTTONUP, 0, 0) };
    if wait(300) {
        std::thread::sleep(std::time::Duration::from_millis(40));
        return true;
    }
    false
}

/// 拖动途中就切换形态：先结束拖动，再按新状态重建窗口（重建在后台线程里做）。
///
/// `cx/cy` 是触发那一刻的光标位置 —— 用户看着哪儿就落到哪儿，比用窗口坐标准
/// （胶囊被任务栏夹住时，窗口坐标其实还在任务栏里）。
fn switch_during_drag(app: &AppHandle, id: &str, to_bar: bool, cx: i32, cy: i32) -> bool {
    let win = match box_window(app, id) {
        Some(w) => w,
        None => return false,
    };
    if !end_drag(hwnd_of(&win)) {
        return false;
    }
    let app = app.clone();
    let id = id.to_string();
    std::thread::spawn(move || {
        let Some(_guard) = lock_relay(8000) else { return };
        // 拖动途中切换：坐标/层级都在变，这一段时间的 snap / move_box 必须让开
        let _sg = switching_begin(&id);
        let st = app.state::<State>();
        let b = {
            let mut cfg = st.cfg.lock().unwrap();
            let others: Vec<i32> = cfg
                .boxes
                .iter()
                .filter(|o| o.in_bar && o.id != id)
                .map(|o| o.x)
                .collect();
            let b = match cfg.boxes.iter_mut().find(|b2| b2.id == id) {
                Some(b) => b,
                None => return,
            };
            if b.in_bar == to_bar {
                return;
            }
            if to_bar {
                let bar = match taskbar_rect() {
                    Some(r) => r,
                    None => return,
                };
                // 横向落在光标处（压到已有胶囊就挪到最近的空位）；
                // 置顶对任务栏子窗口没意义，顺手清掉
                let (want_w, _) = bar_box_size();
                let snap = (SNAP_DIST * desktop::scale_at(cx, bar.y + bar.h / 2)).round() as i32;
                let x = bar_free_x(bar, want_w, cx - want_w / 2, &others, snap);
                b.x = x;
                b.y = bar.y;
                b.in_bar = true;
                b.topmost = false;
            } else {
                // 退回桌面：以光标为中心摆一个普通盒子，整个夹进工作区（不压任务栏）
                b.in_bar = false;
                if let Some(m) = desktop::monitor_at(cx, cy) {
                    let size = (BOX_W * m.scale).round() as i32;
                    b.x = clamp_i(cx - size / 2, m.work.x, (m.work.x + m.work.w - size).max(m.work.x));
                    b.y = clamp_i(cy - size / 2, m.work.y, (m.work.y + m.work.h - size).max(m.work.y));
                }
            }
            let b = b.clone();
            drop_from_group(&mut cfg, &id);
            store::save(&cfg);
            b
        };
        if let Err(e) = rebuild_box_flashless(&app, &b) {
            eprintln!("拖动中切换任务栏形态失败：{e}");
        }
        notify_box_changed(&app, &id);
    });
    true
}

/// 拖动途中的轮询：前端每 60ms 问一次「该不该切形态」。
///
/// 返回值（字符串，前端据此决定还要不要继续问）：
///  - `None`         → 继续轮询
///  - `"idle"`       → 这次拖动结束了 / 不归本逻辑管，停止轮询
///  - `"embed"`      → 已经决定嵌入，窗口正在重建
///  - `"unembed"`    → 已经决定退回桌面，窗口正在重建
///
/// 必须是 `async fn`：系统拖动循环占着主线程，同步命令排不上队（永远进不来）。
#[tauri::command]
async fn bar_hover(
    app: AppHandle,
    state: tauri::State<'_, State>,
    id: String,
) -> Result<Option<String>, String> {
    let (cx, cy) = match cursor_xy() {
        Some(p) => p,
        None => return Ok(None),
    };
    // 松手了：清状态，告诉前端别再问了（剩下的交给 snap_box / snap_bar）
    if !left_down() {
        *HOVER.lock().unwrap() = None;
        return Ok(Some("idle".into()));
    }
    let in_bar = {
        let cfg = state.cfg.lock().unwrap();
        match cfg.boxes.iter().find(|b| b.id == id) {
            Some(b) => b.in_bar,
            None => return Ok(Some("idle".into())),
        }
    };
    let bar = match taskbar_rect() {
        Some(r) => r,
        None => {
            *HOVER.lock().unwrap() = None;
            return Ok(None);
        }
    };
    // 胶囊团：拖动途中**也**允许「悬停 HOVER_MS → 整团换形态」。
    //
    // 单盒子的做法是 `switch_during_drag`（结束拖动循环 → 后台重建）。团多一步：
    // 跟随线程正在按 8ms 的节奏摆成员窗口，必须**先让它收手**再拆窗口，
    // 否则就是在别人手里抢椅子 —— 之前那次「拖出去一个、队友留着、接着卡死」
    // 就是这么来的（当时干脆禁止了拖动途中切形态，只能等松手）。
    if let Some(gid) = {
        let cfg = state.cfg.lock().unwrap();
        cfg.boxes
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| b.group.clone())
    } {
        if in_bar {
            // 胶囊是 `Shell_TrayWnd` 的子窗口，系统的拖动循环会把光标**裁剪**在任务栏里。
            // 单个胶囊在下面的 cond 分支里会把裁剪解掉；团成员走的是这条分支，
            // 漏了这一句就会「整团拖不出任务栏，鼠标被卡在任务栏那一条上」。
            unsafe { win32::ClipCursor(std::ptr::null()) };
        }
        if in_bar && cy < bar.y - BAR_UP_MIN {
            // 松手兜底用：光标真的离开过任务栏上沿（`snap_bar` 认这个标记）
            let mut v = DRAG_ESCAPED.lock().unwrap();
            if !v.iter().any(|x| x == &id) {
                v.push(id.clone());
            }
        }
        let br = box_window(&app, &id).and_then(|w| rect_of(&w));
        let cond = if in_bar {
            cy <= bar.y + BAR_UP_MARGIN
        } else {
            match br {
                Some(r) => on_taskbar(r.x, r.y, r.h.max(1)) && cy >= bar.y - BAR_UP_MIN,
                None => false,
            }
        };
        if !hover_reached(&format!("group:{gid}"), cond) {
            return Ok(None);
        }
        // 计时到了，可以切。顺序是死的：先停跟随线程 → 再结束拖动循环 → 才动窗口。
        *state.group_drag.lock().unwrap() = None;
        // 跟随线程 8ms 一轮，25ms 足够它看见会话没了并自行退出
        std::thread::sleep(std::time::Duration::from_millis(25));
        let Some(w) = box_window(&app, &id) else {
            return Ok(Some("idle".into()));
        };
        if !end_drag(hwnd_of(&w)) {
            // 甩不掉拖动循环就什么都不做：宁可等用户松手走 `snap_bar` 的整团逻辑，
            // 也不能在模态循环里拆窗口（那才是真的卡死）。
            return Ok(Some("idle".into()));
        }
        // 形态这次就变完了，松手时别再让 `snap_bar` 拿「逃逸」再切一遍
        DRAG_ESCAPED.lock().unwrap().retain(|x| x != &id);
        group_switch_mode(&app, &id, !in_bar, cx, cy, false);
        return Ok(Some(if in_bar { "unembed" } else { "embed" }.into()));
    }
    // 拖动时的真实矩形（配置里的坐标在拖动途中是旧的）
    let br = box_window(&app, &id).and_then(|w| rect_of(&w));

    let cond = if in_bar {
        // 胶囊：光标顶到任务栏上沿（或更高）就算「要拖出去」。
        //
        // 这里**不能**要求「比刚抓住时更靠上」：用户把胶囊往上一推只要几十毫秒，
        // 第一次轮询（60ms 后）时光标可能已经在最上面了 —— 那样差值恒为 0，
        // 永远等不到切换（就是「第 5 条没反应」的原因）。
        //
        // 胶囊是 `Shell_TrayWnd` 的子窗口，系统拖动循环会把光标裁剪在任务栏里，
        // 所以下面顺手把裁剪解掉（解不掉也能靠「贴上沿」判定）。
        unsafe { win32::ClipCursor(std::ptr::null()) };
        cy <= bar.y + BAR_UP_MARGIN
    } else {
        // 桌面盒子：下边界越进任务栏超过一半，光标也在任务栏这一带
        match br {
            Some(r) => on_taskbar(r.x, r.y, r.h.max(1)) && cy >= bar.y - BAR_UP_MIN,
            None => false,
        }
    };

    if !hover_reached(&id, cond) {
        return Ok(None);
    }

    // 条件维持够了 HOVER_MS → 切换。切换成功才告诉前端「别再问了」，
    // 失败（拖动循环甩不掉）就当没发生过，让用户松手走原来的吸附逻辑。
    if switch_during_drag(&app, &id, !in_bar, cx, cy) {
        Ok(Some(if in_bar { "unembed" } else { "embed" }.into()))
    } else {
        Ok(Some("idle".into()))
    }
}

// ---------------- 弹窗 / 设置窗口的摆放 ----------------
//
// 规则（用户定义）：
//
//  * 面板：**绝不压住自己这个收纳盒**（压住了用户就点不到它了，也看不出是哪个盒子的面板）；
//      其他收纳盒「尽量」别压 —— 压到了只是不好看，不能为了躲它们把面板挤到屏外。
//      同档候选里一律按「**上 → 左 → 下 → 右**」挑（用户明确要求的顺序：
//      右下角的盒子往上开最自然，上方被邻居占了就往左边开）。
//
//  * 设置：图标在屏幕左半边 → 设置窗口开在图标**右侧**；在右半边 → 开在**左侧**。
//    两边都放不下（屏幕太窄）就退回图标上/下方。始终不盖住图标，也不出工作区。
//
// 坐标全是**物理像素**，和工作区、盒子窗口的位置保持同一套单位。

/// 一块矩形（物理像素）。盒子窗口、面板、工作区都用它。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    fn right(&self) -> i32 {
        self.x + self.w
    }
    fn bottom(&self) -> i32 {
        self.y + self.h
    }
    /// 两块矩形是否相交（贴边不算）
    fn hits(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    fn hits_any(&self, others: &[Rect]) -> bool {
        others.iter().any(|o| self.hits(o))
    }
    fn inside(&self, area: &Rect, margin: i32) -> bool {
        self.x >= area.x + margin
            && self.y >= area.y + margin
            && self.right() <= area.right() - margin
            && self.bottom() <= area.bottom() - margin
    }
}

fn clamp_i(v: i32, lo: i32, hi: i32) -> i32 {
    if hi < lo {
        lo
    } else {
        v.max(lo).min(hi)
    }
}

/// 某个屏幕坐标所在显示器的工作区（已排除任务栏），物理像素。
///
/// 用「按点找显示器」而不是 `window.current_monitor()`：盒子窗口挂在 Progman 下面，
/// 拿不到窗口所属显示器时退回主屏会把面板甩到别的屏幕上去；按图标中心点找更稳。
/// 点所在的显示器工作区（物理像素，已排除任务栏）。
///
/// 用 Win32 直接问显示器，不用 tauri 的 `monitor_from_point`：混合缩放的多屏下
/// 后者依赖 tao 的坐标换算，实测会落到错误的那块屏上。
fn work_area_at(_app: &AppHandle, x: i32, y: i32) -> Rect {
    if let Some(m) = desktop::monitor_at(x, y) {
        return m.work;
    }
    // 显示器都问不出来（几乎不可能）：退回 1080p 的默认值
    Rect { x: 0, y: 0, w: 1920, h: 1080 }
}

/// 窗口在屏幕坐标下的矩形（物理像素）。
///
/// **必须走 Win32**：盒子挂在 Progman 下面之后，tao 的 `outer_position()` /
/// `onMoved` 拿到的是父客户区坐标（WM_MOVE 的 lParam），和屏幕坐标差一整块屏。
fn rect_of(win: &tauri::WebviewWindow) -> Option<Rect> {
    desktop::window_rect(hwnd_of(win))
}

/// 收纳盒的排布方向
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arrangement {
    /// 横排：有邻居和我在同一水平线上
    Row,
    /// 竖排：有邻居和我在同一垂直线上
    Column,
    /// 杂乱（或只有一个盒子）
    Mixed,
}

/// 判断 id 这个盒子所处的排布。
///
/// tol 是「算同一行/同一列」的容差（盒子边长的 55% 左右）：盒子之间实际有 24px 间隙，
/// 横排的 dy 是 0，容差只要比 0 大就行；留一半边长是为了容忍手抖拖动造成的几像素偏差。
fn arrangement_of(boxes: &[BoxConfig], id: &str, tol: i32) -> Arrangement {
    let me = match boxes.iter().find(|b| b.id == id) {
        Some(b) => b,
        None => return Arrangement::Mixed,
    };
    let (mut row, mut col) = (0, 0);
    for b in boxes {
        if b.id == me.id {
            continue;
        }
        let dx = (b.x - me.x).abs();
        let dy = (b.y - me.y).abs();
        // 几乎重合的（历史脏数据）既不算行也不算列
        if dx <= tol && dy <= tol {
            continue;
        }
        if dy <= tol {
            row += 1;
        } else if dx <= tol {
            col += 1;
        }
    }
    if row > col {
        Arrangement::Row
    } else if col > row {
        Arrangement::Column
    } else {
        Arrangement::Mixed
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Up,
    Down,
    Left,
    Right,
}

/// 把窗口摆在 anchor 的某一侧：先算「理想位置」，再看它是否整体落在工作区里。
/// 返回的坐标一定在工作区内（放不下就贴边），fits 表示理想位置本来就在区内。
fn spot_beside(side: Side, anchor: &Rect, w: i32, h: i32, gap: i32, area: &Rect, margin: i32) -> (Rect, bool) {
    let (raw_x, raw_y) = match side {
        Side::Up => (anchor.x + anchor.w / 2 - w / 2, anchor.y - gap - h),
        Side::Down => (anchor.x + anchor.w / 2 - w / 2, anchor.y + anchor.h + gap),
        Side::Right => (anchor.x + anchor.w + gap, anchor.y + anchor.h / 2 - h / 2),
        Side::Left => (anchor.x - gap - w, anchor.y + anchor.h / 2 - h / 2),
    };
    let r = Rect {
        x: clamp_i(raw_x, area.x + margin, area.right() - margin - w),
        y: clamp_i(raw_y, area.y + margin, area.bottom() - margin - h),
        w,
        h,
    };
    let fits = Rect { x: raw_x, y: raw_y, w, h }.inside(area, margin);
    (r, fits)
}

/// 按方向优先级挑位置。
///
/// `hard` = 绝对不能压的（比如设置窗口不能压住图标），`soft` = 尽量别压的（比如正开着的面板）。
/// 优先级：
///  1. 理想位置就在屏内，且 hard / soft 都不压 —— 最好；
///  2. 理想位置就在屏内，不压 hard（soft 顾不上了）；
///  3. 退一步用夹到边上的位置，只要不压 hard；
///  4. 实在没得挑：理想位置哪怕压 hard 也认了；
///  5. 全都落空（窗口比屏幕还大之类）→ 第一个方向夹出来的位置。
/// 注意「这个方向有没有空间」必须排在「贴边硬塞」前面，
/// 否则「上方不够高」的盒子会被夹在顶部盖住自己，而不是老老实实掉到下方。
fn pick_spot(
    order: &[Side],
    anchor: &Rect,
    w: i32,
    h: i32,
    gap: i32,
    area: &Rect,
    margin: i32,
    hard: &[Rect],
    soft: &[Rect],
) -> Rect {
    let cands: Vec<(Rect, bool)> = order
        .iter()
        .map(|s| spot_beside(*s, anchor, w, h, gap, area, margin))
        .collect();
    let free_of = |r: &Rect, list: &[Rect]| !r.hits_any(list);
    if let Some((r, _)) = cands
        .iter()
        .find(|(r, f)| *f && free_of(r, hard) && free_of(r, soft))
    {
        return *r;
    }
    if let Some((r, _)) = cands.iter().find(|(r, f)| *f && free_of(r, hard)) {
        return *r;
    }
    if let Some((r, _)) = cands.iter().find(|(r, _)| free_of(r, hard)) {
        return *r;
    }
    if let Some((r, _)) = cands.iter().find(|(_, f)| *f) {
        return *r;
    }
    cands[0].0
}

/// 面板该摆哪儿。icon 是盒子图标窗口的矩形，others 是其他收纳盒的矩形。
///
/// 分三档挑，越靠前越好：
///  ① 自己和其他盒子都不压；
///  ② 只保证不压自己（其他盒子让一步 —— 用户说「其他的可以覆盖」）；
///  ③ 四个方向都躲不开自己（面板比可用空间还大的极端情况）→ 挑压得最少的。
///
/// 每档内部按 `order`（上 → 左 → 下 → 右）取第一个，所以「不压任何盒子」时
/// 一定是上方优先、其次左侧。
fn panel_origin(icon: &Rect, pw: i32, ph: i32, sc: f64, area: &Rect, layout: Arrangement, others: &[Rect]) -> Rect {
    let gap = (10.0 * sc).round() as i32;
    // 上、左优先是死规矩；排布方向只在「下」和「右」之间调一下顺序：
    // 竖排时同一个盒子上下的邻居最密，往右开比往下方开更不容易压到同列的盒子。
    let order: &[Side] = match layout {
        Arrangement::Column => &[Side::Up, Side::Left, Side::Right, Side::Down],
        _ => &[Side::Up, Side::Left, Side::Down, Side::Right],
    };
    let cands: Vec<Rect> = order
        .iter()
        .map(|s| spot_beside(*s, icon, pw, ph, gap, area, 8).0)
        .collect();

    if let Some(r) = cands.iter().find(|r| !r.hits(icon) && !r.hits_any(others)) {
        return *r;
    }
    if let Some(r) = cands.iter().find(|r| !r.hits(icon)) {
        return *r;
    }
    // 极端情况：宁可压住自己也不能跑到屏幕外 —— 那就压得最少的方向
    let overlap = |r: &Rect| -> i64 {
        let w = (r.right().min(icon.right()) - r.x.max(icon.x)).max(0) as i64;
        let h = (r.bottom().min(icon.bottom()) - r.y.max(icon.y)).max(0) as i64;
        w * h
    };
    cands
        .iter()
        .copied()
        .min_by_key(|r| overlap(r))
        .unwrap_or(cands[0])
}

/// 截图 + 模糊，存进 State 供前端 `panel_backdrop` 取用。
///
/// 必须在「目标位置上还没有我们自己的窗口」的时候截，否则会把自己的面板截进玻璃里。
/// 所以调用点在 `present_panel` 里 `hide` **之前**（此时面板还待在旧位置），
/// 只在切换毛玻璃开关时不需要重截 —— 打开面板时已经截好了。
fn capture_backdrop(app: &AppHandle, x: i32, y: i32, w: i32, h: i32) {
    let url = glass::backdrop_data_url(x, y, w, h);
    *app.state::<State>().backdrop.lock().unwrap() = url;
}

/// 显示面板窗口的唯一出口：显示 + 圆角 + 抢焦点。
///
/// 注意不要用 tao 的样式逻辑之外的东西去改窗口；这里沿用 strip_chrome（内部 SetWindowPos
/// 带 SWP_SHOWWINDOW），位置用物理像素，和外面算出来的一致。
fn show_panel(app: &AppHandle, panel: &tauri::WebviewWindow, x: i32, y: i32) {
    let _ = panel.show();
    let hwnd = hwnd_of(panel);
    let radius = panel_radius(app);
    desktop::strip_chrome(hwnd, x, y, PANEL_W as i32, PANEL_H as i32, radius, true);
    let _ = panel.set_skip_taskbar(true);
    let _ = panel.set_focus();
}

/// 面板显示的公共逻辑。
///
/// 两个关键点：
///  1. 如果面板**已经**在显示这个盒子，就只刷新内容 + 抢回焦点，不 hide/show、也不重设位置
///     —— 否则「点盒子」会先收起再弹出，界面闪一下。
///  2. 换成**另一个**盒子时，先把窗口藏起来，让前端把内容换成新盒子的、画完首帧后
///     带 seq 回报（`panel_ready`），后端对得上号才显示。否则复用同一个 webview 时，
///     用户会先看到上一个盒子的图标再刷新 —— 看起来像缓存了旧内容。
async fn present_panel(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<(), String> {
    let panel = match app.get_webview_window("panel") {
        Some(p) => p,
        None => spawn_panel(&app)?,
    };

    let same = state.panel_target.lock().unwrap().clone() == id;
    // 换盒子了：设置窗口如果正给上一个盒子做设置，就跟着收起来（否则会对着旧盒子改）
    if !same {
        let settings_open = state.settings_target.lock().unwrap().clone();
        if !settings_open.is_empty() && settings_open != id {
            if let Some(s) = app.get_webview_window("settings") {
                if s.is_visible().unwrap_or(false) {
                    let _ = s.hide();
                }
            }
        }
    }
    let seq = {
        let mut s = state.panel_seq.lock().unwrap();
        *s += 1;
        *s
    };
    *state.panel_target.lock().unwrap() = id.clone();

    if same && panel.is_visible().unwrap_or(false) {
        // 同一个盒子再点一次 = **收起**（用户要求的开关式交互：点一次弹出，再点一次关闭）。
        //
        // 必须把 seq 再递增一次，作废还在路上的 `panel_ready` —— 否则刚 hide 又被它
        // 显示回来（和 `close_panel` 同一个坑）。
        {
            let mut s = state.panel_seq.lock().unwrap();
            *s = seq + 1;
        }
        let _ = panel.hide();
        return Ok(());
    }

    let bw = box_window(&app, &id).ok_or("盒子窗口不存在")?;
    let br = rect_of(&bw).ok_or("读不到盒子位置")?;
    let (pos, sz) = (br, br);

    // 逻辑尺寸 → 物理尺寸：面板的宽高按**盒子所在显示器**的缩放换算。
    // 位置（pos/sz/工作区）本来就是物理像素，两边不能混着算，否则 125% 下会错位。
    // 这里必须按位置查显示器：盒子的窗口 DPI 会跟着 Progman（一直是主屏的）。
    let sc = desktop::scale_at(pos.x + sz.w / 2, pos.y + sz.h / 2);
    let pw = (PANEL_W * sc).round() as i32;
    let ph = (PANEL_H * sc).round() as i32;

    let icon = Rect {
        x: pos.x,
        y: pos.y,
        w: sz.w,
        h: sz.h,
    };
    let area = work_area_at(&app, icon.x + icon.w / 2, icon.y + icon.h / 2);
    let (layout, others) = {
        let cfg = state.cfg.lock().unwrap();
        let tol = (icon.w as f64 * 0.55).round() as i32;
        let layout = arrangement_of(&cfg.boxes, &id, tol);
        let others: Vec<Rect> = cfg
            .boxes
            .iter()
            .filter(|b| b.id != id)
            .map(|b| Rect {
                x: b.x,
                y: b.y,
                w: icon.w,
                h: icon.h,
            })
            .collect();
        (layout, others)
    };
    let spot = panel_origin(&icon, pw, ph, sc, &area, layout, &others);
    let (x, y) = (spot.x, spot.y);

    // 毛玻璃底图：必须**在动窗口之前**截。此刻面板还在旧位置，目标位置上是干净的桌面；
    // 如果反过来（先 hide 再截），hide 是投递到主线程的异步操作，很可能截到自己的面板。
    // 顺便这里也解释了「为什么每次打开面板都要截一张」：位置跟着盒子走，换了盒子就得换底图。
    capture_backdrop(&app, x, y, pw, ph);

    // 先藏起来：这一帧里还挂着上一个盒子的内容，露出来就是「闪一下旧图标」
    if panel.is_visible().unwrap_or(false) {
        let _ = panel.hide();
    }
    panel
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;

    // 通知前端换内容；它画完首帧后会调 panel_ready，这里才真正显示
    let _ = panel.emit("panel:target", PanelTarget { id: id.clone(), seq });

    // 兜底：前端出问题（没回报）也不能让面板永远不出现
    let app2 = app.clone();
    let tid = id.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let (seq_now, target_now) = {
            let st = app2.state::<State>();
            let seq_now = *st.panel_seq.lock().unwrap();
            let target_now = st.panel_target.lock().unwrap().clone();
            (seq_now, target_now)
        };
        if seq_now != seq || target_now != tid {
            return;
        }
        if let Some(p) = app2.get_webview_window("panel") {
            if !p.is_visible().unwrap_or(false) {
                show_panel(&app2, &p, x, y);
            }
        }
    });
    Ok(())
}

#[tauri::command]
async fn open_panel(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<(), String> {
    present_panel(app, state, id).await
}

/// 打开「设置」——独立窗口，不再叠在面板里。
///
/// 窗口是常驻的：第一次点开时创建，之后只做「定位 + 显示」，关掉只是 hide。
///
/// 摆放规则：以**盒子图标**为锚点，图标在屏幕左半边就把设置窗口开在它右侧，
/// 在右半边就开在左侧 —— 这样既不会盖住图标，也不会盖住图标周围那一列/一行收纳盒。
/// 左右都放不下（屏幕太窄、或者面板正占着那一侧）再退回图标上/下方。
/// 参数 height 是**逻辑**高度：窗口按内容自适应，高度一变就得重算一次。
fn settings_origin(app: &AppHandle, id: &str, height: f64) -> (i32, i32) {
    // 锚点：盒子图标窗口。找不到就退回主屏中间（几乎不会发生）
    let anchor = box_window(app, id).and_then(|bw| {
        let icon = rect_of(&bw)?;
        let area = work_area_at(app, icon.x + icon.w / 2, icon.y + icon.h / 2);
        // 按盒子位置所在显示器取缩放（盒子的窗口 DPI 不可信，见 desktop::scale_at）
        let sc = desktop::scale_at(icon.x + icon.w / 2, icon.y + icon.h / 2);
        Some((icon, area, sc))
    });

    let (icon, area, sc) = match anchor {
        Some(v) => v,
        None => {
            let sc = app_scale(app);
            let area = app
                .primary_monitor()
                .ok()
                .flatten()
                .map(|m| {
                    let wa = m.work_area();
                    Rect {
                        x: wa.position.x,
                        y: wa.position.y,
                        w: wa.size.width as i32,
                        h: wa.size.height as i32,
                    }
                })
                .unwrap_or(Rect { x: 0, y: 0, w: 1920, h: 1080 });
            let icon = Rect {
                x: area.x + area.w / 2,
                y: area.y + area.h / 2,
                w: 0,
                h: 0,
            };
            (icon, area, sc)
        }
    };

    let sw = (SET_W * sc).round() as i32;
    let sh = (height * sc).round() as i32;
    let gap = (12.0 * sc).round() as i32;

    // 图标在左半边 → 右侧；右半边 → 左侧
    let center = icon.x + icon.w / 2;
    let prefer_right = center < area.x + area.w / 2;
    let order: [Side; 4] = if prefer_right {
        [Side::Right, Side::Left, Side::Up, Side::Down]
    } else {
        [Side::Left, Side::Right, Side::Up, Side::Down]
    };

    // 硬约束：图标本身 —— 任何情况下都不能盖住它（盖住就点不到、右键菜单也弹不出）
    let hard = [icon];
    // 软约束：面板如果正开着（从面板菜单进的设置），尽量别把设置窗口压在它身上
    let soft: Vec<Rect> = app
        .get_webview_window("panel")
        .filter(|p| p.is_visible().unwrap_or(false))
        .and_then(|p| rect_of(&p))
        .into_iter()
        .collect();

    let spot = pick_spot(&order, &icon, sw, sh, gap, &area, 8, &hard, &soft);
    (spot.x, spot.y)
}

/// 按内容高度调整设置窗口。
///
/// 前端量完 `.dlg` 的实际高度后调这里；高度变了一次性把窗口尺寸和位置都重设
/// （位置重设是因为「图标左边/右边」的摆放位置跟窗口高度有关）。高度夹在 200~880 之间：
/// 再小的窗口连一行都放不下，再大的在 768 高的屏上会顶出去。
#[tauri::command]
async fn fit_settings(app: AppHandle, state: tauri::State<'_, State>, height: f64) -> Result<(), String> {
    let h = height.clamp(200.0, 880.0).round();
    // 全局设置窗口（托盘进的）优先：它Visible时按主屏居中重摆
    if let Some(w) = app.get_webview_window("gsettings") {
        if w.is_visible().unwrap_or(false) {
            *state.gsettings_height.lock().unwrap() = h;
            let (x, y) = gsettings_origin(&app, h);
            desktop::strip_chrome(hwnd_of(&w), x, y, SET_W as i32, h as i32, SET_RADIUS, true);
            let _ = w.set_skip_taskbar(true);
            return Ok(());
        }
    }
    let win = match app.get_webview_window("settings") {
        Some(w) => w,
        None => return Ok(()),
    };
    if !win.is_visible().unwrap_or(false) {
        return Ok(());
    }
    *state.settings_height.lock().unwrap() = h;
    let id = state.settings_target.lock().unwrap().clone();
    // 用户拖过窗口就别再按自动位置摆回去：先把「当前高度对应的自动位置」算出来，
    // 窗口正好在那儿（±6px）才认为是自动摆放的，可以跟着新高度重新定位。
    let (mut x, mut y) = settings_origin(&app, &id, h);
    if let (Ok(pos), Ok(sz)) = (win.outer_position(), win.outer_size()) {
        // 按设置窗口自身位置所在显示器取缩放
        let sc = desktop::scale_at(pos.x + sz.width as i32 / 2, pos.y + sz.height as i32 / 2);
        let (cx, cy) = settings_origin(&app, &id, sz.height as f64 / sc);
        let auto_placed = (pos.x - cx).abs() <= 6 && (pos.y - cy).abs() <= 6;
        if !auto_placed {
            x = pos.x;
            y = pos.y;
        }
    }
    desktop::strip_chrome(hwnd_of(&win), x, y, SET_W as i32, h as i32, SET_RADIUS, true);
    let _ = win.set_skip_taskbar(true);
    Ok(())
}

#[tauri::command]
async fn open_settings(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<(), String> {
    {
        let cfg = state.cfg.lock().unwrap();
        if !cfg.boxes.iter().any(|b| b.id == id) {
            return Err("盒子不存在".into());
        }
    }
    let win = match app.get_webview_window("settings") {
        Some(w) => w,
        None => spawn_settings(&app)?,
    };

    // 用上次量到的高度摆放：避免先按占位高度居中、再被 fit_settings 跳一下
    let h = *state.settings_height.lock().unwrap();
    let (x, y) = settings_origin(&app, &id, h);

    win.set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    win.show().map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), x, y, SET_W as i32, h as i32, SET_RADIUS, true);
    let _ = win.set_skip_taskbar(true);
    // 页面要重新拉一次这个盒子的数据（窗口是复用的，id 可能变了）
    *state.settings_target.lock().unwrap() = id.clone();
    let _ = win.emit("settings:target", id);
    let _ = win.set_focus();
    Ok(())
}

#[tauri::command]
async fn hide_settings(app: AppHandle) {
    for label in ["settings", "gsettings"] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.hide();
        }
    }
}

/// 设置窗口重载 / 首次加载时要知道「我在给哪个盒子做设置」
#[tauri::command]
fn get_settings_target(state: tauri::State<State>) -> Option<String> {
    let id = state.settings_target.lock().unwrap().clone();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// 前端换好内容、画完首帧后回报：这时才把面板显示出来（见 present_panel 注释 2）
#[tauri::command]
async fn panel_ready(
    app: AppHandle,
    state: tauri::State<'_, State>,
    id: String,
    seq: u64,
) -> Result<(), String> {
    let (seq_now, target_now) = {
        let seq_now = *state.panel_seq.lock().unwrap();
        let target_now = state.panel_target.lock().unwrap().clone();
        (seq_now, target_now)
    };
    if seq_now != seq || target_now != id {
        return Ok(()); // 已经切到别的盒子 / 面板被关掉了，这次回报作废
    }
    if let Some(p) = app.get_webview_window("panel") {
        if !p.is_visible().unwrap_or(false) {
            let pos = p.outer_position().unwrap_or(PhysicalPosition::new(0, 0));
            show_panel(&app, &p, pos.x, pos.y);
        }
    }
    Ok(())
}

/// 把图标拖出到桌面之后，面板会失去前台（鼠标交给了 OLE 拖拽）。
///
/// 之后用户点桌面时，面板**不会再收到一次**失焦事件 —— 它早就没焦点了。所以「点外面收起」
/// 就失效了。拖拽结束后主动把前台抢回来，后续点桌面才能照常触发失焦 → 收起。
#[tauri::command]
async fn refocus_panel(app: AppHandle) -> Result<(), String> {
    if let Some(p) = app.get_webview_window("panel") {
        if p.is_visible().unwrap_or(false) {
            let hwnd = hwnd_of(&p);
            if hwnd != 0 {
                unsafe {
                    win32::SetForegroundWindow(hwnd);
                }
            }
            let _ = p.set_focus();
        }
    }
    Ok(())
}

/// 外观主题：盒子背景色 / 面板背景色 / 毛玻璃 / 透明度。
///
/// 文字颜色（含图标名、标题、按钮）不在这里，前端按背景色的相对亮度自动挑深色或浅色，
/// 保证「字色永远不和背景撞色」。
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Theme {
    box_bg: String,
    panel_bg: String,
    acrylic: bool,
    /// 透明度百分比：0 = 不透明，50 = 最透。数字越大越透明。
    transparency: f32,
    /// 弹出面板是否画圆角（关掉 = 方形窗口，只为面板，不影响收纳盒/菜单）
    radius: bool,
    /// 嵌入任务栏时是否隐藏名称（打开后任务栏里只显示图标）。默认关闭，老配置没有这个键。
    #[serde(default)]
    bar_hide_label: bool,
}

fn theme_of(cfg: &Config) -> Theme {
    Theme {
        box_bg: cfg.box_bg.clone(),
        panel_bg: cfg.panel_bg.clone(),
        acrylic: cfg.panel_acrylic,
        transparency: cfg.panel_transparency.clamp(0.0, 50.0),
        radius: cfg.panel_radius,
        bar_hide_label: cfg.bar_hide_label,
    }
}

/// 规整成 `#RRGGBB`（大写）。前端 `<input type=color>` 给的就是这个格式，
/// 但配置文件是用户可改的，这里必须挡住乱写的值，否则 CSS 会整块失效。
fn norm_hex(s: &str) -> Option<String> {
    let t = s.trim().trim_start_matches('#');
    if t.len() != 6 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", t.to_ascii_uppercase()))
}

#[tauri::command]
fn get_theme(state: tauri::State<State>) -> Theme {
    theme_of(&state.cfg.lock().unwrap())
}

/// 改外观：写配置 → 立刻应用到面板窗口（毛玻璃要动 hwnd）→ 广播给所有窗口。
///
/// 广播很重要：盒子窗口、面板窗口是两个 webview，只改调用方那个的话，
/// 另一个会停在旧配色上（尤其是「点盒子弹出面板」时两边的观感必须一致）。
#[tauri::command]
async fn set_theme(app: AppHandle, state: tauri::State<'_, State>, theme: Theme) -> Result<(), String> {
    let box_bg = norm_hex(&theme.box_bg).ok_or("收纳盒背景色格式不正确")?;
    let panel_bg = norm_hex(&theme.panel_bg).ok_or("面板背景色格式不正确")?;
    let transparency = theme.transparency.clamp(0.0, 50.0);
    let acrylic = theme.acrylic;
    let radius = theme.radius;
    let hide_label = theme.bar_hide_label;
    // 「嵌入任务栏时隐藏名称」只改这一次时才需要重摆任务栏里的盒子（宽度变了）；
    // 其余外观改动（拖颜色滑块）每次都调本命令，不能顺带把盒子重摆一遍。
    let label_changed;
    let bar_ids: Vec<String>;
    let next = {
        let mut cfg = state.cfg.lock().unwrap();
        label_changed = cfg.bar_hide_label != hide_label;
        bar_ids = cfg.boxes.iter().filter(|b| b.in_bar).map(|b| b.id.clone()).collect();
        cfg.box_bg = box_bg;
        cfg.panel_bg = panel_bg;
        cfg.panel_acrylic = acrylic;
        cfg.panel_transparency = transparency;
        cfg.panel_radius = radius;
        cfg.bar_hide_label = hide_label;
        store::save(&cfg);
        theme_of(&cfg)
    };
    set_bar_hide_label(hide_label);
    // 圆角是窗口区域决定的（不只是 CSS），改完要立刻重设，不然要等下次开面板
    if let Some(p) = app.get_webview_window("panel") {
        if p.is_visible().unwrap_or(false) {
            desktop::reclip(
                hwnd_of(&p),
                PANEL_W as i32,
                PANEL_H as i32,
                if radius { PANEL_RADIUS } else { 0 },
            );
        }
    }
    let _ = app.emit("theme:changed", next);

    // 「隐藏名称」开关变了：任务栏里的盒子宽度跟着变（胶囊 ↔ 正方形），
    // 立刻按新宽度重摆一次，再让页面重新取一次 view（拿新的 w/h 重排版）。
    // 只在这一位真的变了时才做，拖颜色滑块不能触发下面这套动作。
    if label_changed && !bar_ids.is_empty() {
        reflow_boxes(&app);
        for id in &bar_ids {
            notify_box_changed(&app, id);
        }
    }

    // 底图不用重截：每次打开面板时都已经截好了，这里只是把现有那张再推给前端一次
    // （开关毛玻璃时前端要靠它把玻璃层填上，不用重新开面板）。
    // 特意不做 hide→截→show：那一下会让面板闪一下，还会被「失焦自动收起」误判。
    let url = app.state::<State>().backdrop.lock().unwrap().clone();
    let _ = app.emit("panel:backdrop", url);
    Ok(())
}

/// 取当前这张毛玻璃底图（没有 / 关了毛玻璃就返回 None）
#[tauri::command]
fn panel_backdrop(state: tauri::State<State>) -> Option<String> {
    state.backdrop.lock().unwrap().clone()
}

/// 面板刷新（F5 / 重载）后靠这个找回「我在显示哪个盒子」
#[tauri::command]
fn get_panel_target(state: tauri::State<State>) -> Option<PanelTarget> {
    let id = state.panel_target.lock().unwrap().clone();
    if id.is_empty() {
        None
    } else {
        let seq = *state.panel_seq.lock().unwrap();
        Some(PanelTarget { id, seq })
    }
}

/// 前台窗口是不是自己的（点收纳盒/菜单导致的失焦，不该收起面板）
#[tauri::command]
fn foreground_is_self() -> bool {
    desktop::foreground_is_self()
}

/// 左键鼠标是否按着。
///
/// 用于「桌面图标 → 面板」的拖拽判断：用户按住左键从桌面拖图标往面板走时，
/// 按下那一下面板就会失焦（前台给了桌面），如果立刻收起面板，图标就拖不进去了。
/// 这时不关，等松手再复判。
#[tauri::command]
fn mouse_left_down() -> bool {
    (unsafe { win32::GetAsyncKeyState(0x01) } as u16) & 0x8000 != 0
}

/// 光标是否落在面板窗口里（拖拽落点判断：松手时光标在面板内 = 刚拖进来，保持打开）。
#[tauri::command]
fn cursor_in_panel(app: AppHandle) -> bool {
    let win = match app.get_webview_window("panel") {
        Some(w) => w,
        None => return false,
    };
    let mut pt = win32::POINT::default();
    if unsafe { win32::GetCursorPos(&mut pt) } == 0 {
        return false;
    }
    let mut r = win32::RECT::default();
    if unsafe { win32::GetWindowRect(hwnd_of(&win) as win32::HWND, &mut r) } == 0 {
        return false;
    }
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

#[tauri::command]
fn cursor_on_box(app: AppHandle, state: tauri::State<'_, State>) -> bool {
    let mut pt = win32::POINT::default();
    if unsafe { win32::GetCursorPos(&mut pt) } == 0 {
        return false;
    }
    let ids: Vec<String> = {
        let cfg = state.cfg.lock().unwrap();
        cfg.boxes.iter().map(|b| b.id.clone()).collect()
    };
    for id in ids {
        let Some(w) = box_window(&app, &id) else { continue };
        if !w.is_visible().unwrap_or(false) {
            continue;
        }
        let Some(r) = rect_of(&w) else { continue };
        if pt.x >= r.x && pt.x < r.x + r.w && pt.y >= r.y && pt.y < r.y + r.h {
            return true;
        }
    }
    false
}

#[tauri::command]
async fn close_panel(app: AppHandle, state: tauri::State<'_, State>) -> Result<(), String> {
    // 作废还在路上的 panel_ready，避免「刚关掉又被它显示出来」
    {
        let mut s = state.panel_seq.lock().unwrap();
        *s += 1;
    }
    if let Some(p) = app.get_webview_window("panel") {
        let _ = p.hide();
    }
    // 设置窗口**不再**跟着收：它现在是独立窗口，用户点桌面只是想收起面板，
    // 正在改的设置不该一起消失（关设置要靠右上角的 ✕ 或 Esc）。
    Ok(())
}

/// 在光标处弹出盒子右键菜单
#[tauri::command]
async fn show_box_menu(app: AppHandle, state: tauri::State<'_, State>, id: String) -> Result<(), String> {
    let (name, topmost, in_bar, has_icon, grouped, can_group) = {
        let cfg = state.cfg.lock().unwrap();
        let b = cfg.boxes.iter().find(|b| b.id == id).ok_or("盒子不存在")?;
        let grouped = b.group.is_some();
        // 没成团时看周围有没有吸附在一起的邻居：至少两个才给「组合」这一项
        let can_group = !grouped && cluster_of(&app, &cfg, &id).len() >= 2;
        (
            b.name.clone(),
            is_floating(b),
            b.in_bar,
            b.icon.is_some(),
            grouped,
            can_group,
        )
    };
    let count = store::count_items(&id);
    let win = app.get_webview_window("menu").ok_or("菜单窗口不存在")?;

    let mut pt = win32::POINT::default();
    unsafe { win32::GetCursorPos(&mut pt) };
    *state.menu_pt.lock().unwrap() = (pt.x, pt.y);
    // 逻辑尺寸 → 物理尺寸（place_menu / set_size 用的都是物理像素）。
    // 按**光标所在显示器**换算：菜单跟着光标弹，光标在哪块屏就按哪块屏的缩放。
    let sc = desktop::scale_at(pt.x, pt.y);
    let h = menu_height(count > 0, has_icon, grouped || can_group);
    let (mw, mh) = ((MENU_W * sc).round() as i32, (h * sc).round() as i32);
    let (x, y) = place_menu(&app, pt.x, pt.y, mw, mh);

    let _ = win.set_size(PhysicalSize::new(mw as u32, mh as u32));
    win.set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    *state.menu_target.lock().unwrap() = id.clone();
    let _ = win.emit(
        "menu:data",
        MenuData { id, name, count, topmost, in_bar, has_icon, grouped, can_group },
    );
    win.show().map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), x, y, MENU_W as i32, h as i32, MENU_RADIUS, true);
    let _ = win.set_skip_taskbar(true);
    let _ = win.set_focus();
    // 菜单自己也在置顶区：盒子切成「置顶」后是 topmost 窗口，新建的 topmost 窗口
    // 会排到置顶区最前面，把菜单盖掉一角。两个都在置顶区时，前端切完置顶会
    // 再 set_focus 一次菜单，菜单就回到最上面（见 MenuView 的 toggleTop）。
    let _ = win.set_always_on_top(true);
    Ok(())
}

#[tauri::command]
async fn hide_menu(app: AppHandle) {
    if let Some(w) = app.get_webview_window("menu") {
        let _ = w.hide();
    }
}

/// 菜单窗口按**前端测量出来的真实内容高度**修正一次（首次显示用的是公式估值）。
///
/// 为什么需要：菜单条目是条件渲染的（自定义图标、非空提示、错误提示），
/// 公式估值一旦和实际不符，底部条目会被 `overflow:hidden` 裁掉——v0.9.11
/// 加了「全部图标移回桌面」就踩过这个坑（退出 LightBox 被裁得失联）。
/// 前端渲染后测准高度回报这里，从根上免维护。
#[tauri::command]
async fn fit_menu(app: AppHandle, state: tauri::State<'_, State>, height: f64) -> Result<(), String> {
    let win = match app.get_webview_window("menu") {
        Some(w) => w,
        None => return Ok(()),
    };
    if !win.is_visible().unwrap_or(false) {
        return Ok(());
    }
    // 按菜单弹出时的光标位置所在显示器换算（和 show_box_menu 保持一致）
    let (px, py) = *state.menu_pt.lock().unwrap();
    let sc = desktop::scale_at(px, py);
    let h = height.clamp(120.0, 760.0);
    let (mw, mh) = ((MENU_W * sc).round() as i32, (h * sc).round() as i32);
    let (x, y) = place_menu(&app, px, py, mw, mh);
    let _ = win.set_size(PhysicalSize::new(mw as u32, mh as u32));
    win.set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    desktop::strip_chrome(hwnd_of(&win), x, y, MENU_W as i32, h as i32, MENU_RADIUS, true);
    Ok(())
}

/// 菜单窗口启动时拉一次数据（避免 show 早于页面加载、错过 menu:data 事件）
#[tauri::command]
fn get_menu_data(app: AppHandle, state: tauri::State<State>) -> Option<MenuData> {
    let id = state.menu_target.lock().unwrap().clone();
    if id.is_empty() {
        return None;
    }
    let (name, topmost, in_bar, has_icon, grouped, can_group) = {
        let cfg = state.cfg.lock().unwrap();
        let b = cfg.boxes.iter().find(|b| b.id == id)?;
        let grouped = b.group.is_some();
        let can_group = !grouped && cluster_of(&app, &cfg, &id).len() >= 2;
        (
            b.name.clone(),
            is_floating(b),
            b.in_bar,
            b.icon.is_some(),
            grouped,
            can_group,
        )
    };
    Some(MenuData {
        count: store::count_items(&id),
        id,
        name,
        topmost,
        in_bar,
        has_icon,
        grouped,
        can_group,
    })
}

/// 一个最小的系统提示框（模态、置顶）。给「选错文件」这类场合用。
fn warn_box(owner: isize, text: &str) {
    let t = win32::wide(text);
    let cap = win32::wide("LightBox");
    unsafe {
        win32::MessageBoxW(
            owner,
            t.as_ptr(),
            cap.as_ptr(),
            win32::MB_OK | win32::MB_ICONWARNING | win32::MB_SETFOREGROUND | win32::MB_TOPMOST,
        );
    }
}

/// 给收纳盒挑一个自定义图标（只接受 .ico）。///
/// 流程：系统文件对话框（主线程、模态）→ 校验扩展名 → 用 `LoadImageW` 把 .ico 转成
/// 64×64 的 PNG data URL 存进配置。存 data URL 而不是存路径，是因为前端 `<img src>` 在
/// WebView2 里加载不了本地 `file://`（要用 asset 协议），而 data URL 直接就能画。
///
/// 返回：选中处理后的 data URL；用户取消返回 None。
#[tauri::command]
async fn pick_box_icon(
    app: AppHandle,
    state: tauri::State<'_, State>,
    id: String,
) -> Result<Option<String>, String> {
    // 盒子存在性先探一下：删掉的盒子不该再弹对话框
    if !state.cfg.lock().unwrap().boxes.iter().any(|b| b.id == id) {
        return Err("盒子不存在".into());
    }
    let owner = box_window(&app, &id).map(|w| hwnd_of(&w)).unwrap_or(0);

    // 文件对话框要在**主线程**跑（Explorer 风格要 shell 命名空间 / COM），
    // 命令本身在 runtime 线程，所以投递过去再用 channel 等结果。
    let (tx, rx) = std::sync::mpsc::channel::<Option<std::path::PathBuf>>();
    app.run_on_main_thread(move || {
        let picked = iconpick::pick_ico(owner, "选择收纳盒图标（.ico）");
        let _ = tx.send(picked);
    })
    .map_err(|e| e.to_string())?;
    let picked = rx
        .recv_timeout(std::time::Duration::from_secs(600))
        .map_err(|_| "选择图标超时".to_string())?;
    let Some(path) = picked else {
        return Ok(None); // 用户取消
    };

    // 第二道过滤：用户可以在对话框里手打文件名绕过过滤器。
    // 这类「选错了」只在当下需要告诉用户，而这时菜单已经收起来了 —— 直接用系统提示框，
    // 不依赖前端有没有打开的面板能弹 toast。
    if !path.is_file() {
        warn_box(owner, "文件不存在");
        return Ok(None);
    }
    if !iconpick::is_ico(&path) {
        warn_box(owner, "只支持 .ico 图标文件");
        return Ok(None);
    }
    let Some(url) = icons::ico_file_data_url(&path, 64) else {
        warn_box(owner, "这个 .ico 文件读不出图标（可能已损坏）");
        return Ok(None);
    };

    {
        let mut cfg = state.cfg.lock().unwrap();
        let b = cfg.boxes.iter_mut().find(|b| b.id == id).ok_or("盒子不存在")?;
        b.icon = Some(url.clone());
        store::save(&cfg);
    }
    notify_box_changed(&app, &id);
    Ok(Some(url))
}

/// 清掉自定义图标，回到内置的收纳盒图形
#[tauri::command]
fn clear_box_icon(app: AppHandle, state: tauri::State<State>, id: String) -> Result<(), String> {
    {
        let mut cfg = state.cfg.lock().unwrap();
        let b = cfg.boxes.iter_mut().find(|b| b.id == id).ok_or("盒子不存在")?;
        b.icon = None;
        store::save(&cfg);
    }
    notify_box_changed(&app, &id);
    Ok(())
}

#[tauri::command]
fn get_settings(_state: tauri::State<State>) -> AppSettings {
    AppSettings {
        autostart: autostart::enabled(),
        exe: std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        data_dir: store::root_dir().display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[tauri::command]
fn set_autostart(on: bool) -> Result<(), String> {
    autostart::set(on)
}

#[tauri::command]
fn open_data_dir() -> Result<(), String> {
    let dir = store::root_dir();
    if desktop::shell_open(&dir.to_string_lossy()) {
        Ok(())
    } else {
        Err("打开目录失败".into())
    }
}

// ---------------- 版本更新 ----------------

/// 当前更新状态（页面挂载时主动拉一次）
#[tauri::command]
fn update_state() -> updater::UpdateView {
    updater::snapshot()
}

/// 手动检查更新（托盘「检查更新…」/ 更新窗口里的「重试」）。
/// 网上请求是阻塞的，丢给 blocking 线程池，别占着 async worker。
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<updater::UpdateView, String> {
    let a = app.clone();
    match tauri::async_runtime::spawn_blocking(move || updater::check(&a, true)).await {
        Ok(Ok(_)) => Ok(updater::snapshot()),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("检查失败：{e}")),
    }
}

#[tauri::command]
async fn open_update_window(app: AppHandle) -> Result<(), String> {
    open_update_window_inner(&app).await
}

/// 用系统浏览器打开版本清单里的 `page`（现在是 **Gitee 发行版列表页**）。
///
/// **不直接把 .exe 交给浏览器**：Gitee 没有 `releases/latest/download` 那种
/// 重定向，直链必须带版本号，程序若记死某个文件名，换版本就 404。所以落点
/// 始终是一个固定不变的页面，由它去展示最新版和下载入口 —— 网页怎么改都不用
/// 重新编译发版。
///
/// 清单是网络来的内容，链接只放行 http/https —— 否则远端文件里写个
/// `file:///C:/...` 就能让程序替它执行本地东西。
#[tauri::command]
fn open_update_site() -> Result<(), String> {
    let m = updater::latest().ok_or("还没有拿到版本信息")?;
    // 优先 page（发行版列表页）；只有 download 的老清单也能用
    let url = if !m.page.is_empty() { m.page } else { m.download };
    if url.is_empty() {
        return Err("版本清单里没有站点地址".into());
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("链接不合法".into());
    }
    if desktop::shell_open(&url) {
        Ok(())
    } else {
        Err("打开链接失败".into())
    }
}

/// 「稍后」/ 右上角叉：只把窗口藏起来，不动任何状态
#[tauri::command]
fn hide_update(app: AppHandle) {
    if let Some(w) = app.get_webview_window("update") {
        let _ = w.hide();
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// `lightbox.exe --fix-bar` 的实体：把所有「嵌入任务栏」的盒子复位回桌面。
///
/// 应急出口。历史背景：Win11 上任务栏是 XAML 合成层，子窗口挂上去能画出来但
/// **收不到鼠标消息**（点不动、拖不动、右键也弹不出）—— 用户唯一的出路是删配置文件。
/// 现在 Win11 压根不走子窗口这条路（见 `desktop::win11()`），但万一还有别的路径
/// 把盒子摆进看不见的地方，这条命令能一条命令救回来，不用删数据。
fn fix_bar_boxes() {
    let mut cfg = store::load();
    let mut n = 0;
    for b in cfg.boxes.iter_mut() {
        if !b.in_bar {
            continue;
        }
        b.in_bar = false;
        b.topmost = false;
        if let Some(m) = desktop::monitor_at(b.x + 1, b.y + 1) {
            let size = (BOX_W * m.scale).round() as i32;
            b.x = clamp_i(b.x, m.work.x, (m.work.x + m.work.w - size).max(m.work.x));
            b.y = clamp_i(b.y, m.work.y, (m.work.y + m.work.h - size).max(m.work.y));
        }
        n += 1;
    }
    if n > 0 {
        store::save(&cfg);
    }
    println!("已把 {n} 个收纳盒从任务栏移回桌面");
}

pub fn run() {
    // `lightbox.exe --version`：只打印版本号就退出（排障 / 脚本读取用）。
    // 必须在单实例判断之前 —— 已经开着 LightBox 时也要能查版本。
    if std::env::args().skip(1).any(|a| a == "--version" || a == "-v") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // `lightbox.exe --check-update`：立刻拉一次版本清单并打印结果（排障用）。
    // 同样要放在单实例判断之前 —— 开着 LightBox 也能查。
    if std::env::args().skip(1).any(|a| a == "--check-update") {
        match updater::probe() {
            Ok(text) => println!("{text}"),
            Err(e) => println!("检查失败：{e}"),
        }
        return;
    }
    // `lightbox.exe --fix-bar`：把所有「嵌入任务栏」的盒子复位回桌面（应急出口）。
    // 放在单实例判断之后 —— 配置以内存里那份为准，两个进程同时写会互相覆盖。
    let fix_bar = std::env::args().skip(1).any(|a| a == "--fix-bar");
    if !acquire_single_instance() {
        // 已有实例在运行：直接退出，避免两套盒子窗口叠在一起
        if fix_bar {
            println!("请先退出正在运行的 LightBox，再执行 --fix-bar");
        }
        return;
    }
    // 读配置之前先留一份快照（见 store::snapshot）
    store::snapshot();
    if fix_bar {
        fix_bar_boxes();
    }
    let cfg0 = store::load();
    // 「嵌入任务栏时隐藏名称」要在建窗口之前就位：盒子宽度靠它算，而任务栏守护线程
    // 拿不到 State，所以同步到全局原子量（见 bar_hide_label）。
    set_bar_hide_label(cfg0.bar_hide_label);
    let app = tauri::Builder::default()
        .manage(State {
            cfg: Mutex::new(cfg0),
            panel_target: Mutex::new(String::new()),
            panel_seq: Mutex::new(0),
            menu_target: Mutex::new(String::new()),
            menu_pt: Mutex::new((0, 0)),
            settings_target: Mutex::new(String::new()),
            settings_height: Mutex::new(SET_H),
            gsettings_height: Mutex::new(SET_H),
            backdrop: Mutex::new(None),
            pick_area: Mutex::new(None),
            pick_start: Mutex::new(None),
            pick_caller: Mutex::new(None),
            group_drag: Mutex::new(None),
        })
        .setup(|app| {
            let handle = app.handle().clone();
            let scale = app_scale(&handle);
            let boxes = {
                let st = app.state::<State>();
                let mut cfg = st.cfg.lock().unwrap();
                if cfg.boxes.is_empty() {
                    // 首次启动：默认两个空盒，主屏工作区底部居中、水平依次排列
                    let spots = default_row(&handle, 2);
                    for ((x, y), name) in spots.into_iter().zip(["系统工具", "办公工具"]) {
                        cfg.boxes.push(BoxConfig {
                            id: new_id(),
                            name: name.into(),
                            x,
                            y,
                            topmost: false,
                            in_bar: false,
                            icon: None,
                            order: Vec::new(),
                            group: None,
                        });
                    }
                    store::save(&cfg);
                } else if dedupe(&mut cfg.boxes, scale) {
                    store::save(&cfg);
                }
                // 老配置修正：「压在任务栏上一半以上」= 嵌入任务栏（见 migrate_pressed_into_bar）
                if migrate_pressed_into_bar(&mut cfg.boxes) {
                    store::save(&cfg);
                }
                // 落点：跑到屏幕外的盒子（上次在副屏上、副屏这次没插）先临时夹回可见区域。
                // **不写回配置** —— 副屏插回来后它会自己回到原来那块屏，见 reflow_boxes。
                cfg.boxes
                    .iter()
                    .map(|b| {
                        let mut t = b.clone();
                        clamp_box_into_screen(&mut t);
                        t
                    })
                    .collect::<Vec<BoxConfig>>()
            };
            for b in &boxes {
                store::box_dir(&b.id);
                if let Err(e) = spawn_box_window(&handle, b, true) {
                    eprintln!("创建收纳盒窗口失败: {e}");
                }
            }
            if let Err(e) = spawn_panel(&handle) {
                eprintln!("创建面板窗口失败: {e}");
            }
            if let Err(e) = spawn_menu(&handle) {
                eprintln!("创建菜单窗口失败: {e}");
            }
            // 设置窗口也提前建好（隐藏）：第一次点「设置…」就是直接显示，
            // 不用现建窗口 + 等页面加载，不会有一秒空白。
            if let Err(e) = spawn_settings(&handle) {
                eprintln!("创建设置窗口失败: {e}");
            }
            // 更新窗口同样提前建好（隐藏）：提醒弹出时不用等页面加载
            if let Err(e) = spawn_update(&handle) {
                eprintln!("创建更新窗口失败: {e}");
            }
            if let Err(e) = build_tray(&handle) {
                eprintln!("创建系统托盘失败: {e}");
            }
            desktop::start_watchdog(handle.clone(), BOX_W as i32, BOX_RADIUS);
            // Ctrl 物理键状态广播：面板的 Ctrl+点击多选要靠它同步判断（见 start_ctrl_watch）
            start_ctrl_watch(handle.clone());
            // 显示配置（拔插屏 / 单屏双屏 / 分辨率 / 缩放）一变就把盒子摆回原位，
            // 否则 Progman 客户区原点一挪，所有盒子就整块平移出屏幕
            start_layout_watch(handle.clone());
            // 盒子窗口自愈：Explorer 重启会连带销毁挂在 Progman 下的盒子窗口，
            // 这里每 3 秒查一次，缺了就按 config 重建（否则盒子会「突然全部消失」）
            start_box_heal(handle.clone());
            // 版本检查：后台线程，启动 8 秒后跑第一次，之后每天一次
            updater::start(handle.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_box_view,
            box_ready,
            begin_box_rename,
            list_items,
            reorder_items,
            import_paths,
            open_item,
            reveal_item,
            item_properties,
            restore_item,
            remove_item,
            restore_items,
            remove_items,
            restore_all_items,
            drag_item_out,
            create_box,
            rename_box,
            delete_box,
            move_box,
            snap_box,
            snap_bar,
            bar_hover,
            box_scale,
            label_luma,
            set_box_topmost,
            set_box_in_bar,
            open_panel,
            open_settings,
            open_global_settings,
            auto_sort,
            fit_settings,
            hide_settings,
            get_settings_target,
            panel_ready,
            refocus_panel,
            get_theme,
            set_theme,
            panel_backdrop,
            get_panel_target,
            foreground_is_self,
            mouse_left_down,
            cursor_in_panel,
            cursor_on_box,
            ctrl_down,
            group_boxes,
            ungroup_box,
            group_drag_begin,
            group_follow,
            group_end,
            close_panel,
            show_box_menu,
            hide_menu,
            fit_menu,
            get_menu_data,
            pick_box_icon,
            clear_box_icon,
            pick::begin_pick,
            pick::pick_ready,
            pick::pick_start_now,
            pick::pick_sample,
            pick::pick_commit,
            pick::pick_cancel,
            get_settings,
            set_autostart,
            open_data_dir,
            update_state,
            check_update,
            open_update_window,
            open_update_site,
            hide_update,
            quit_app
        ])
        .build(tauri::generate_context!())
        .expect("LightBox 启动失败");
    // 所有退出路径（托盘退出 / 设置页退出 / app.exit）最终都会到这里：
    // 事件循环结束前把托盘图标显式删掉，不等 Explorer 事后回收
    app.run(|_app, event| {
        if let tauri::RunEvent::Exit = event {
            destroy_tray();
        }
    });
}
