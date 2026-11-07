//! 配置与收纳库：每个收纳盒对应磁盘上一个真实目录，收纳 = 把 .lnk 移进来。
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::desktop;
use crate::icons;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BoxConfig {
    pub id: String,
    pub name: String,
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub topmost: bool,
    /// 「嵌入任务栏」：盒子变成任务栏的子窗口（高 = 任务栏厚度、右侧显示两个汉字），
    /// 位置仍记在 x/y（屏幕坐标），但 y 恒等于任务栏上沿。见 lib.rs 的 `set_box_in_bar`。
    ///
    /// 由「拖到任务栏区域一半以上」自动触发（`embed_after_snap`），也可以从右键菜单切。
    #[serde(default)]
    pub in_bar: bool,
    #[serde(default)]
    pub icon: Option<String>,
    /// 面板里的自定义顺序（存文件名）。空 = 按名称排。用户拖动图标后写入。
    #[serde(default)]
    pub order: Vec<String>,
    /// 「胶囊团」id：几个吸附在一起的盒子被「组合」成一个整体后共享同一个值，
    /// 拖其中一个，其它成员跟着走（见 lib.rs 的 `group_drag_begin` / `group_follow`）。
    ///
    /// `None` = 不属于任何团，可以单独移动。由右键菜单的「组合 / 解散胶囊团」维护，
    /// 形态切换（嵌入/移出任务栏）、自动排序、删除盒子都会把成员摘出去。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 团内顺序（0 起）。团成员的左右/上下顺序本靠 x/y 隐含，但切屏后绝对坐标失效、
    /// 顺序就跟着乱，所以这里**持久化**一份顺序：成员排序时优先用它，缺失才退回
    /// x/y（兼容老配置）。由 `assign_group_pos` 在团形成 / 拖动落定后按最终坐标固化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_pos: Option<i32>,
}

/// 盒子坐标是「在哪块显示器布局下记下来的」的参照系（**物理像素**）。
///
/// 配置里的 x/y 一直是绝对屏幕坐标，可两台屏的工作区高度、缩放都不一样：
/// 笔记本屏（1920×1200 @125%、盒边 100、工作区底 1150）上贴着任务栏上沿的 y=1050，
/// 到外接 1920×1080 @100%（盒边 80、工作区底 1040）就成了整排压进任务栏里。
/// 记下当时的「工作区矩形 + 缩放」，显示配置一变就按它把坐标整体换算过去。
/// 见 lib.rs 的 `apply_layout_ref`。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutRef {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 缩放 ×100（125% → 125）。存整数，避免浮点比较抖动导致反复重排。
    pub scale_100: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub boxes: Vec<BoxConfig>,
    /// 面板背景的**透明度百分比**（0 = 完全不透明，50 = 最透）。数字越大越透明。
    ///
    /// 旧字段名是 `panel_opacity`（0.35~1.0 的 alpha），这里用 alias 兼容老配置：
    /// 老值 1.0 读进来就是 1%，视觉上仍然接近不透明，不会突然变透明。
    #[serde(default = "default_transparency", alias = "panel_opacity")]
    pub panel_transparency: f32,
    /// 收纳盒背景色（#RRGGBB）
    #[serde(default = "default_box_bg")]
    pub box_bg: String,
    /// 弹出面板背景色（#RRGGBB）
    #[serde(default = "default_panel_bg")]
    pub panel_bg: String,
    /// 面板开毛玻璃（DWM acrylic）
    #[serde(default = "default_acrylic")]
    pub panel_acrylic: bool,
    /// 弹出面板是否显示圆角（只影响面板，收纳盒和菜单不受影响）。
    /// 默认 **false = 直角窗口**：Win10 上圆角只能靠 SetWindowRgn 硬裁，边缘是 1 像素台阶，
    /// 不如直角干净；想要圆角的用户在设置里打开即可。
    #[serde(default)]
    pub panel_radius: bool,
    /// 「嵌入任务栏时隐藏名称」：打开后任务栏里的盒子只显示图标（正方形），
    /// 默认**关闭** = 显示两个汉字（横向胶囊）。
    #[serde(default = "default_bar_hide_label")]
    pub bar_hide_label: bool,
    /// 上一次记下 boxes 坐标时的显示器参照系。老配置没有这一项 → 首次运行写入当前值，
    /// 从下一次切屏开始生效（见 lib.rs 的 `apply_layout_ref`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_ref: Option<LayoutRef>,
}

pub fn default_transparency() -> f32 {
    0.0
}

/// 默认底板色：收纳盒和弹出窗口统一用这个中性灰蓝（#5A606B）。
/// 比纯黑柔和、比纯白不刺眼，深浅两种桌面壁纸下都耐看。
pub fn default_box_bg() -> String {
    "#5A606B".into()
}

pub fn default_panel_bg() -> String {
    "#5A606B".into()
}

pub fn default_acrylic() -> bool {
    // 默认关：毛玻璃在部分核显/远程桌面上会拖慢弹出速度，也不适合实色底板
    false
}

pub fn default_bar_hide_label() -> bool {
    // 默认关：任务栏里显示横向胶囊 + 右侧两个汉字
    false
}

/// 配置的「默认值版本」。改了默认外观就 +1，老配置会在 `load()` 里被迁移一次。
///
/// 2 = 2026-10-26 的实色底板批（#5A606B / 关毛玻璃 / 关圆角）
/// 3 = 同一批，但「嵌入任务栏时隐藏名称」改回默认关
pub const CONFIG_VERSION: u32 = 3;

fn default_version() -> u32 {
    CONFIG_VERSION
}

/// 老配置的默认值迁移：把仍然是**旧默认**的项换成新默认。
///
/// 只按版本号走一次，不逐项比对 —— 用户明确要求「把这批默认值固定下来」，
/// 升级后就该看到新的默认外观。想找回原来的值：`config.before-v<N>.json`
/// 是迁移前留的备份。
fn migrate_defaults(cfg: &mut Config) -> bool {
    if cfg.version >= CONFIG_VERSION {
        return false;
    }
    let from = cfg.version;
    if from < 2 {
        cfg.box_bg = default_box_bg();
        cfg.panel_bg = default_panel_bg();
        cfg.panel_acrylic = default_acrylic();
        cfg.panel_radius = false;
        cfg.panel_transparency = default_transparency();
    }
    if from < 3 {
        // v2 那一版把「隐藏名称」默认成了开，用户要求改回关
        cfg.bar_hide_label = default_bar_hide_label();
    }
    cfg.version = CONFIG_VERSION;
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            boxes: Vec::new(),
            panel_transparency: default_transparency(),
            box_bg: default_box_bg(),
            panel_bg: default_panel_bg(),
            panel_acrylic: default_acrylic(),
            panel_radius: false,
            bar_hide_label: default_bar_hide_label(),
            layout_ref: None,
        }
    }
}

pub struct State {
    pub cfg: Mutex<Config>,
    /// 面板当前展示的盒子 id（用于「再次点击同一个盒子=收起」）
    pub panel_target: Mutex<String>,
    /// 面板显示序号：每次「换盒子」自增。前端把内容换好、画完首帧后带着这个序号回报，
    /// 后端对得上才真正把窗口显示出来 —— 否则会先看到上一个盒子的图标。
    /// 关闭面板时也自增，用来作废那些还在路上的回报。
    pub panel_seq: Mutex<u64>,
    /// 右键菜单当前指向的盒子 id
    pub menu_target: Mutex<String>,
    /// 菜单弹出时的光标位置（物理像素）。前端测量完内容高度回报 `fit_menu` 时，
    /// 要按「弹出时」的位置重新贴边定位——用户此时可能已经移动了鼠标。
    pub menu_pt: Mutex<(i32, i32)>,
    /// 设置窗口当前指向的盒子 id（设置是独立窗口，页面重载时要能恢复）
    pub settings_target: Mutex<String>,
    /// 设置窗口上次「按内容算出来的高度」（逻辑像素）。下次打开直接用它摆放，
    /// 免得先按占位高度居中、再跳一下。
    pub settings_height: Mutex<f64>,
    /// 全局设置窗口（托盘进的「设置…」）上次的内容高度。和上面那个分开存：
    /// 全局设置没有「名称/置顶」两行，比单盒设置矮一截，混用会跳高度。
    pub gsettings_height: Mutex<f64>,
    /// 面板毛玻璃用的「背后画面」快照（PNG data URL）。
    /// 由 present_panel 在窗口还没露出来时截好，前端 `panel_backdrop` 取走。
    pub backdrop: Mutex<Option<String>>,
    /// 屏幕取色器正在工作时，取色窗口盖住的矩形（物理像素）；空闲时为 None。
    pub pick_area: Mutex<Option<crate::Rect>>,
    /// 最近一次取色会话的快照数据。
    ///
    /// 取色窗口是**复用**的（只在第一次创建），页面也就只挂载一次 —— 所以不能靠
    /// 「页面挂载时请求快照」，否则第二次点吸管就再也拿不到新快照、窗口也永远
    /// 不显示（「小画笔只能用一次」就是这么来的）。改为：begin_pick 把数据存这里
    /// 并广播 `pick:start`；窗口页面首次挂载时用 `pick_start_now` 取，之后每次
    /// 走事件。会话结束（commit / cancel）时清空。
    pub pick_start: Mutex<Option<crate::pick::PickStart>>,
    /// 发起取色的那个窗口 label（单盒设置 `settings` 或全局设置 `gsettings`）。
    /// 取色期间它被藏起来（否则会被截进快照），commit / cancel 时按 label 原样放回来，
    /// 取到的颜色也发给它——以前写死发给 `settings`，从托盘「设置…」里点吸管就白取。
    pub pick_caller: Mutex<Option<String>>,
    /// 正在进行中的「胶囊团拖动」会话：锚点 id + 其余成员相对锚点的偏移。
    ///
    /// 只在一次拖动期间有值（按下时算偏移，松手落位后清空）。拖动途中每次窗口移动
    /// 都按它把成员摆到「锚点 + 偏移」的位置，于是整团看起来是一起被拖走的。
    pub group_drag: Mutex<Option<crate::GroupDrag>>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Item {
    pub name: String,
    pub file: String,
    pub path: String,
    pub icon: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct BoxView {
    pub id: String,
    pub name: String,
    pub count: usize,
    pub topmost: bool,
    /// 是否已嵌入任务栏（前端据此切「任务栏胶囊」形态、并改用 snap_bar 落位）
    pub in_bar: bool,
    /// 舞台设计尺寸（**逻辑**像素）：普通盒子 80×80；任务栏盒子 = 任务栏厚度换算的高、
    /// 以及加宽后的宽。前端用它当设计尺寸（配合 box:scale 缩放，见 BoxView.tsx 的说明）。
    pub w: f64,
    pub h: f64,
    /// 嵌入任务栏时是否隐藏名称（前端据此决定画不画右侧两个字）
    pub bar_hide_label: bool,
    /// 用户自定义的盒子图标（png data URL 或文件路径）；为空时前端画内置图标
    pub icon: Option<String>,
    /// 属于某个「胶囊团」：拖动时要先问后端整团的偏移，拖着一起走
    pub in_group: bool,
}

/// 程序根目录 = exe 所在目录。
///
/// 配置（config.json）和收纳目录（boxes/）都放在这里，整个程序就是「绿色版」：
/// 拷走 exe + 这两样东西，换台机器直接能用。
///
/// 唯一例外：exe 放在 `C:\Program Files` 之类没有写权限的目录（比如用安装包装的）时，
/// 写探测会失败，这时退回 `%LOCALAPPDATA%\LightBox`，不能让程序因为写不了配置而起不来。
/// 设置页里显示的「存储位置」就是实际用的那个目录。
static ROOT: OnceLock<PathBuf> = OnceLock::new();
static LEGACY_DIR: OnceLock<PathBuf> = OnceLock::new();

fn legacy_dir() -> PathBuf {
    LEGACY_DIR
        .get_or_init(|| {
            let base = std::env::var("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."));
            base.join("LightBox")
        })
        .clone()
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".lightbox-write-test");
    let ok = fs::write(&probe, b"1").is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

/// 老版本（配置在 %LOCALAPPDATA%\LightBox）升级上来的第一次运行：
/// 把配置和收纳目录搬到程序目录，避免用户升级后「盒子全没了」。
fn migrate_from_legacy(dir: &Path) {
    let legacy = legacy_dir();
    if legacy == dir || !legacy.exists() {
        return;
    }
    if dir.join("config.json").exists() {
        return; // 已经迁移过 / 本来就是新装
    }
    let _ = fs::create_dir_all(dir.join("boxes"));
    if let Ok(rd) = fs::read_dir(legacy.join("boxes")) {
        for e in rd.flatten() {
            let src = e.path();
            let dst = dir.join("boxes").join(e.file_name());
            if dst.exists() {
                continue;
            }
            // 同盘直接 rename（瞬时）；跨盘 rename 会失败 → 退化成逐文件复制
            if fs::rename(&src, &dst).is_err() {
                if src.is_dir() {
                    let _ = fs::create_dir_all(&dst);
                    if let Ok(inner) = fs::read_dir(&src) {
                        for f in inner.flatten() {
                            let _ = fs::copy(f.path(), dst.join(f.file_name()));
                        }
                    }
                }
            }
        }
    }
    let _ = fs::copy(legacy.join("config.json"), dir.join("config.json"));
}

pub fn root_dir() -> PathBuf {
    ROOT.get_or_init(|| {
        if let Some(d) = exe_dir() {
            if writable(&d) {
                migrate_from_legacy(&d);
                return d;
            }
        }
        let d = legacy_dir();
        let _ = fs::create_dir_all(&d);
        d
    })
    .clone()
}

pub fn box_dir(id: &str) -> PathBuf {
    let dir = root_dir().join("boxes").join(id);
    let _ = fs::create_dir_all(&dir);
    dir
}

fn config_path() -> PathBuf {
    root_dir().join("config.json")
}

/// 启动时把当前配置快照成 `config.backup.json`。
///
/// 目的只有一个：**这次运行里不管发生什么（自动整理、误拖、崩溃），用户都还能拿回
/// 启动那一刻的盒子列表**。历史上出过一次「盒子莫名少两个」，就是自动整理删掉了
/// 重叠的空盒子 —— 现在既不删盒子，也留了这份快照。
pub fn snapshot() {
    let p = config_path();
    if p.exists() {
        let _ = fs::copy(&p, root_dir().join("config.backup.json"));
    }
}

pub fn load() -> Config {
    let p = config_path();
    if let Ok(text) = fs::read_to_string(&p) {
        if let Some(mut cfg) = parse_config(&text) {
            // 默认外观变过（CONFIG_VERSION 涨了）：迁移一次并立刻落盘，
            // 否则用户升级后看到的还是旧默认值，跟「默认设置」对不上。
            if migrate_defaults(&mut cfg) {
                // 迁移会覆盖用户的颜色/开关选择，先单独留一份迁移前的配置。
                // （config.backup.json 每次启动都重写，靠它兜不住下一次启动）
                let keep = root_dir().join(format!("config.before-v{CONFIG_VERSION}.json"));
                if !keep.exists() {
                    let _ = fs::copy(&p, &keep);
                }
                save(&cfg);
            }
            return cfg;
        }
        // 配置损坏：留一份备份，避免用户数据无声丢失
        let _ = fs::copy(&p, p.with_extension("json.bak"));
    }
    Config::default()
}

/// 解析配置文本。中间过一层 `Value`，是为了处理「老键 + 新键同时存在」：
/// `panel_transparency` 用 `#[serde(alias = "panel_opacity")]` 兼容老配置，
/// 但两个键同时在文件里时 serde 会按「重复字段」报错 —— 整份配置连带盒子列表全丢。
/// 用户手改过配置就可能撞上，这里先把老键摘掉再反序列化。
fn parse_config(text: &str) -> Option<Config> {
    let mut v: serde_json::Value = serde_json::from_str(text).ok()?;
    if let Some(o) = v.as_object_mut() {
        if o.contains_key("panel_transparency") {
            o.remove("panel_opacity");
        }
    }
    serde_json::from_value(v).ok()
}

pub fn save(cfg: &Config) {
    let p = config_path();
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        let tmp = p.with_extension("json.tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
    }
}

fn is_shortcut(p: &Path) -> bool {
    match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let e = ext.to_ascii_lowercase();
            e == "lnk" || e == "url"
        }
        None => false,
    }
}

fn unique_target(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(file_name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("item");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    for i in 2..9999 {
        let name = if ext.is_empty() {
            format!("{stem} ({i})")
        } else {
            format!("{stem} ({i}).{ext}")
        };
        let c = dir.join(name);
        if !c.exists() {
            return c;
        }
    }
    dir.join(format!("{stem}-{}.{ext}", uuid::Uuid::new_v4()))
}

fn to_item(p: &Path) -> Item {
    let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("未命名").to_string();
    Item {
        name,
        file: p.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_string(),
        path: p.to_string_lossy().to_string(),
        icon: icons::icon_data_url(p),
    }
}

/// 列出盒子内容。`order` 是用户拖动排好的文件名顺序：
/// 在 order 里的按 order 排，不在的（新拖进来的）按名字排在后面。
pub fn list_items(id: &str, order: &[String]) -> Vec<Item> {
    let dir = box_dir(id);
    let mut items: Vec<Item> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() && is_shortcut(&p) {
                items.push(to_item(&p));
            }
        }
    }
    let rank = |f: &str| -> usize {
        order.iter().position(|x| x == f).unwrap_or(usize::MAX)
    };
    items.sort_by(|a, b| {
        rank(&a.file)
            .cmp(&rank(&b.file))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    items
}

/// 只要数量：不抽图标，右键菜单/删除校验用，避免一次 SHGetFileInfo 风暴
pub fn count_items(id: &str) -> usize {
    let dir = box_dir(id);
    let mut n = 0;
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() && is_shortcut(&p) {
                n += 1;
            }
        }
    }
    n
}

/// 收纳：把桌面（或任意位置）的快捷方式真正移动进盒子目录
pub fn import(id: &str, paths: &[String]) -> (Vec<Item>, Vec<String>) {
    let dir = box_dir(id);
    let mut added = Vec::new();
    let mut skipped = Vec::new();

    for raw in paths {
        let src = PathBuf::from(raw);
        if !src.exists() {
            skipped.push(format!("{}：文件不存在", raw));
            continue;
        }
        if !src.is_file() {
            skipped.push(format!("{}：不是文件", raw));
            continue;
        }
        if !is_shortcut(&src) {
            skipped.push(format!(
                "{}：只收纳快捷方式（.lnk / .url），真实文件不会被移动",
                src.file_name().and_then(|s| s.to_str()).unwrap_or(raw)
            ));
            continue;
        }
        if src.parent() == Some(dir.as_path()) {
            skipped.push(format!("{}：已在盒子里", raw));
            continue;
        }
        let file_name = src.file_name().and_then(|s| s.to_str()).unwrap_or("item.lnk");
        let dst = unique_target(&dir, file_name);
        // 先把文件搬走，立刻通知 shell 源路径已消失 —— 桌面图标随即消失，
        // 不然后面抽图标的耗时会让「拖进去」看起来像是卡了 1 秒。
        let moved = fs::rename(&src, &dst).is_ok()
            || (fs::copy(&src, &dst).is_ok() && fs::remove_file(&src).is_ok());
        if moved {
            desktop::notify_gone(&src);
            added.push(to_item(&dst));
        } else {
            skipped.push(format!("{file_name}：移动失败（可能被占用）"));
        }
    }
    (added, skipped)
}

/// 还原：把盒子里的快捷方式移回桌面
pub fn restore_to_desktop(id: &str, file: &str) -> Result<(), String> {
    let src = box_dir(id).join(file);
    if !src.exists() {
        return Err("文件不存在".into());
    }
    let desktop = desktop::desktop_dir();
    let dst = unique_target(&desktop, file);
    let r = fs::rename(&src, &dst)
        .or_else(|_| fs::copy(&src, &dst).map(|_| ()).and_then(|_| fs::remove_file(&src)))
        .map_err(|e| format!("移动回桌面失败：{e}"))?;
    desktop::notify_appeared(&dst);
    Ok(r)
}

pub fn remove_from_box(id: &str, file: &str) -> Result<(), String> {
    let p = box_dir(id).join(file);
    if !p.exists() {
        return Err("文件不存在".into());
    }
    fs::remove_file(&p).map_err(|e| format!("删除失败：{e}"))
}
