//! 版本更新检查。
//!
//! 流程：每天拉一次 `version.json`（就是 Cloudflare Pages 上那个静态文件），
//! 拿里面的 `latest` 和本机 `CARGO_PKG_VERSION` 比。**有新版本也不弹窗** ——
//! 只在设置窗口左下角的版本号那里显示一个红色提醒，用户自己想更新再点。
//!
//! 三条设计约束（都是「别烦到用户」）：
//! 1. **每天最多连一次**：联网结果记在 `update.json` 的 `last_check` 里，
//!    24 小时内不再请求；失败时按 30 分钟重试，不因为断网就一天不检查。
//! 2. **不主动弹窗、不主动抢焦点**：检查结果通过 `update:data` 事件推给已经打开的
//!    设置窗口；没打开设置窗口的用户完全感觉不到后台在检查。
//! 3. **手动入口照旧**：托盘「检查更新…」和设置页的版本号都能主动查。
//!
//! `update.json` 放在 exe 同级（和 config.json 一起），删掉它就等于「重置更新状态」。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::AppHandle;
use tauri::Emitter;

use crate::http;
use crate::store;

/// 版本清单地址。编译期从 `src-tauri/update-url.txt` 读进来 ——
/// 想换地址就改那个文件再重新编译（发新版本来就要重编译，多这一步不亏）。
const DEFAULT_URL: &str = include_str!("../update-url.txt");

/// 自动检查间隔：24 小时。
const CHECK_EVERY: u64 = 24 * 60 * 60 * 1000;
/// 检查失败后的重试间隔：30 分钟（断网半小时后恢复，不用等到明天）。
const RETRY_EVERY: u64 = 30 * 60 * 1000;
/// 网络超时（毫秒）。一次检查可能要发 2~3 个请求（跳转 / JS 验证 / 真正取数据），
/// 每个请求都重新计时，所以这个值不用乘。别设太大：主地址慢的时候要尽快
/// 切到备用地址，不然用户在设置页点「检查更新」会干等。
const TIMEOUT_MS: u32 = 8000;
/// 清单最大字节数
const MAX_BYTES: usize = 256 * 1024;

/// 服务器上的 version.json 解析结果
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Manifest {
    /// 最新版本号，如 "0.9.20"
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

/// 给前端「更新」窗口用的完整状态
#[derive(Serialize, Clone, Debug)]
pub struct UpdateView {
    pub current: String,
    pub latest: String,
    pub date: String,
    pub notes: Vec<String>,
    pub download: String,
    pub page: String,
    pub has_update: bool,
    pub checked_at: u64,
    pub error: String,
    pub busy: bool,
    /// false = 还没配置更新地址（update-url.txt 还是占位符）
    pub configured: bool,
}

/// 内存里的最近一次检查结果（进程生命周期内有效）
#[derive(Default)]
struct Mem {
    latest: Option<Manifest>,
    checked_at: u64,
    error: String,
    busy: bool,
}

static MEM: Mutex<Mem> = Mutex::new(Mem {
    latest: None,
    checked_at: 0,
    error: String::new(),
    busy: false,
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
    store::root_dir().join("update.json")
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
/// - 否则用编译期从 `src-tauri/update-url.txt` 读进来的那个
///
/// 支持多个地址是有原因的：`*.pages.dev` 在国内部分网络下解析不到，
/// 这时可以再加一个国内可达的镜像地址兜底。
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
    if let Ok(s) = std::fs::read_to_string(store::root_dir().join("update_url.txt")) {
        collect(&s, &mut out);
    }
    let default_local = DEFAULT_URL.trim().to_string();
    if !out.contains(&default_local) {
        collect(DEFAULT_URL, &mut out);
    }
    out
}

/// 去掉还带占位符的地址（用户没改过模板时不去发请求，免得每次报 DNS 错误）
fn configured_urls() -> Vec<String> {
    update_urls()
        .into_iter()
        .filter(|u| !u.contains("YOUR-PROJECT") && !u.contains("example.com"))
        .collect()
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

pub fn snapshot() -> UpdateView {
    let m = MEM.lock().unwrap();
    let current = current_version();
    let latest = m.latest.clone().unwrap_or_default();
    let has_update = !latest.latest.is_empty() && is_newer(&latest.latest, &current);
    UpdateView {
        current,
        latest: latest.latest.clone(),
        date: latest.date.clone(),
        notes: latest.notes.clone(),
        download: latest.download.clone(),
        page: latest.page.clone(),
        has_update,
        checked_at: m.checked_at,
        error: m.error.clone(),
        busy: m.busy,
        configured: !configured_urls().is_empty(),
    }
}

/// 最近一次成功拉到的清单（给「打开下载页」用）
pub fn latest() -> Option<Manifest> {
    MEM.lock().unwrap().latest.clone()
}

/// 命令行诊断：`lightbox.exe --check-update` —— 立刻拉一次清单并打印结果。
///
/// 不连 GUI、不落盘、不改 `update.json`，所以随时可以跑，也不影响当天的
/// 自动检查节奏。用来快速回答「到底是网络不通、地址不对，还是清单格式错了」。
pub fn probe() -> Result<String, String> {
    use std::sync::mpsc;

    let urls = configured_urls();
    if urls.is_empty() {
        return Err("还没有配置更新地址（src-tauri/update-url.txt）".into());
    }
    // 和正式检查一样并发问，谁先成功用谁；失败的地址连同耗时一起记下来
    let (tx, rx) = mpsc::channel::<(String, u64, Result<Manifest, String>)>();
    for u in &urls {
        let (tx, u) = (tx.clone(), u.clone());
        std::thread::spawn(move || {
            let t0 = now_ms();
            let r = http::get_text(&u, TIMEOUT_MS, MAX_BYTES).and_then(|t| parse_manifest(&t));
            let _ = tx.send((u, now_ms().saturating_sub(t0), r));
        });
    }
    drop(tx);

    let mut log = String::new();
    let mut last = String::from("拿不到版本清单");
    while let Ok((u, cost, r)) = rx.recv() {
        match r {
            Ok(m) => {
                let mut out = String::new();
                out.push_str(&format!("地址    : {u}（{cost} ms）\n"));
                out.push_str(&format!("本机    : {}\n", current_version()));
                out.push_str(&format!("最新    : {}\n", m.latest));
                out.push_str(&format!("日期    : {}\n", m.date));
                out.push_str(&format!(
                    "有新版本: {}\n",
                    if is_newer(&m.latest, &current_version()) {
                        "是"
                    } else {
                        "否"
                    }
                ));
                out.push_str(&format!("网页    : {}\n", m.page));
                out.push_str("更新说明:\n");
                for n in &m.notes {
                    out.push_str(&format!("  - {n}\n"));
                }
                if !log.is_empty() {
                    out.push_str("\n本次尝试记录:\n");
                    out.push_str(&log);
                }
                return Ok(out);
            }
            Err(e) => {
                last = format!("{u} -> {e}");
                log.push_str(&format!("  {u} -> {e}（{cost} ms）\n"));
            }
        }
    }
    Err(format!("{last}\n本次尝试记录:\n{log}"))
}

/// 把最新状态推给所有页面（设置窗口用它点亮版本号旁边的红点，
/// 更新窗口用它切换显示内容）。没在听的窗口会直接忽略。
fn push(app: &AppHandle) {
    let _ = app.emit("update:data", snapshot());
}

/// 检查一次。`manual` 只影响日志语气，行为完全一样 ——
/// **无论自动还是手动，都不会弹窗**，结果统一走 `update:data` 事件。
pub fn check(app: &AppHandle, _manual: bool) -> Result<bool, String> {
    {
        let mut m = MEM.lock().unwrap();
        if m.busy {
            return Err("正在检查…".into());
        }
        m.busy = true;
        m.error.clear();
    }
    // 先推一次「检查中」给窗口，让界面立刻有反馈
    push(app);

    let r = do_check();

    {
        let mut m = MEM.lock().unwrap();
        m.busy = false;
        m.checked_at = now_ms();
        if let Err(e) = &r {
            m.error = e.clone();
        }
    }
    push(app);
    r
}

/// 并发问所有地址，谁先拿到合格清单就用谁。
///
/// 为什么要并发：现在的清单地址是「域名转发 → 免费主机」两跳，实测偶尔要
/// 二十几秒；而另一条直连真实主机的地址只要几秒。串行等前一个会让人以为
/// 「点了没反应」。两个地址指向同一个 `version.json`，先到先得没有副作用。
fn fetch_first(urls: &[String]) -> Result<Manifest, String> {
    use std::sync::mpsc;

    if urls.len() == 1 {
        return http::get_text(&urls[0], TIMEOUT_MS, MAX_BYTES).and_then(|t| parse_manifest(&t));
    }

    let (tx, rx) = mpsc::channel::<(String, Result<Manifest, String>)>();
    for u in urls {
        let (tx, u) = (tx.clone(), u.clone());
        std::thread::spawn(move || {
            let r = http::get_text(&u, TIMEOUT_MS, MAX_BYTES).and_then(|t| parse_manifest(&t));
            let _ = tx.send((u, r));
        });
    }
    drop(tx);

    let mut errs: Vec<String> = Vec::new();
    while let Ok((u, r)) = rx.recv() {
        match r {
            Ok(m) => return Ok(m),
            Err(e) => errs.push(format!("{u} -> {e}")),
        }
    }
    Err(if errs.is_empty() {
        "拿不到版本清单".to_string()
    } else {
        errs.join("；")
    })
}

fn do_check() -> Result<bool, String> {
    let urls = configured_urls();
    if urls.is_empty() {
        return Err("还没有配置更新地址（src-tauri/update-url.txt）".into());
    }
    let man = fetch_first(&urls)?;
    let newer = is_newer(&man.latest, &current_version());

    {
        let mut m = MEM.lock().unwrap();
        m.latest = Some(man.clone());
        m.error.clear();
    }

    let mut d = load_disk();
    d.last_check = now_ms();
    d.last_try = d.last_check;
    save_disk(&d);

    // 这里**不再弹窗**：有没有新版本由 `update:data` 事件带给设置窗口，
    // 用户在左下角的版本号那里看到红色提醒，自己决定要不要更新。
    Ok(newer)
}

/// 后台定时检查线程：启动后等 8 秒（先让盒子和托盘起来）**无条件查一次**，
/// 之后每 30 分钟醒一次，24 小时内不再重复请求。
///
/// 首轮为什么不受「一天一次」限制：`update.json` 里的 `last_check` 记的是
/// 「上次成功检查」，如果用户上午手动查过一次，下午重开软件时按 24 小时节流
/// 就会直接跳过 —— 结果设置页永远不亮红字，看起来像「根本不检查更新」。
/// 开了软件就该知道自己是不是旧版，所以每次启动都问一次（一天也就多几次请求）。
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(8));
        let mut first_round = true;
        loop {
            // 还没有配置更新地址（模板里的占位符）就别白试，省得每半小时写一条日志
            if configured_urls().is_empty() {
                std::thread::sleep(Duration::from_secs(30 * 60));
                continue;
            }
            let d = load_disk();
            let now = now_ms();
            let never = d.last_check == 0;
            let due = first_round || never || now.saturating_sub(d.last_check) >= CHECK_EVERY;
            // 失败后 30 分钟内不再试（避免断网时一直狂刷）；首轮例外，既然刚启动就试
            let throttled =
                !first_round && !never && now.saturating_sub(d.last_try) < RETRY_EVERY;
            if due && !throttled {
                let mut d2 = load_disk();
                d2.last_try = now_ms();
                save_disk(&d2);
                if let Err(e) = check(&app, false) {
                    // 静默失败：更新检查不该打扰用户，日志留给排查
                    eprintln!("检查更新失败：{e}");
                }
            }
            first_round = false;
            std::thread::sleep(Duration::from_secs(30 * 60));
        }
    });
}
