//! 配置与收纳库：每个收纳盒对应磁盘上一个真实目录，收纳 = 把 .lnk 移进来。
//!
//! 磁盘格式与旧版（v0.9.x）保持兼容：读旧写新，用户原有的盒子、图标、排序都能直接用。
//! 唯一的变化是**盒子从「各自独立摆放」变成「一条长条」**：长条的位置、是否嵌进任务栏
//! 记在 `Config.bar` 里，盒子的 `x/y` 只作为旧配置读取时的迁移来源。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::desktop;
use crate::icons;

/// 一个收纳盒（长条上的一个分段）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BoxConfig {
    pub id: String,
    pub name: String,
    /// 旧版：盒子自己所在的绝对屏幕坐标。新版长条统一位置，这两个字段只保留着
    /// 让旧配置能读进来（迁移时用来推导长条位置）。
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub topmost: bool,
    /// 旧版：这个盒子是否单独嵌在任务栏里。新版整条只嵌一次，长条固定住任务栏。
    /// 这个字段只为读旧配置而保留（迁移时用来推导长条的初始位置）。
    #[serde(default)]
    pub in_bar: bool,
    /// 自定义盒子图标（png data URL）；为空时画内置图标
    #[serde(default)]
    pub icon: Option<String>,
    /// 面板里的自定义顺序（存文件名）。空 = 按名称排。用户拖动图标后写入。
    #[serde(default)]
    pub order: Vec<String>,
}

/// 长条自身的状态
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BarConfig {
    /// 长条左上角的绝对屏幕坐标（**物理**像素）。只有「任务栏不是底部横条」
    /// 这条降级路径才用得上 —— 正常情况下长条在任务栏里，位置由任务栏决定。
    pub x: i32,
    pub y: i32,
    /// 长条是否总在最前。旧版是每个盒子各有一个置顶开关，新版只有一条长条，
    /// 所以置顶是整条的属性 —— 迁移时取「任意旧盒子置顶」为准。
    #[serde(default)]
    pub topmost: bool,
    /// 横向滚动位置：**第一个可见分段的序号**（分段太多放不下时用）
    #[serde(default)]
    pub scroll: i32,
    /// 长条在任务栏里往右挪了多少（**物理**像素，相对「贴着任务栏左侧」的默认位置）。
    /// 用户在任务栏里左右拖长条时改这个值。
    ///
    /// 基准是**左边**而不是托盘：托盘里的图标/时钟宽度会变（新程序装托盘、
    /// 隐藏图标点开），拿它当基准长条就会跟着左右跑。任务栏左边除了用户自己
    /// 固定/取消固定图标之外基本不动。
    #[serde(default)]
    pub drag_dx: i32,
    /// 主界面浮在桌面上（true）还是嵌进任务栏（false）。默认嵌任务栏。
    #[serde(default)]
    pub desktop: bool,
}

/// 盒子坐标是「在哪块显示器布局下记下来的」的参照系（**物理像素**）。
///
/// 配置里的 x/y 一直是绝对屏幕坐标，可两台屏的工作区高度、缩放都不一样：
/// 笔记本屏（1920×1200 @125%、盒边 100、工作区底 1150）上贴着任务栏上沿的 y=1050，
/// 到外接 1920×1080 @100%（盒边 80、工作区底 1040）就成了整排压进任务栏里。
/// 记下当时的「工作区矩形 + 缩放」，显示配置一变就按它把坐标整体换算过去。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutRef {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 缩放 ×100（125% → 125）。存整数，避免浮点比较抖动导致反复重排。
    pub scale_100: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub boxes: Vec<BoxConfig>,
    /// 长条状态（位置 / 是否嵌任务栏 / 滚动偏移）
    #[serde(default)]
    pub bar: BarConfig,
    /// 弹出面板背景的**透明度百分比**（0 = 完全不透明，50 = 最透）。数字越大越透明。
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
    /// 上一次记下长条坐标时的显示器参照系。老配置没有这一项 → 首次运行写入当前值，
    /// 从下一次切屏开始生效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_ref: Option<LayoutRef>,
    /// 弹出面板文字的字体名（系统已安装的字体 face name）。
    /// 老配置没有这一项 → 默认「微软雅黑」。
    #[serde(default = "default_panel_font")]
    pub panel_font: String,
    /// 弹出面板文字的基准字号（96 DPI 下的 px，默认 12 = 原来图标名写死的大小）。
    /// 面板里其它文字按「原字号 / 12 × 这个值」等比缩放，标题/标签/提示的层级不变。
    #[serde(default = "default_panel_font_size")]
    pub panel_font_size: f32,
    /// 打印诊断信息：把排障日志写进 `lightbox.log`。
    ///
    /// 默认**关闭** —— 关着的时候一个字节都不落盘（连日志文件都不创建）。
    /// 只有用户在「设置 → 功能设置」里主动打开，才记录启动、出错和一次性动作。
    #[serde(default)]
    pub diagnostics: bool,
}

/// 弹窗字体设置的默认与取值范围。
/// 默认用中文名「微软雅黑」：和设置页下拉框里看到的、用户认得的名字一致
/// （GDI 的 CreateFontW / EnumFontFamiliesExW 对中文名和英文名都能识别）。
pub fn default_panel_font() -> String {
    "微软雅黑".into()
}
pub fn default_panel_font_size() -> f32 {
    FONT_SIZE_BASE
}
pub const FONT_SIZE_BASE: f32 = 12.0;
pub const FONT_SIZE_MIN: f32 = 10.0;
pub const FONT_SIZE_MAX: f32 = 20.0;

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
/// 4 = 原生重写：盒子合成一条长条，位置/嵌任务栏移到 `bar`
/// 5 = 长条改为以任务栏左侧为基准（drag_dx 归零重算），新增「桌面模式」
pub const CONFIG_VERSION: u32 = 5;

fn default_version() -> u32 {
    CONFIG_VERSION
}

/// 老配置的迁移：把仍然是**旧默认**的项换成新默认，并把「多盒子」模型换成「长条」。
///
/// 只按版本号走一次，不逐项比对 —— 用户明确要求「把这批默认值固定下来」，
/// 升级后就该看到新的默认外观。想找回原来的值：`config.before-v<N>.json`
/// 是迁移前留的备份。
fn migrate(cfg: &mut Config) -> bool {
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
    if from < 4 {
        // 旧模型：N 个盒子各自摆着（其中一些嵌在任务栏里）。
        // 新模型：一条长条，固定住在任务栏里（一个胶囊）。
        // 位置只留作降级路径（任务栏不是底部横条时浮在桌面上）的初始值，
        // 所以取「还在桌面上」的那个盒子（优先），否则取第一个。
        let pick = cfg
            .boxes
            .iter()
            .find(|b| !b.in_bar)
            .or_else(|| cfg.boxes.first());
        if let Some(b) = pick {
            cfg.bar.x = b.x;
            cfg.bar.y = b.y;
        }
        // 旧版每个盒子各有一个置顶开关；新版本只有一条长条，置顶升级为整条的属性
        cfg.bar.topmost = cfg.boxes.iter().any(|b| b.topmost);
    }
    if from < 5 {
        // 定位基准从「右端贴托盘」改成「左端贴任务栏左侧」：
        // 旧 drag_dx 记的是相对托盘的左偏量，放到新基准下语义完全不同，
        // 直接归零（回到左基准默认位置）最不容易让人困惑。
        cfg.bar.drag_dx = 0;
        cfg.bar.desktop = false;
    }
    cfg.version = CONFIG_VERSION;
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            boxes: Vec::new(),
            bar: BarConfig::default(),
            panel_transparency: default_transparency(),
            box_bg: default_box_bg(),
            panel_bg: default_panel_bg(),
            panel_acrylic: default_acrylic(),
            panel_radius: false,
            bar_hide_label: default_bar_hide_label(),
            layout_ref: None,
            panel_font: default_panel_font(),
            panel_font_size: default_panel_font_size(),
            diagnostics: false,
        }
    }
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
/// 目的只有一个：**这次运行里不管发生什么（误拖、崩溃），用户都还能拿回
/// 启动那一刻的盒子列表**。历史上出过一次「盒子莫名少两个」，就是这么丢的。
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
            // 默认外观/模型变过（CONFIG_VERSION 涨了）：迁移一次并立刻落盘，
            // 否则用户升级后看到的还是旧模型，跟新版本对不上。
            if migrate(&mut cfg) {
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
        return Config::default();
    }
    // 这里只剩「目录里连 config.json 都没有」= 真正的第一次运行。
    // 建两个默认盒并立刻落盘：任务栏里从第一次启动就有两段，和 0.9.33 一致，
    // 新用户不会看到一条空长条。顺带把 boxes\<id> 目录建好，绿色版目录结构一次到位。
    //
    // ⚠️ 只在「文件不存在」时走这里。老配置升级、或者用户把盒子全删了，
    // 文件都在，一律不补 —— 否则用户删掉的盒子会自己长回来。
    let mut cfg = Config::default();
    seed_default_boxes(&mut cfg);
    save(&cfg);
    for b in &cfg.boxes {
        let _ = fs::create_dir_all(root_dir().join("boxes").join(&b.id));
    }
    cfg
}

/// 首次运行的两个默认盒：名字沿用 0.9.33（「系统工具」「办公工具」）
fn seed_default_boxes(c: &mut Config) {
    for name in ["系统工具", "办公工具"] {
        c.boxes.push(BoxConfig {
            id: format!("b{}", uuid::Uuid::new_v4().simple()),
            name: name.to_string(),
            x: c.bar.x,
            y: c.bar.y,
            topmost: false,
            in_bar: false,
            icon: None,
            order: Vec::new(),
        });
    }
}

/// 只读不迁移：把磁盘上的配置原样解析出来。
///
/// 给 `--dump-config` 用 —— 排障时要看的是「文件里现在是什么」，
/// 不能顺手把迁移跑掉（那样看到的永远是迁移后的结果）。
pub fn load_raw() -> Config {
    let p = config_path();
    if let Ok(text) = fs::read_to_string(&p) {
        if let Some(cfg) = parse_config(&text) {
            return cfg;
        }
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
    // 盒子不再各自摆放：x/y 统一成长条的位置。这两个字段只是为了旧配置能读、
    // 旧版本能回退，写出去时保持一致，免得留下互相矛盾的值。
    let mut out = cfg.clone();
    for b in &mut out.boxes {
        b.x = out.bar.x;
        b.y = out.bar.y;
        b.in_bar = false;
    }
    if let Ok(text) = serde_json::to_string_pretty(&out) {
        let tmp = p.with_extension("json.tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
    }
}

/// 新建一个盒子（长条新增一个分段）
pub fn add_box(cfg: &mut Config, name: &str) -> String {
    let id = format!("b{}", uuid::Uuid::new_v4().simple());
    cfg.boxes.push(BoxConfig {
        id: id.clone(),
        name: name.to_string(),
        x: cfg.bar.x,
        y: cfg.bar.y,
        topmost: false,
        in_bar: false,
        icon: None,
        order: Vec::new(),
    });
    save(cfg);
    id
}

/// 删除盒子：连带删掉收纳目录
pub fn remove_box(cfg: &mut Config, id: &str) {
    cfg.boxes.retain(|b| b.id != id);
    let dir = root_dir().join("boxes").join(id);
    let _ = fs::remove_dir_all(dir);
    save(cfg);
}

#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub file: String,
    pub path: String,
    /// 图标位图（BGBA，非预乘）
    pub icon: Option<crate::render::IconBmp>,
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
        // 64 像素：面板上的图标边长是 48 逻辑像素（100% 缩放），
        // 抽大一点才经得起放大不会糊
        icon: icons::icon_bmp(p, 64),
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
        rank(&a.file).cmp(&rank(&b.file)).then_with(|| crate::collate::cmp_name(&a.name, &b.name))
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
    // Explorer 有概率不处理逐条的删除通知，桌面图标会赖着不走 —— 补一次刷新
    if !added.is_empty() {
        desktop::schedule_refresh();
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
    desktop::schedule_refresh();
    Ok(r)
}

pub fn remove_from_box(id: &str, file: &str) -> Result<(), String> {
    let p = box_dir(id).join(file);
    if !p.exists() {
        return Err("文件不存在".into());
    }
    fs::remove_file(&p).map_err(|e| format!("删除失败：{e}"))
}

/// 写回盒子的自定义顺序（用户在面板里拖动排序后调用）
pub fn save_order(cfg: &mut Config, id: &str, order: &[String]) {
    if let Some(b) = cfg.boxes.iter_mut().find(|b| b.id == id) {
        b.order = order.to_vec();
    }
    save(cfg);
}

/// 按名称重排盒子里的图标（面板「按名称排序」）。
///
/// 走的是和手动拖动同一条路：把排好的文件名写进 `order` 表 —— 用户之后再拖动
/// 就以这份顺序为起点，两种排序不会互相打架。
pub fn sort_by_name(cfg: &mut Config, id: &str) {
    let items = list_items(id, &[]);
    let order: Vec<String> = items.into_iter().map(|i| i.file).collect();
    save_order(cfg, id, &order);
}

/// 解散收纳盒：盒子里所有快捷方式移回桌面，**盒子本身留着**（变成空盒）。
/// 返回移回去的个数。
pub fn restore_all(id: &str) -> usize {
    let dir = box_dir(id);
    let desktop = desktop::desktop_dir();
    let mut n = 0;
    let Ok(rd) = fs::read_dir(&dir) else {
        return 0;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() || !is_shortcut(&p) {
            continue;
        }
        let Some(file) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let dst = unique_target(&desktop, file);
        let moved = fs::rename(&p, &dst).is_ok()
            || (fs::copy(&p, &dst).is_ok() && fs::remove_file(&p).is_ok());
        if moved {
            desktop::notify_appeared(&dst);
            n += 1;
        }
    }
    if n > 0 {
        desktop::schedule_refresh();
    }
    n
}

/// 盒子里的文件被拖走或删掉之后，顺序表里已经不存在的条目要清掉，
/// 否则那个位置会一直空着。返回有没有改动。
pub fn prune_order(cfg: &mut Config, id: &str) -> bool {
    let dir = box_dir(id);
    let Some(b) = cfg.boxes.iter_mut().find(|b| b.id == id) else {
        return false;
    };
    let before = b.order.len();
    b.order.retain(|f| dir.join(f).is_file());
    before != b.order.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 首启两个默认盒_名字和数量固定() {
        let mut c = Config::default();
        assert!(c.boxes.is_empty(), "默认构造本身应该是空列表");
        seed_default_boxes(&mut c);
        assert_eq!(c.boxes.len(), 2);
        let names: Vec<&str> = c.boxes.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["系统工具", "办公工具"]);
        // id 必须唯一，否则两个盒子会共用同一个 boxes\<id> 目录
        assert_ne!(c.boxes[0].id, c.boxes[1].id);
        assert!(c.boxes.iter().all(|b| b.id.starts_with('b') && b.id.len() > 1));
    }

    #[test]
    fn 诊断日志默认关_老配置没有这个键也读成关() {
        // 新装默认关 —— 用户不主动打开就不该在磁盘上留日志
        assert!(!Config::default().diagnostics);
        // 老版本写出来的 config.json 没有 diagnostics 键，反序列化要用默认值 false
        let old = r##"{
            "version": 5,
            "boxes": [],
            "bar": {"x": 0, "y": 0, "topmost": false, "scroll": 0, "drag_dx": 0, "desktop": false},
            "panel_transparency": 0.0,
            "box_bg": "#5A606B",
            "panel_bg": "#5A606B",
            "panel_acrylic": false,
            "panel_radius": false,
            "bar_hide_label": false
        }"##;
        let cfg: Config = serde_json::from_str(old).expect("老配置应当能读进来");
        assert!(!cfg.diagnostics);
        // 显式写 true 时要能读成 true
        let on = old.replace("\"bar_hide_label\": false", "\"bar_hide_label\": false, \"diagnostics\": true");
        let cfg: Config = serde_json::from_str(&on).expect("带 diagnostics 的配置应当能读进来");
        assert!(cfg.diagnostics);
    }
}
