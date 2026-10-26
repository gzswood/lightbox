//! 屏幕取色器（吸管）：设置页里「背景颜色」的取色工具。
//!
//! 为什么不用 `<input type="color">`：它弹的是 Chromium 的原生取色对话框，
//! 那是个**普通顶层窗口**，而设置窗口是 `always_on_top` —— 对话框直接被压在设置窗口
//! 底下，用户看到的就是「点了没反应」（换一台机器 / 换一个 WebView2 版本表现还不一样，
//! 用户的笔记本上就这么挂的）。
//!
//! 现在自己实现一套「原生取色器」的体验：
//!  1. 点「吸管」→ 后端把设置窗口收起来，截一张**整个虚拟屏幕**的快照；
//!  2. 弹出一个盖住全屏的取色窗口，把快照铺进去当背景（画面是静止的，和系统取色器一致）；
//!  3. 鼠标移动 → 前端把窗口内坐标传回来，后端 BitBlt 光标周围 15×15 个物理像素
//!     缩成图 + 用 `GetPixel` 取中心点颜色，前端在光标旁边画一个放大镜；
//!  4. 左键点击 → 取**当前屏幕**上那个点的真实颜色（不是快照里的，防止这期间画面变了），
//!     通过 `pick:done` 事件发给设置窗口，设置窗口把颜色填进对应的色块；
//!  5. 右键 / Esc → 取消。
//!
//! 坐标说明：前端传的是**取色窗口内的 CSS 坐标**，但它只用来画放大镜和判断贴边。
//! 真正取样/落色一律用 `GetCursorPos` 拿到的物理光标位置 —— 取色窗口横跨所有显示器，
//! 混合缩放下它只有一个 DPI，用 CSS 坐标换算会在另一块屏上整体偏移（取到的颜色和
//! 光标实际指的颜色对不上，就是这来的）。
use crate::desktop;
use crate::glass;
use crate::store::State;
use crate::Rect;
use crate::win32::*;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};

/// 光标周围取样的大小（物理像素）。15×15 放大 9 倍后足够看清细节，编码也只要几十微秒
const SAMPLE: i32 = 15;

#[derive(Serialize, Clone)]
pub struct PickStart {
    /// 整个虚拟屏幕的快照（PNG data URL），取色窗口铺它当背景
    pub shot: String,
    /// 虚拟屏幕矩形（物理像素）
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 取色窗口的 DPI 系数（CSS px → 物理 px）
    pub scale: f64,
}

#[derive(Serialize, Clone)]
pub struct PickSample {
    /// 光标周围 SAMPLE×SAMPLE 的原图（PNG data URL）
    pub shot: String,
    /// 光标处颜色，#RRGGBB
    pub color: String,
}

/// 虚拟屏幕（所有显示器拼起来的那块矩形），物理像素。
/// 取色窗口盖整个虚拟屏幕：光标无论在哪儿都在窗口里，pointermove 才不会断。
pub fn virtual_screen() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// 取屏幕上一个物理像素的颜色。取不到返回 None。
pub fn pixel_at(x: i32, y: i32) -> Option<(u8, u8, u8)> {
    unsafe {
        let dc = GetDC(0);
        if dc == 0 {
            return None;
        }
        let c = GetPixel(dc, x, y);
        ReleaseDC(0, dc);
        if c == CLR_INVALID {
            return None;
        }
        Some((c as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8))
    }
}

pub fn hex_at(x: i32, y: i32) -> Option<String> {
    pixel_at(x, y).map(|(r, g, b)| format!("#{:02X}{:02X}{:02X}", r, g, b))
}

/// 建（或复用）盖住全屏的取色窗口。窗口是常驻的：第一次建，之后只搬位置和尺寸。
pub fn ensure_window(app: &AppHandle, area: &Rect) -> Result<tauri::WebviewWindow, String> {
    if let Some(w) = app.get_webview_window("pick") {
        return Ok(w);
    }
    let win = WebviewWindowBuilder::new(
        app,
        "pick",
        WebviewUrl::App("index.html?mode=pick".into()),
    )
    .title("LightBox 取色")
    // 逻辑尺寸随便给个近似值，紧接着 strip_chrome 会按物理像素钉死
    .inner_size(area.w as f64, area.h as f64)
    .position(area.x as f64, area.y as f64)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .skip_taskbar(true)
    .always_on_top(true)
    .focused(true)
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    place(&win, area);
    Ok(win)
}

/// 把取色窗口摆到「正好盖住 area」并清掉窗口区域（整块都能点）。
/// 尺寸用物理像素：strip_chrome 收逻辑像素，所以先按窗口自己的 DPI 换算回去。
pub fn place(win: &tauri::WebviewWindow, area: &Rect) {
    let hwnd = crate::hwnd_of(win);
    let sc = desktop::scale_of(hwnd).max(0.5);
    let lw = (area.w as f64 / sc).round() as i32;
    let lh = (area.h as f64 / sc).round() as i32;
    desktop::strip_chrome(hwnd, area.x, area.y, lw, lh, 0, false);
    let _ = win.set_position(PhysicalPosition::new(area.x, area.y));
    let _ = win.set_size(PhysicalSize::new(area.w, area.h));
    // 再钉一次：跨屏移动后 DPI 可能变，strip_chrome 内部会按新 DPI 补一次尺寸
    let sc2 = desktop::scale_of(hwnd).max(0.5);
    if (sc2 - sc).abs() > 0.01 {
        let lw2 = (area.w as f64 / sc2).round() as i32;
        let lh2 = (area.h as f64 / sc2).round() as i32;
        desktop::strip_chrome(hwnd, area.x, area.y, lw2, lh2, 0, false);
    }
}

/// 前端（取色窗口内）坐标 → 物理屏幕坐标。
///
/// 只作为**兜底**：取色窗口是一个横跨所有显示器的窗口，混合缩放（笔记本 125% + 外接
/// 100%）下它只能有一个 DPI，而 WebView 的 devicePixelRatio 又是建窗口那一刻定死的，
/// `client * scale` 在另一块屏上会整体偏掉 —— 表现就是「吸管指着的颜色和取到的颜色
/// 不是一回事」。真实光标位置直接在屏幕物理坐标系里，问系统最准（见 `cursor_phys`）。
fn client_to_phys(app: &AppHandle, cx: f64, cy: f64) -> Option<(i32, i32)> {
    let win = app.get_webview_window("pick")?;
    let pos = win.outer_position().ok()?;
    let sc = win.scale_factor().unwrap_or(1.0);
    Some((
        pos.x + (cx * sc).round() as i32,
        pos.y + (cy * sc).round() as i32,
    ))
}

/// 光标此刻的**物理屏幕坐标**。取色的取样点和落点一律用它。
fn cursor_phys() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return None;
    }
    Some((pt.x, pt.y))
}

/// 点「吸管」：收起设置窗口 → 截全屏快照 → 返回给前端画背景。
/// 窗口先不显示：等前端把快照铺好（`pick_ready`）再露头，既不会闪一下玻璃，
/// 也不会把自己截进快照里。
#[tauri::command]
pub async fn begin_pick(
    app: AppHandle,
    state: tauri::State<'_, State>,
    caller: tauri::WebviewWindow,
) -> Result<PickStart, String> {
    let area = virtual_screen();
    if area.w <= 0 || area.h <= 0 {
        return Err("取不到屏幕尺寸".into());
    }
    let win = ensure_window(&app, &area)?;

    // 发起取色的窗口（单盒设置 / 全局设置）先收起来：它一直在屏幕上，会被一起
    // 截进快照，取色时看到一层「冻住的设置窗口」很怪。commit / cancel 时再原样放回来。
    // 用调用者自己的 label 而不是写死 "settings"——否则从托盘「设置…」进来时
    // 既不会藏起来，取到的颜色也发不回去（发给了另一个隐藏窗口）。
    let label = caller.label().to_string();
    let visible = caller.is_visible().unwrap_or(false);
    if visible {
        let _ = caller.hide();
        // 等桌面把设置窗口那块重新画出来，否则快照里还有它的影子
        std::thread::sleep(std::time::Duration::from_millis(140));
    }
    *state.pick_caller.lock().unwrap() = if visible { Some(label.clone()) } else { None };
    *state.pick_area.lock().unwrap() = Some(area);

    let shot = match glass::raw_data_url(area.x, area.y, area.w, area.h) {
        Some(s) => s,
        None => {
            // 截屏失败：把设置窗口放回去，别让用户对着一个什么都没有的屏幕
            *state.pick_caller.lock().unwrap() = None;
            *state.pick_area.lock().unwrap() = None;
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.show();
            }
            return Err("截屏失败".into());
        }
    };
    place(&win, &area);
    let data = PickStart {
        shot,
        x: area.x,
        y: area.y,
        w: area.w,
        h: area.h,
        scale: win.scale_factor().unwrap_or(1.0),
    };
    // 窗口是复用的：上次可能还停在屏幕上（用户没按 Esc 就点了吸管），先藏起来，
    // 否则新快照铺上去之前会闪一下旧画面。
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    }
    // 存会话 + 广播。
    //   - 页面还没加载完（第一次）时事件会丢 → 它挂载后自己用 `pick_start_now` 取；
    //   - 页面已在运行（第二次及以后）→ 走事件，拿到新快照后重新显示窗口。
    *state.pick_start.lock().unwrap() = Some(data.clone());
    let _ = win.emit("pick:start", data.clone());
    Ok(data)
}

/// 取色窗口页面挂载时调一次：拿到「当前这次取色会话」的快照。
///
/// 为什么需要它：`pick:start` 事件在页面加载完成前广播出去就丢了（第一次打开时
/// 必然如此）。这个命令把同样的数据从 State 里取回来，保证首帧也能铺上快照。
#[tauri::command]
pub fn pick_start_now(state: tauri::State<'_, State>) -> Option<PickStart> {
    state.pick_start.lock().unwrap().clone()
}

/// 前端把快照铺好了：这时候才能显示取色窗口（再早一帧都会闪）。
#[tauri::command]
pub async fn pick_ready(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("pick") {
        let area = virtual_screen();
        place(&win, &area);
        win.show().map_err(|e| e.to_string())?;
        let _ = win.set_focus();
    }
    Ok(())
}

/// 鼠标移动：取样光标周围 15×15 + 中心点颜色。
#[tauri::command]
pub async fn pick_sample(
    app: AppHandle,
    state: tauri::State<'_, State>,
    x: f64,
    y: f64,
) -> Result<PickSample, String> {
    let area = state
        .pick_area
        .lock()
        .unwrap()
        .unwrap_or_else(virtual_screen);
    let (px, py) = cursor_phys()
        .or_else(|| client_to_phys(&app, x, y))
        .ok_or("取色窗口不存在")?;
    // 取样框不能超出虚拟屏幕（贴边时 BitBlt 会失败）
    let half = SAMPLE / 2;
    let x0 = (px - half).max(area.x);
    let y0 = (py - half).max(area.y);
    let w = SAMPLE.min(area.right() - x0).max(1);
    let h = SAMPLE.min(area.bottom() - y0).max(1);
    let shot = glass::raw_data_url(x0, y0, w, h).ok_or("取样失败")?;
    let color = hex_at(px, py).unwrap_or_else(|| "#000000".into());
    Ok(PickSample { shot, color })
}

/// 左键点击：取当前屏幕上那个点的真实颜色，发给设置窗口，然后收摊。
#[tauri::command]
pub async fn pick_commit(
    app: AppHandle,
    state: tauri::State<'_, State>,
    x: f64,
    y: f64,
) -> Result<(), String> {
    let color = cursor_phys()
        .or_else(|| client_to_phys(&app, x, y))
        .and_then(|(px, py)| hex_at(px, py))
        .ok_or("取色失败");
    // finish 会把 caller 取走（并把它重新显示出来），所以先记下 label 再收摊
    let caller = state.pick_caller.lock().unwrap().clone();
    finish(&app, state);
    if let Ok(c) = color {
        // 发给发起取色的那个窗口：单盒设置是 `settings`，托盘「设置…」是 `gsettings`。
        // 以前写死发给 settings，从全局设置里点吸管就永远填不回去。
        // 万一没记下 caller（理论上不会），广播兜底——只有设置窗口监听这个事件。
        match caller.and_then(|l| app.get_webview_window(&l)) {
            Some(w) => {
                let _ = w.emit("pick:done", c);
            }
            None => {
                let _ = app.emit("pick:done", c);
            }
        }
    }
    Ok(())
}

/// 右键 / Esc：取消，什么都不改。
#[tauri::command]
pub async fn pick_cancel(app: AppHandle, state: tauri::State<'_, State>) -> Result<(), String> {
    finish(&app, state);
    Ok(())
}

/// 收摊：藏起取色窗口，把设置窗口放回来并还它焦点。
fn finish(app: &AppHandle, state: tauri::State<'_, State>) {
    if let Some(w) = app.get_webview_window("pick") {
        let _ = w.hide();
    }
    *state.pick_area.lock().unwrap() = None;
    // 会话结束：快照一起丢掉，下次 begin_pick 一定会重新截一张新的
    *state.pick_start.lock().unwrap() = None;
    // 把发起取色的窗口原样放回来（它可能被藏起来了）
    if let Some(label) = state.pick_caller.lock().unwrap().take() {
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}
