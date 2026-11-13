//! 极简文件日志：写 `<根目录>/lightbox.log`。
//!
//! 程序是 `windows_subsystem = "windows"` 的 GUI 程序，没有控制台，
//! `println!` 写出去就是虚空。诊断信息都落到文件里，排障时直接看这个文件。
//! 超过上限（2 MB）就改名轮转，避免无限增长。
//!
//! **默认完全关闭**：设置页「功能设置 → 打印诊断信息」控制这个总开关
//! （配置项 `diagnostics`，默认 false）。关着的时候连文件都不创建，
//! 一个字节都不落盘 —— 用户没要求排障就不该在磁盘上留痕。
//! 打开之后也只记「出错 / 一次性动作」，不做逐帧、逐次拖拽的刷屏日志。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 2 * 1024 * 1024;

static SINK: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
static PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 记下日志路径，并按开关决定要不要真的开始写。
///
/// 启动时由 `main` 在读完配置之后调用 —— 开关的值来自 `config.json`，
/// 所以要等配置载入完才知道该不该写。
pub fn init(path: &Path, enabled: bool) {
    if let Ok(mut g) = PATH.get_or_init(|| Mutex::new(None)).lock() {
        *g = Some(path.to_path_buf());
    }
    SINK.get_or_init(|| Mutex::new(None));
    ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled {
        return; // 关着：不建文件、不写任何东西
    }
    open_file(path);
    info(&format!("=== LightBox {} 启动 ===", env!("CARGO_PKG_VERSION")));
}

fn open_file(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    rotate_if_needed(path);
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    if let Some(sink) = SINK.get() {
        if let Ok(mut g) = sink.lock() {
            *g = file;
        }
    }
}

/// 运行中切换开关（设置页「打印诊断信息」）。
///
/// 打开时立刻建文件并写一行起始标记；关掉时直接放开文件句柄，
/// 之后 `info()` 全部短路 —— 真正做到「一个字节都不写」。
pub fn set_enabled(on: bool) {
    if on == ENABLED.load(Ordering::Relaxed) {
        return;
    }
    ENABLED.store(on, Ordering::Relaxed);
    if on {
        let path = PATH
            .get()
            .and_then(|m| m.lock().ok().and_then(|g| g.clone()));
        if let Some(p) = path {
            open_file(&p);
            info(&format!(
                "=== 诊断日志已开启（LightBox {}）===",
                env!("CARGO_PKG_VERSION")
            ));
        }
    } else if let Some(sink) = SINK.get() {
        if let Ok(mut g) = sink.lock() {
            *g = None;
        }
    }
}

fn rotate_if_needed(path: &Path) {
    let Ok(meta) = fs::metadata(path) else { return };
    if meta.len() < MAX_BYTES {
        return;
    }
    let _ = fs::rename(path, path.with_extension("log.old"));
}

/// 本地时间 `MM-DD HH:MM:SS`
fn stamp() -> String {
    unsafe {
        let st = windows::Win32::System::SystemInformation::GetLocalTime();
        format!(
            "{:02}-{:02} {:02}:{:02}:{:02}",
            st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

/// 写一行日志（开关关着时什么都不做）
pub fn info(msg: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let sink = match SINK.get() {
        Some(s) => s,
        None => return,
    };
    let line = format!("[{}] {}\n", stamp(), msg);
    if let Ok(mut g) = sink.lock() {
        if let Some(f) = g.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

/// 写一行日志（格式化）
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::info(&format!($($arg)*))
    };
}
