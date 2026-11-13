//! 显示器 / 工作区 / DPI。
//!
//! 全部用**物理像素**。进程本身是 per-monitor DPI aware（见 main.rs），
//! 所以这里拿到的坐标直接就是物理像素，不用再乘缩放。

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

/// 一块显示器
#[derive(Clone, Copy, Debug)]
pub struct Monitor {
    /// 整块屏幕
    pub rect: RECT,
    /// 工作区（去掉任务栏）
    pub work: RECT,
    /// 缩放百分比（125 表示 125%）
    pub scale: u32,
    /// 是否主显示器
    pub primary: bool,
}

impl Monitor {
    pub fn work_width(&self) -> i32 {
        self.work.right - self.work.left
    }
    pub fn work_height(&self) -> i32 {
        self.work.bottom - self.work.top
    }
}

unsafe extern "system" fn collect(
    hmon: HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _lprc: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let out = &mut *(data.0 as *mut Vec<Monitor>);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        out.push(Monitor {
            rect: mi.rcMonitor,
            work: mi.rcWork,
            scale: dpi_x.max(1),
            primary: mi.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
        });
    }
    BOOL(1)
}

/// 枚举所有显示器
pub fn monitors() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut out as *mut Vec<Monitor> as isize),
        );
    }
    if out.is_empty() {
        // 兜底：至少给出主显示器的尺寸，别让程序因为枚举失败就没窗口
        out.push(Monitor {
            rect: RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            work: RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
            scale: 96,
            primary: true,
        });
    }
    out
}

/// 主显示器
pub fn primary() -> Monitor {
    monitors()
        .into_iter()
        .find(|m| m.primary)
        .unwrap_or_else(|| monitors()[0])
}

/// 虚拟屏幕（所有显示器的并集）
pub fn virtual_rect() -> RECT {
    let mut r = RECT::default();
    for m in monitors() {
        r.left = r.left.min(m.rect.left);
        r.top = r.top.min(m.rect.top);
        r.right = r.right.max(m.rect.right);
        r.bottom = r.bottom.max(m.rect.bottom);
    }
    if r.right <= r.left || r.bottom <= r.top {
        r = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
    }
    r
}

/// 某个点所在显示器的工作区
pub fn work_area_at(x: i32, y: i32) -> Monitor {
    let mut best = primary();
    let mut best_dist = i64::MAX;
    for m in monitors() {
        if x >= m.rect.left && x < m.rect.right && y >= m.rect.top && y < m.rect.bottom {
            return m;
        }
        let dx = (x - m.work.left - m.work_width() / 2).abs() as i64;
        let dy = (y - m.work.top - m.work_height() / 2).abs() as i64;
        let d = dx * dx + dy * dy;
        if d < best_dist {
            best_dist = d;
            best = m;
        }
    }
    best
}

/// 把一个矩形整体平移，使其完整落在 `work` 里（越界时贴边，不缩放）
pub fn clamp_to_work(r: &mut RECT, work: &RECT) {
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if w > work.right - work.left {
        r.left = work.left;
        r.right = work.left + w;
        return;
    }
    if h > work.bottom - work.top {
        r.top = work.top;
        r.bottom = work.top + h;
        return;
    }
    if r.left < work.left {
        r.left = work.left;
        r.right = work.left + w;
    }
    if r.right > work.right {
        r.right = work.right;
        r.left = work.right - w;
    }
    if r.top < work.top {
        r.top = work.top;
        r.bottom = work.top + h;
    }
    if r.bottom > work.bottom {
        r.bottom = work.bottom;
        r.top = work.bottom - h;
    }
}
