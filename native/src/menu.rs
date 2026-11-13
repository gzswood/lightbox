//! 右键菜单。
//!
//! 菜单项都带一个动作标签（`Act`），弹出之后按动作派发 —— 不用记「第几项是
//! 哪个功能」，加一项菜单也不会把后面的下标全带偏。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYBD_EVENT_FLAGS, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetForegroundWindow,
    SetForegroundWindow, TrackPopupMenuEx, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
};

use crate::app::app;
use crate::config::{self, Item};
use crate::{autostart, desktop, log};

const ID_BASE: usize = 1000;

/// 菜单正开着。菜单是独立窗口，鼠标钩子看到菜单上的点击会以为
/// 「点了面板外面」把面板关了 —— 用这个标记挡一下。
static MENU_OPEN: AtomicBool = AtomicBool::new(false);

/// 菜单是不是正开着
pub fn is_open() -> bool {
    MENU_OPEN.load(Ordering::Relaxed)
}

/// 菜单项触发的动作
#[derive(Clone, Copy, PartialEq, Debug)]
enum Act {
    // 长条
    BoxSettings,
    ToggleTopmost,
    ToggleDesktopMode,
    Autostart,
    DeleteBox,
    NewBox,
    GlobalSettings,
    OpenDataDir,
    SetBoxIcon,
    ResetBoxIcon,
    /// 退出程序
    Exit,
    // 面板头部
    SortByName,
    DissolveBox,
    // 面板里的图标
    OpenItems,
    RevealItem,
    RestoreToDesktop,
    RemoveFromBox,
    Properties,
}

/// 菜单项。`disabled` 的项（比如「盒子名 / 共 N 个图标」这种抬头）画成灰的，
/// 键盘上下键会跳过它们 —— 免得用户按两下箭头才走到真正的操作项。
struct MenuItem {
    text: String,
    checked: bool,
    disabled: bool,
    act: Option<Act>,
}

fn item(text: impl Into<String>, act: Act) -> MenuItem {
    MenuItem {
        text: text.into(),
        checked: false,
        disabled: false,
        act: Some(act),
    }
}

fn header(text: impl Into<String>) -> MenuItem {
    MenuItem {
        text: text.into(),
        checked: false,
        disabled: true,
        act: None,
    }
}

/// 分隔线（文字为空即视为分隔线）
fn sep() -> MenuItem {
    MenuItem {
        text: String::new(),
        checked: false,
        disabled: false,
        act: None,
    }
}

impl MenuItem {
    fn checked(mut self, on: bool) -> Self {
        self.checked = on;
        self
    }
}

/// 把本进程提到前台。
///
/// 长条是 WS_EX_NOACTIVATE 的：点它**不会**激活本进程，所以直接
/// SetForegroundWindow 会被系统拒绝，菜单也就拿不到键盘焦点（方向键选不了项）。
/// 「按一下假 ALT」是系统认可的通行办法 —— 系统看到 ALT 键事件就放开前台权限。
pub fn grab_foreground(hwnd: HWND) {
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        if GetForegroundWindow() != hwnd {
            let _ = keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            let _ = keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0x0002), 0);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// 在光标处弹一个菜单，返回被选中的动作（None = 用户取消）
fn popup(items: &[MenuItem]) -> Option<Act> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (i, it) in items.iter().enumerate() {
            if it.text.is_empty() {
                let _ = AppendMenuW(
                    menu,
                    MF_SEPARATOR,
                    0,
                    &windows::core::HSTRING::from(""),
                );
                continue;
            }
            let mut flags = MF_STRING;
            if it.checked {
                flags |= MF_CHECKED;
            }
            if it.disabled {
                flags |= MF_GRAYED | MF_DISABLED;
            }
            let _ = AppendMenuW(
                menu,
                flags,
                ID_BASE + i,
                &windows::core::HSTRING::from(it.text.as_str()),
            );
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 用隐藏的宿主窗口当菜单 owner：长条是 WS_EX_NOACTIVATE 的，
        // 拿它当 owner 菜单拿不到键盘焦点。
        let owner = app().hwnd.lock().unwrap().unwrap_or_default();
        grab_foreground(owner);
        MENU_OPEN.store(true, Ordering::Relaxed);
        let cmd = TrackPopupMenuEx(
            menu,
            (TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY).0,
            pt.x,
            pt.y,
            owner,
            None,
        );
        MENU_OPEN.store(false, Ordering::Relaxed);
        let _ = DestroyMenu(menu);
        if cmd.0 == 0 {
            return None;
        }
        let idx = cmd.0 as usize - ID_BASE;
        items.get(idx).and_then(|it| it.act)
    }
}

/// 长条上某个分段的右键菜单
pub fn box_menu(idx: usize) {
    let cfg = app().cfg.lock().unwrap().clone();
    let Some(b) = cfg.boxes.get(idx) else { return };
    let name = b.name.clone();
    let id = b.id.clone();
    let topmost = app().cfg.lock().unwrap().bar.topmost;
    let desktop = app().cfg.lock().unwrap().bar.desktop;
    let count = config::count_items(&id);

    let picked = popup(&[
        header(name),
        header(format!("共 {count} 个图标")),
        sep(),
        item("设置图标…", Act::SetBoxIcon),
        item("恢复默认图标", Act::ResetBoxIcon),
        sep(),
        item("置顶", Act::ToggleTopmost).checked(topmost),
        item(
            if desktop { "嵌入任务栏" } else { "桌面模式" },
            Act::ToggleDesktopMode,
        ),
        item("开机自启", Act::Autostart).checked(autostart::enabled()),
        item("设置…", Act::BoxSettings),
        sep(),
        item("新建收纳盒", Act::NewBox),
        item("解散收纳盒（图标放回桌面）", Act::DissolveBox),
        item("删除收纳盒", Act::DeleteBox),
        sep(),
        item("退出", Act::Exit),
    ]);
    match picked {
        Some(Act::BoxSettings) => {
            crate::settings::open(Some(&id));
        }
        Some(Act::ToggleTopmost) => {
            let mut g = app().cfg.lock().unwrap();
            g.bar.topmost = !g.bar.topmost;
            config::save(&g);
            drop(g);
            crate::bar::refresh();
        }
        Some(Act::ToggleDesktopMode) => {
            let mut g = app().cfg.lock().unwrap();
            g.bar.desktop = !g.bar.desktop;
            let desktop = g.bar.desktop;
            if desktop {
                // 刚切到桌面模式：清掉旧坐标，让长条落到默认位置
                // （任务栏上边缘、屏幕水平中间）
                g.bar.x = 0;
                g.bar.y = 0;
            }
            config::save(&g);
            drop(g);
            crate::bar::refresh();
            log::info(if desktop {
                "已切换到桌面模式"
            } else {
                "已嵌入任务栏"
            });
        }
        Some(Act::Autostart) => toggle_autostart(),
        Some(Act::SetBoxIcon) => {
            set_box_icon(&id);
        }
        Some(Act::ResetBoxIcon) => {
            let mut g = app().cfg.lock().unwrap();
            let Some(b) = g.boxes.iter_mut().find(|b| b.id == id) else {
                return;
            };
            if b.icon.is_none() {
                return;
            }
            b.icon = None;
            config::save(&g);
            drop(g);
            crate::bar::refresh();
            crate::panel::reload();
        }
        Some(Act::NewBox) => add_box(),
        Some(Act::DissolveBox) => {
            if count == 0 {
                return;
            }
            let n = config::restore_all(&id);
            crate::bar::refresh();
            crate::panel::reload();
            log::info(&format!("解散收纳盒：{n} 个图标已放回桌面"));
        }
        Some(Act::DeleteBox) => {
            if count > 0 {
                warn(&format!(
                    "这个收纳盒里还有 {count} 个图标。\n请先「解散收纳盒」把图标放回桌面，再删除。"
                ));
                return;
            }
            let mut g = app().cfg.lock().unwrap();
            config::remove_box(&mut g, &id);
            drop(g);
            crate::bar::refresh();
        }
        Some(Act::Exit) => {
            crate::app::quit();
        }
        _ => {}
    }
}

/// 切换开机自启（盒子和托盘菜单共用）
fn toggle_autostart() {
    let want = !autostart::enabled();
    match autostart::set(want) {
        Ok(()) => log::info(&format!("开机自启已{}", if want { "打开" } else { "关闭" })),
        Err(e) => {
            log::info(&format!("开机自启设置失败：{e}"));
            warn(&format!("开机自启设置失败：{e}"));
        }
    }
}

/// 给收纳盒挑一个自定义图标（只接受 .ico）。
///
/// 流程：系统文件对话框（主线程、模态）→ 校验扩展名 → 用 `LoadImageW` 把 .ico
/// 转成 64×64 的 PNG data URL 存进配置。存 data URL 而不是存路径，是因为渲染
/// 层要的是可以直接贴到画布上的位图，存路径就得多一层「读文件 + 失败兜底」。
fn set_box_icon(id: &str) {
    // 盒子可能已经被删了 —— 先探一下，没这个盒子就别弹对话框
    {
        let cfg = app().cfg.lock().unwrap();
        if !cfg.boxes.iter().any(|b| b.id == id) {
            return;
        }
    }
    let owner = app().hwnd.lock().unwrap().unwrap_or_default();
    let Some(path) = crate::iconpick::pick_ico(owner, "选择收纳盒图标（.ico）") else {
        return; // 用户取消
    };
    // 第二道过滤：用户可以在对话框里手打文件名绕过过滤器
    if !path.is_file() {
        warn("文件不存在");
        return;
    }
    if !crate::iconpick::is_ico(&path) {
        warn("只支持 .ico 图标文件");
        return;
    }
    let Some(url) = crate::icons::ico_file_data_url(&path, 64) else {
        warn("这个 .ico 文件读不出图标（可能已损坏）");
        return;
    };
    {
        let mut g = app().cfg.lock().unwrap();
        let Some(b) = g.boxes.iter_mut().find(|b| b.id == id) else {
            return;
        };
        b.icon = Some(url);
        config::save(&g);
    }
    crate::bar::refresh();
    crate::panel::reload();
}

/// 一个简短的提示框（菜单已经收起来了，只能靠系统提示框告知用户）
fn warn(text: &str) {
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
            None,
            &windows::core::HSTRING::from(text),
            &windows::core::HSTRING::from("LightBox"),
            windows::Win32::UI::WindowsAndMessaging::MB_OK
                | windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
        );
    }
}

/// 长条空白处（非分段）的右键菜单
pub fn bar_menu() {
    let picked = popup(&[
        item("新建收纳盒", Act::NewBox),
        sep(),
        item("设置…", Act::GlobalSettings),
        sep(),
        item("打开数据目录", Act::OpenDataDir),
        sep(),
        item("退出", Act::Exit),
    ]);
    match picked {
        Some(Act::NewBox) => add_box(),
        Some(Act::GlobalSettings) => {
            crate::settings::open(None);
        }
        Some(Act::OpenDataDir) => {
            let dir = config::root_dir();
            let _ = desktop::shell_open(&dir.to_string_lossy());
        }
        Some(Act::Exit) => {
            crate::app::quit();
        }
        _ => {}
    }
}

/// 新建一个收纳盒，长条随之多一个分段
pub fn add_box() {
    let mut g = app().cfg.lock().unwrap();
    let n = g.boxes.len() + 1;
    let name = format!("收纳盒 {n}");
    config::add_box(&mut g, &name);
    drop(g);
    crate::bar::refresh();
    // 新盒子在长条末尾，滚过去让它露出来
    crate::bar::scroll_to_end();
}

/// 面板头部的「⋯」菜单：只放和「这一盒的内容」有关的两个动作。
/// 盒子本身的增删改在长条分段的右键菜单里。
pub fn panel_header_menu(box_id: &str) {
    let count = config::count_items(box_id);
    let picked = popup(&[
        item("按名称排序", Act::SortByName),
        item("解散收纳盒", Act::DissolveBox),
    ]);
    match picked {
        Some(Act::SortByName) => {
            let mut g = app().cfg.lock().unwrap();
            config::sort_by_name(&mut g, box_id);
            drop(g);
            crate::panel::reload();
            log::info(&format!("盒子 {box_id} 已按名称排序"));
        }
        Some(Act::DissolveBox) => {
            if count == 0 {
                return;
            }
            let n = config::restore_all(box_id);
            crate::bar::refresh();
            crate::panel::reload();
            log::info(&format!("解散收纳盒：{n} 个图标已放回桌面"));
        }
        _ => {}
    }
}

/// 面板里图标的右键菜单：对选中集生效（单选一个、多选一批，同一套菜单）
pub fn items_menu(items: &[Item]) {
    let n = items.len();
    if n == 0 {
        return;
    }
    let mut menu: Vec<MenuItem> = Vec::new();
    if n == 1 {
        menu.push(item(format!("打开 {}", items[0].name), Act::OpenItems));
        menu.push(sep());
        menu.push(item("打开文件所在位置", Act::RevealItem));
    } else {
        menu.push(item(format!("打开全部（{n}）"), Act::OpenItems));
    }
    menu.push(sep());
    if n > 1 {
        menu.push(item(format!("放回桌面（{n}）"), Act::RestoreToDesktop));
        menu.push(item(format!("从盒子移除（{n}）"), Act::RemoveFromBox));
    } else {
        menu.push(item("放回桌面", Act::RestoreToDesktop));
        menu.push(item("从盒子移除", Act::RemoveFromBox));
    }
    if n == 1 {
        menu.push(sep());
        menu.push(item("属性", Act::Properties));
    }

    let picked = popup(&menu);
    match picked {
        Some(Act::OpenItems) => {
            let mut ok = 0;
            for it in items {
                match crate::shortcut::open(&it.path) {
                    Ok(()) => ok += 1,
                    Err(e) => log::info(&format!("启动失败：{}（{e}）", it.path)),
                }
            }
            if ok > 0 {
                crate::panel::close();
            }
        }
        Some(Act::RevealItem) => {
            if let Err(e) = crate::shortcut::reveal(&items[0].path) {
                log::info(&format!("打开资源管理器失败：{e}"));
            }
        }
        Some(Act::RestoreToDesktop) => {
            let Some(id) = items.iter().find_map(|it| find_box_id(&it.path)) else {
                return;
            };
            let mut done = 0;
            for it in items {
                match config::restore_to_desktop(&id, &it.file) {
                    Ok(()) => done += 1,
                    Err(e) => log::info(&format!("放回桌面失败：{e}")),
                }
            }
            crate::bar::refresh();
            crate::panel::reload();
            log::info(&format!("已放回 {done} 个图标到桌面"));
        }
        Some(Act::RemoveFromBox) => {
            let Some(id) = items.iter().find_map(|it| find_box_id(&it.path)) else {
                return;
            };
            let mut done = 0;
            for it in items {
                match config::remove_from_box(&id, &it.file) {
                    Ok(()) => done += 1,
                    Err(e) => log::info(&format!("删除失败：{e}")),
                }
            }
            crate::panel::reload();
            log::info(&format!("已从盒子移除 {done} 个图标"));
        }
        Some(Act::Properties) => {
            if let Err(e) = crate::shortcut::properties(&items[0].path) {
                log::info(&format!("打开属性失败：{e}"));
            }
        }
        _ => {}
    }
}

/// 这个路径属于哪个盒子
fn find_box_id(path: &str) -> Option<String> {
    let cfg = app().cfg.lock().unwrap().clone();
    let p = std::path::Path::new(path);
    cfg.boxes
        .iter()
        .find(|b| p.starts_with(config::box_dir(&b.id)))
        .map(|b| b.id.clone())
}
