//! 版本更新检查。
//!
//! 每天拉一次 `version.json`（Gitee 上的静态文件），拿里面的 `latest` 和本机
//! 版本比。**有新版本也不主动弹窗** —— 只在设置窗口左下角的版本号那里点一个
//! 红点，用户自己想更新再点；托盘「检查更新…」是手动入口。
//!
//! 三条设计约束（都是「别烦到用户」）：
//! 1. **每天最多连一次**：联网结果记在 `update.json` 的 `last_check` 里，
//!    24 小时内不再请求；失败时按 30 分钟重试，不因为断网就一天不检查。
//! 2. **不主动弹窗、不主动抢焦点**：检查跑在后台线程里，结果只更新设置窗口
//!    那个红点；没打开设置窗口的用户完全感觉不到后台在检查。
//! 3. **手动入口照旧**：托盘「检查更新…」和设置页的版本号都能主动查。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, PostMessageW, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_YESNO, WM_APP,
};

use crate::app::app;
use crate::config;
use crate::{desktop, http, log};

/// 版本清单地址。编译期从 `native/update-url.txt` 读进来 ——
/// 想换地址就改那个文件再重新编译（发新版本来就要重编译，多这一步不亏）。
const DEFAULT_URL: &str = include_str!("../update-url.txt");

/// 自动检查间隔：24 小时。
const CHECK_EVERY: u64 = 24 * 60 * 60 * 1000;
/// 检查失败后的重试间隔：30 分钟（断网半小时后恢复，不用等到明天）。
const RETRY_EVERY: u64 = 30 * 60 * 1000;
/// 网络超时（毫秒）。一次检查可能要发两三个请求（跳转 / 真正取数据），
/// 每个请求都重新计时，所以这个值不用乘。
const TIMEOUT_MS: u32 = 8000;
/// 清单最大字节数
const MAX_BYTES: usize = 256 * 1024;

/// 检查完了，主线程收到这条消息后弹结果框。
/// 注意别和 `tray::WM_TRAY`（0x8001）撞号 —— 撞了会被托盘那条分支吃掉。
pub const WM_UPDATE_DONE: u32 = WM_APP + 2;

/// 服务器上的 version.json 解析结果
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Manifest {
    /// 最新版本号，如 "1.0.1"
    pub latest: String,
    /// 发布日期，随便什么格式，只用于显示
    pub date: String,
    /// 更新说明（一行一条）
    pub notes: Vec<String>,
    /// 安装包直链
    pub download: String,
    /// 更新说明网页
    pub page: String,
}

/// 内存里的最近一次检查结果（进程生命周期内有效）
struct Mem {
    latest: Option<Manifest>,
    checked_at: u64,
    error: String,
    busy: bool,
    /// 手动检查：结果要弹框告诉用户
    manual: bool,
}

static MEM: Mutex<Mem> = Mutex::new(Mem {
    latest: None,
    checked_at: 0,
    error: String::new(),
    busy: false,
    manual: false,
});

/// 落盘的检查状态
#[derive(Default, Serialize, Deserialize, Clone)]
struct Disk {
    /// 上次**成功**检查的时间
    last_check: u64,
    /// 上次尝试检查的时间（含失败，用于失败重试节流）
    last_try: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn state_path() -> std::path::PathBuf {
    config::root_dir().join("update.json")
}

fn load_disk() -> Disk {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Disk>(&t).ok())
        .unwrap_or_default()
}

fn save_disk(d: &Disk) {
    if let Ok(text) = serde_json::to_string_pretty(d) {
        let p = state_path();
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// 清单地址列表（按顺序尝试，第一个成功的就用）。
///
/// - exe 同级的 `update_url.txt` 优先（自测时改成 `http://127.0.0.1:8788/version.json`
///   之类，重启即生效；也可以写多行做备用地址）
/// - 否则用编译期从 `native/update-url.txt` 读进来的那个
///
/// 支持多个地址是有原因的：主站偶尔抽风时，可以用另一个镜像地址兜底。
pub fn update_urls() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let collect = |text: &str, out: &mut Vec<String>| {
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with("http://") || l.starts_with("https://") {
                out.push(l.to_string());
            }
        }
    };
    if let Ok(s) = std::fs::read_to_string(config::root_dir().join("update_url.txt")) {
        collect(&s, &mut out);
    }
    let default_local = DEFAULT_URL.trim().to_string();
    if !out.contains(&default_local) {
        collect(&default_local, &mut out);
    }
    out
}

/// 把 "v0.9.20" / "0.9.20-beta" 拆成数字段
fn parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().unwrap_or(0))
        .collect()
}

/// a 比 b 新？（逐段比较，缺的段按 0，所以 0.9.20 > 0.9.9）
pub fn is_newer(a: &str, b: &str) -> bool {
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let x = *pa.get(i).unwrap_or(&0);
        let y = *pb.get(i).unwrap_or(&0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 解析 version.json。容错处理：`notes` 写成数组或整段多行文本都认。
fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("版本清单格式不对：{e}"))?;
    let latest = v
        .get("latest")
        .or_else(|| v.get("version"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .trim_start_matches(['v', 'V'])
        .to_string();
    if latest.is_empty() {
        return Err("版本清单里没有 latest 字段".into());
    }
    let notes = match v.get("notes").or_else(|| v.get("changelog")) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .lines()
            .map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    Ok(Manifest {
        latest,
        date: s("date"),
        notes,
        download: s("download"),
        page: s("page"),
    })
}

/// 有新版本吗（只认已经拉到过清单的情况）
pub fn has_update() -> bool {
    let m = MEM.lock().unwrap();
    match &m.latest {
        Some(x) => !x.latest.is_empty() && is_newer(&x.latest, &current_version()),
        None => false,
    }
}

/// 正在检查吗
pub fn is_busy() -> bool {
    MEM.lock().unwrap().busy
}

/// 最近一次检查的错误（空 = 没出错）
pub fn last_error() -> String {
    MEM.lock().unwrap().error.clone()
}

/// 最近一次拿到的清单（给「打开下载页」用）
pub fn latest_manifest() -> Option<Manifest> {
    MEM.lock().unwrap().latest.clone()
}

/// 并发问所有地址，谁先拿到合格清单就用谁。
///
/// 为什么要并发：主地址偶尔要二十几秒才能通，串行等会让用户以为
/// 「点了没反应」。两个地址指向同一个 `version.json`，先到先得没有副作用。
fn fetch_first(urls: &[String]) -> Result<Manifest, String> {
    use std::sync::mpsc;

    if urls.len() == 1 {
        return http::get_text(&urls[0], TIMEOUT_MS, MAX_BYTES).and_then(|t| parse_manifest(&t));
    }

    let (tx, rx) = mpsc::channel::<Result<Manifest, String>>();
    for u in urls {
        let (tx, u) = (tx.clone(), u.clone());
        std::thread::spawn(move || {
            let r = http::get_text(&u, TIMEOUT_MS, MAX_BYTES).and_then(|t| parse_manifest(&t));
            let _ = tx.send(r);
        });
    }
    drop(tx);

    let mut errs: Vec<String> = Vec::new();
    while let Ok(r) = rx.recv() {
        match r {
            Ok(m) => return Ok(m),
            Err(e) => errs.push(e),
        }
    }
    Err(if errs.is_empty() {
        "拿不到版本清单".to_string()
    } else {
        errs.join("；")
    })
}

fn do_check() -> Result<bool, String> {
    let urls = update_urls();
    if urls.is_empty() {
        return Err("还没有配置更新地址".into());
    }
    let man = fetch_first(&urls)?;
    let newer = is_newer(&man.latest, &current_version());

    {
        let mut m = MEM.lock().unwrap();
        m.latest = Some(man);
        m.error.clear();
    }

    let mut d = load_disk();
    d.last_check = now_ms();
    d.last_try = d.last_check;
    save_disk(&d);
    Ok(newer)
}

/// 检查一次（异步）。`manual` 只决定要不要给用户弹结果框 ——
/// **无论自动还是手动，都不会主动抢焦点弹窗**，自动检查只点红点。
pub fn check(manual: bool) {
    {
        let mut m = MEM.lock().unwrap();
        if m.busy {
            return;
        }
        m.busy = true;
        m.error.clear();
        if manual {
            m.manual = true;
        }
    }
    std::thread::spawn(|| {
        let r = do_check();
        {
            let mut m = MEM.lock().unwrap();
            m.busy = false;
            m.checked_at = now_ms();
            if let Err(e) = &r {
                m.error = e.clone();
            }
        }
        if let Err(e) = &r {
            log::info(&format!("检查更新失败：{e}"));
        }
        notify_main();
    });
}

/// 通知主线程：检查完了，来更新界面 / 弹结果框
fn notify_main() {
    let hwnd = app().hwnd.lock().unwrap();
    if let Some(h) = *hwnd {
        unsafe {
            let _ = PostMessageW(Some(h), WM_UPDATE_DONE, WPARAM(0), LPARAM(0));
        }
    }
}

/// 主线程收到 `WM_UPDATE_DONE`：红点刷新 + 手动检查时弹结果框
pub fn on_done(hwnd: HWND) {
    let manual = {
        let mut m = MEM.lock().unwrap();
        std::mem::take(&mut m.manual)
    };
    crate::settings::repaint();
    // 面板标题栏的「有更新」小牌也跟着出现/消失（检查可能是在面板开着时跑完的）
    crate::panel::refresh_head();
    if manual {
        show_result(hwnd);
    }
}

/// 把最近一次检查结果告诉用户
fn show_result(hwnd: HWND) {
    let err = last_error();
    if !err.is_empty() {
        unsafe {
            MessageBoxW(
                Some(hwnd),
                &windows::core::HSTRING::from(format!("检查更新失败：{err}")),
                &windows::core::HSTRING::from("检查更新"),
                MB_OK | MB_ICONERROR,
            );
        }
        return;
    }
    let Some(man) = latest_manifest() else { return };
    if !is_newer(&man.latest, &current_version()) {
        unsafe {
            MessageBoxW(
                Some(hwnd),
                &windows::core::HSTRING::from(format!(
                    "当前已是最新版本（v{}）。",
                    current_version()
                )),
                &windows::core::HSTRING::from("检查更新"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        return;
    }

    let mut text = format!("发现新版本 v{}", man.latest);
    if !man.date.is_empty() {
        text.push_str(&format!("（{}）", man.date));
    }
    if !man.notes.is_empty() {
        text.push_str("\n\n");
        for n in &man.notes {
            text.push_str(&format!("· {n}\n"));
        }
    }
    let url = if man.download.is_empty() {
        man.page.clone()
    } else {
        man.download.clone()
    };
    if !url.is_empty() {
        text.push_str("\n是否打开下载页面？");
    }
    unsafe {
        let r = MessageBoxW(
            Some(hwnd),
            &windows::core::HSTRING::from(text),
            &windows::core::HSTRING::from("有新版本"),
            MB_YESNO | MB_ICONINFORMATION,
        );
        if r == IDYES && !url.is_empty() {
            let _ = desktop::shell_open(&url);
        }
    }
}

/// 命令行诊断：`lightbox.exe --check-update` —— 立刻拉一次清单并打印结果。
///
/// 不连 GUI、不落盘、不改 `update.json`，所以随时可以跑，也不影响当天的
/// 自动检查节奏。用来快速回答「到底是网络不通、地址不对，还是清单格式错了」。
pub fn probe() -> Result<String, String> {
    let urls = update_urls();
    if urls.is_empty() {
        return Err("还没有配置更新地址".into());
    }
    let man = fetch_first(&urls)?;
    let mut out = String::new();
    out.push_str(&format!("地址    : {}\n", urls.join(" / ")));
    out.push_str(&format!("本机    : {}\n", current_version()));
    out.push_str(&format!("最新    : {}\n", man.latest));
    out.push_str(&format!("日期    : {}\n", man.date));
    out.push_str(&format!(
        "有新版本: {}\n",
        if is_newer(&man.latest, &current_version()) {
            "是"
        } else {
            "否"
        }
    ));
    out.push_str(&format!("网页    : {}\n", man.page));
    out.push_str("更新说明:\n");
    for n in &man.notes {
        out.push_str(&format!("  - {n}\n"));
    }
    Ok(out)
}

/// 后台定时检查线程：启动后等 8 秒（先让盒子和托盘起来）**无条件查一次**，
/// 之后每 30 分钟醒一次，24 小时内不再重复请求。
///
/// 首轮为什么不受「一天一次」限制：`update.json` 里的 `last_check` 记的是
/// 「上次成功检查」，如果用户上午手动查过一次，下午重开软件时按 24 小时节流
/// 就会直接跳过 —— 结果设置页永远不亮红点，看起来像「根本不检查更新」。
/// 开了软件就该知道自己是不是旧版，所以每次启动都问一次。
pub fn start() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(8));
        let mut first_round = true;
        loop {
            let d = load_disk();
            let now = now_ms();
            let never = d.last_check == 0;
            let due = first_round || never || now.saturating_sub(d.last_check) >= CHECK_EVERY;
            // 失败后 30 分钟内不再试（避免断网时一直狂刷）；首轮例外
            let throttled =
                !first_round && !never && now.saturating_sub(d.last_try) < RETRY_EVERY;
            if due && !throttled {
                let mut d2 = load_disk();
                d2.last_try = now_ms();
                save_disk(&d2);
                check(false);
            }
            first_round = false;
            std::thread::sleep(Duration::from_secs(30 * 60));
        }
    });
}
