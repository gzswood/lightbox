//! LightBox（原生版）入口。
//!
//! 整个程序只有一个进程、一个消息循环：托盘图标 + 长条窗口 + 弹出面板
//! 全是同一进程里的顶层窗口。没有 WebView2，也没有 Node。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod bar;
mod collate;
mod config;
mod desktop;
mod dnd;
mod geom;
mod glass;
mod http;
mod iconpick;
mod icons;
mod log;
mod menu;
mod panel;
mod picker;
mod rename;
mod render;
mod settings;
mod shortcut;
mod taskbar;
mod theme;
mod tray;
mod ui;
mod updater;
mod util;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE, WIN32_ERROR,
};
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::app::app;

/// 单实例用的命名互斥体。`.v1` 必须和 0.9.x 保持一致 —— 覆盖升级时
/// 旧实例还没退出，新 exe 才能被正确拦住，不会两套长条同时跑。
const INSTANCE_MUTEX: &str = "Local\\LightBox.SingleInstance.v1";
/// 早期原生试验版用的是没有后缀的名字，一并占住，避免新旧原生实例并存。
const INSTANCE_MUTEX_LEGACY: &str = "Local\\LightBox.SingleInstance";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--version`：只打印版本号就退出（排障 / 脚本读取用）。
    // 必须放在单实例判断之前 —— 已经开着 LightBox 时也要能查版本。
    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // `--paths`：打印实际使用的目录（配置、收纳、日志）
    if args.iter().any(|a| a == "--paths") {
        let root = config::root_dir();
        println!("数据目录：{}", root.display());
        println!("配置文件：{}", root.join("config.json").display());
        println!("收纳目录：{}", root.join("boxes").display());
        println!("日志文件：{}", root.join("lightbox.log").display());
        return;
    }
    // `--dump-config`：把读到的配置原样打出来（排障用：看迁移有没有出错）
    // 只读不迁移 —— 排障要看到「文件里现在是什么」，不是「迁移后是什么」
    if args.iter().any(|a| a == "--dump-config") {
        let cfg = config::load_raw();
        match serde_json::to_string_pretty(&cfg) {
            Ok(t) => println!("{t}"),
            Err(e) => println!("序列化失败：{e}"),
        }
        return;
    }
    // `--check-update`：立刻拉一次版本清单并打印（不连 GUI、不落盘）
    if args.iter().any(|a| a == "--check-update") {
        match updater::probe() {
            Ok(t) => println!("{t}"),
            Err(e) => println!("检查更新失败：{e}"),
        }
        return;
    }

    // per-monitor aware：所有坐标都是物理像素，多屏各屏缩放不同也不会错位
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // OLE 初始化：拖放（DoDragDrop / RegisterDragDrop）必须先做这一步，
        // 否则注册放置目标会直接失败（CO_E_NOTINITIALIZED）
        let _ = OleInitialize(None);
    }

    // 诊断日志：默认完全关闭（设置页「功能设置 → 打印诊断信息」）。
    // 这里只**只读**地看一眼开关，好让「已有实例在运行」这种启动期故障也留得下痕迹；
    // 真正的加载（可能触发迁移）在单实例判断之后。
    log::init(
        &config::root_dir().join("lightbox.log"),
        config::load_raw().diagnostics,
    );

    // 单实例：拿不到命名互斥体说明已经有一个在跑
    let _instance = match acquire_instance() {
        Ok(h) => h,
        Err(e) => {
            log::info(&format!("已有实例在运行，退出（{e}）"));
            return;
        }
    };

    config::snapshot();
    let cfg = config::load();
    // 迁移不该改这一项，保险起见再同步一次（同值直接返回）
    log::set_enabled(cfg.diagnostics);
    log::info(&format!(
        "配置载入完成：{} 个收纳盒，长条降级位置 ({},{})",
        cfg.boxes.len(),
        cfg.bar.x,
        cfg.bar.y
    ));
    // 弹窗字体：启动时先把 config.json 里的值灌进文字渲染器，
    // 之后设置页「应用 / 确定」会再调一次并清缓存。
    render::set_font(&cfg.panel_font, cfg.panel_font_size);
    *app().cfg.lock().unwrap() = cfg;

    if let Err(e) = bar::init() {
        log::info(&format!("{e}"));
        return;
    }
    bar::refresh();

    // 后台检查更新：每天一次，查到新版本只在设置页点个红点
    updater::start();

    app::run();
}

/// 拿单实例锁。已经有实例在跑时返回 Err。
///
/// `windows` crate 的 `CreateMutexW` 在「名字已存在」时仍然返回 `Ok(HANDLE)`
/// （只有句柄无效才走 `Err`），`ERROR_ALREADY_EXISTS` 只留在线程 last-error 里。
/// 所以必须调用前清零、成功后立刻读 `GetLastError()`，只看 `Result` 会漏判。
fn acquire_instance() -> Result<Vec<HANDLE>, String> {
    unsafe {
        let mut handles = Vec::with_capacity(2);
        for name in [INSTANCE_MUTEX, INSTANCE_MUTEX_LEGACY] {
            let wide = util::wide(name);
            SetLastError(WIN32_ERROR(0));
            let h = CreateMutexW(None, true, PCWSTR(wide.as_ptr()))
                .map_err(|e| e.to_string())?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return Err("已经有一个 LightBox 在运行".into());
            }
            handles.push(h);
        }
        Ok(handles)
    }
}
