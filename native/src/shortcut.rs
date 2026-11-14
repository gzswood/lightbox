//! 打开快捷方式：先把 .lnk 解析成「目标程序 + 参数 + 工作目录」再启动，
//! 而不是把 .lnk 直接丢给 ShellExecute。
//!
//! 为什么要多这一步：
//! 1. **工作目录**。.lnk 里没记工作目录时，ShellExecute 会把「调用方」的当前目录
//!    传给新进程。LightBox 被从别处启动时 cwd 可能是任意目录，于是那些依赖 cwd
//!    的软件（Electron 一类最常见）会在启动阶段直接退出，表现出来就是「点了没反应」。
//!    这里显式给目标程序自己的目录，和资源管理器双击的行为对齐。
//! 2. **失效能被发现**。目标程序已经不在了（卸载 / 换盘）时，可以明确告诉用户，
//!    而不是弹一个系统错误框或者干脆什么都不发生。
//! 3. **参数**。个别快捷方式靠参数区分模式（--silent、--profile 之类），
//!    解析出来原样传下去。
//!
//! 解析失败（.url、自定义协议、指向 UWP 应用的 .lnk）就退回原来的做法：
//! 把文件本身交给 ShellExecute。

use std::path::{Path, PathBuf};

use windows::core::{BOOL, Interface};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, TRUE, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::System::Environment::{FreeEnvironmentStringsW, GetEnvironmentStringsW};
use windows::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcessId, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTF_USESHOWWINDOW, STARTUPINFOW,
};
use windows::Win32::UI::Controls::{TCIF_TEXT, TCITEMW, TCM_GETITEMRECT, TCM_GETITEMW};
use windows::Win32::UI::Shell::{IShellLinkW, ShellExecuteW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetWindowThreadProcessId, IsWindowVisible,
    SendMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, SW_SHOWNORMAL,
};

use crate::util::wide;

/// 一个快捷方式解析出来的内容
#[derive(Debug, Clone, Default)]
pub struct Shortcut {
    /// 目标程序（可能是 exe，也可能是文档）
    pub target: String,
    /// 参数
    pub args: String,
    /// 工作目录（.lnk 自己记的）
    pub workdir: Option<String>,
}

/// 解析 .lnk。失败返回 None（调用方退回直接丢给 shell）。
pub fn resolve_lnk(path: &Path) -> Option<Shortcut> {
    let is_lnk = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("lnk"))
        .unwrap_or(false);
    if !is_lnk {
        return None;
    }

    unsafe {
        // 线程可能已经在别的套间里（RPC_E_CHANGED_MODE），那种情况下不能反初始化
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let uninit = hr.is_ok();

        let out = resolve_inner(path);

        if uninit {
            CoUninitialize();
        }
        out
    }
}

unsafe fn resolve_inner(path: &Path) -> Option<Shortcut> {
    let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
    let persist: IPersistFile = link.cast().ok()?;
    let wpath = wide(&path.to_string_lossy());
    persist
        .Load(windows::core::PCWSTR(wpath.as_ptr()), STGM_READ)
        .ok()?;

    let mut sc = Shortcut::default();

    let mut buf = vec![0u16; 2048];
    if link.GetPath(&mut buf, std::ptr::null_mut(), 0).is_ok() {
        sc.target = take_str(&buf).unwrap_or_default();
    }
    if sc.target.is_empty() {
        return None;
    }

    let mut abuf = vec![0u16; 2048];
    if link.GetArguments(&mut abuf).is_ok() {
        sc.args = take_str(&abuf).unwrap_or_default();
    }
    let mut wbuf = vec![0u16; 2048];
    if link.GetWorkingDirectory(&mut wbuf).is_ok() {
        sc.workdir = take_str(&wbuf);
    }
    Some(sc)
}

/// 从 COM 返回的宽字符串里取内容（去掉尾部 NUL 和空白）
fn take_str(buf: &[u16]) -> Option<String> {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    if end == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..end]);
    let s = s.trim_matches('\u{0}').trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 构造一份「干净」的环境块：去掉 ELECTRON_* / NODE_OPTIONS。
///
/// 为什么必须做这件事：Electron 应用会读 `ELECTRON_RUN_AS_NODE`，一旦这个变量存在，
/// 它们就不再启动界面，而是退化成纯 Node 进程然后立刻退出 —— 用户看到的就是
/// 「点了一下，什么都没发生」。这类变量只对「我们想启动的那个程序」有害，
/// 所以启动前统一滤掉。
unsafe fn clean_env_block() -> Vec<u16> {
    let raw = GetEnvironmentStringsW();
    let mut items: Vec<String> = Vec::new();
    if !raw.is_null() {
        // GetEnvironmentStringsW 返回的是**一整块**「变量名=值\0…\0\0」，
        // 必须自己按双 NUL 扫。不能图省事用 as_wide() —— 它只扫到第一个 NUL，
        // 于是环境块里只剩**第一个**变量，子进程拿不到 PATH / APPDATA / USERPROFILE，
        // 启动后就地秒退（表现成「点了图标没反应」）。
        let base: *const u16 = raw.0;
        let mut i = 0isize;
        loop {
            let start = base.offset(i);
            if *start == 0 {
                break;
            }
            let mut len = 0isize;
            while *base.offset(i + len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(start, len as usize));
            i += len + 1;
            let upper = s.to_ascii_uppercase();
            if upper.starts_with("ELECTRON_") || upper.starts_with("NODE_OPTIONS=") {
                continue;
            }
            items.push(s);
        }
        let _ = FreeEnvironmentStringsW(raw);
    }
    // 环境块要求按名字排序，Windows 才认（不排序多数情况也能跑，但没必要赌）
    items.sort_by(|a, b| {
        let key = |s: &str| s.split('=').next().unwrap_or("").to_ascii_uppercase();
        key(a).cmp(&key(b))
    });
    let mut block: Vec<u16> = Vec::new();
    for it in items {
        block.extend(it.encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// 给命令行里的路径加引号（如果还没被引号包住）。
///
/// Windows 的命令行切分规则：先按空格切，再看引号。所以路径里有空格就必须加引号，
/// 否则 `C:\Program Files\a\b.exe` 会被切成三段。
fn quote_arg(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        return t.to_string();
    }
    format!("\"{}\"", t)
}

/// 用 CreateProcess 直接拉起目标可执行文件（可执行文件才适用）。
unsafe fn create_process(
    exe: &str,
    args: &str,
    workdir: Option<&str>,
) -> Result<(), windows::core::Error> {
    // 命令行必须自己加引号：CreateProcess 在 lpApplicationName 为 None 时
    // 会拿 lpCommandLine 自己切分，切分点就是空格 —— 于是
    // 「C:\Program Files\xxx\app.exe」会被切成「C:\Program」而失败。
    let mut cmd = wide(&quote_arg(exe));
    if !args.is_empty() {
        cmd.pop();
        cmd.extend(wide(args));
    }
    let env = clean_env_block();
    // 工作目录：空指针表示「继承调用方当前目录」。
    // 注意只能传 Option<&PCWSTR>，传 Option<PCWSTR> 过不了 Param 的类型约束。
    let cwd_buf = workdir.map(wide);
    let cwd = cwd_buf.as_ref().map(|v| windows::core::PCWSTR(v.as_ptr()));
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: SW_SHOWNORMAL.0 as u16,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    let r = CreateProcessW(
        None,
        Some(windows::core::PWSTR(cmd.as_mut_ptr())),
        None,
        None,
        false,
        CREATE_UNICODE_ENVIRONMENT,
        Some(env.as_ptr() as *const std::ffi::c_void),
        cwd.as_ref(),
        &si,
        &mut pi,
    );
    if let Err(e) = r {
        return Err(e);
    }
    // 句柄要马上关掉，否则目标程序退出前我们一直占着它
    let _ = CloseHandle(pi.hThread);
    let _ = CloseHandle(pi.hProcess);
    Ok(())
}

/// 用 shell 打开（带参数和工作目录）
unsafe fn shell_exec(file: &str, args: &str, workdir: Option<&str>) -> bool {
    let op = windows::core::HSTRING::from("open");
    let f = windows::core::HSTRING::from(file);
    let a = if args.is_empty() {
        None
    } else {
        Some(windows::core::HSTRING::from(args))
    };
    let d = workdir.map(windows::core::HSTRING::from);
    // 空参数用 Option<&PCWSTR> 传：Option<PCWSTR> / Option<&HSTRING> 都过不了 Param 约束
    let a_ptr = a.as_ref().map(|h| windows::core::PCWSTR(h.as_ptr()));
    let d_ptr = d.as_ref().map(|h| windows::core::PCWSTR(h.as_ptr()));
    let r = ShellExecuteW(
        None,
        &op,
        &f,
        a_ptr.as_ref(),
        d_ptr.as_ref(),
        SW_SHOWNORMAL,
    );
    r.0 as isize > 32
}

/// 这个错误是不是「需要管理员权限」（ERROR_ELEVATION_REQUIRED = 740）。
///
/// CreateProcess 拉不起带 `requireAdministrator` 清单的程序时就报这个，
/// 得让 shell 去弹 UAC，不能当成普通失败。
fn is_elevation_required(e: &windows::core::Error) -> bool {
    e.code().0 == 0x8007_02E4u32 as i32
}

/// 启动一个快捷方式（.lnk）或任意可执行 / 文档。
///
/// 返回 Err 时带具体原因，调用方直接给用户看。
pub fn open(path: &str) -> Result<(), String> {    let p = Path::new(path);
    if !p.exists() {
        return Err("文件已不存在".into());
    }

    if let Some(sc) = resolve_lnk(p) {
        let target = PathBuf::from(&sc.target);
        if target.exists() {
            // 工作目录：优先用 .lnk 自己记的；没记就用目标程序所在目录。
            // 这一条就是「某些软件从收纳盒里打不开」的关键之一。
            let workdir = sc
                .workdir
                .clone()
                .filter(|d| Path::new(d).is_dir())
                .or_else(|| {
                    target
                        .parent()
                        .map(|d| d.to_string_lossy().to_string())
                        .filter(|d| Path::new(d).is_dir())
                });

            let is_exe = target
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    let e = e.to_ascii_lowercase();
                    e == "exe" || e == "com"
                })
                .unwrap_or(false);

            if is_exe {
                match unsafe { create_process(&sc.target, &sc.args, workdir.as_deref()) } {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        // 需要管理员权限：CreateProcess 拉不动，交给 shell 去弹 UAC。
                        // 交的是 .lnk 本身 —— 和资源管理器双击完全一致，
                        // 快捷方式里自带的「以管理员身份运行」「起始位置」都算数。
                        if is_elevation_required(&e)
                            && unsafe { shell_exec(path, "", None) }
                        {
                            return Ok(());
                        }
                        // 其它拉不起来的情况（缺 DLL 之类）：先试 shell 按文件关联兜底。
                        if unsafe { shell_exec(&sc.target, &sc.args, workdir.as_deref()) } {
                            return Ok(());
                        }
                        // 再退一步：把 .lnk 整体交给 shell，和资源管理器双击完全一致。
                        if unsafe { shell_exec(path, "", None) } {
                            return Ok(());
                        }
                        return Err(format!("启动失败：{e}"));
                    }
                }
            } else if unsafe { shell_exec(&sc.target, &sc.args, workdir.as_deref()) } {
                return Ok(());
            }
        } else {
            // 目标程序已经不在了（卸载 / 换盘 / UWP 应用的占位目标）。
            // 但 .lnk 本身还在，交给 shell 往往照样能启动（UWP 就是这种情况：
            // 目标字段是空的或指向一个虚拟路径，shell 走 AppUserModelID 启动）。
            if unsafe { shell_exec(path, "", None) } {
                return Ok(());
            }
            return Err(format!("快捷方式指向的程序已不存在：{}", sc.target));
        }
        // 解析出来但直接拉起失败（例如目标需要 shell 处理），
        // 退回把 .lnk 整体交给 shell，和资源管理器双击完全一致。
    }

    unsafe {
        let r = ShellExecuteW(
            None,
            &windows::core::HSTRING::from("open"),
            &windows::core::HSTRING::from(path),
            None,
            None,
            SW_SHOWNORMAL,
        );
        if r.0 as isize > 32 {
            Ok(())
        } else {
            Err(match r.0 as isize {
                2 => "找不到文件或关联程序".to_string(),
                3 => "找不到路径".to_string(),
                5 => "拒绝访问".to_string(),
                31 => "没有关联的打开方式".to_string(),
                _ => format!("启动失败（ShellExecute 返回 {}）", r.0 as isize),
            })
        }
    }
}

/// 「打开文件所在位置」：在资源管理器里定位这个文件并选中它。
///
/// `.lnk` 要跳到它**指向的程序**所在目录（和系统右键那个菜单项的语义一致）。
/// 解析不出来、或者目标已经被卸载，就退回打开快捷方式自己所在的文件夹，
/// 至少让用户能看到文件本体。
pub fn reveal(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err("文件已不存在".into());
    }

    let mut target = path.to_string();
    if let Some(sc) = resolve_lnk(p) {
        if !sc.target.is_empty() && Path::new(&sc.target).exists() {
            target = sc.target;
        }
    }

    unsafe {
        let params = windows::core::HSTRING::from(format!("/select,\"{target}\""));
        let params_ptr = windows::core::PCWSTR(params.as_ptr());
        let r = ShellExecuteW(
            None,
            &windows::core::HSTRING::from("open"),
            &windows::core::HSTRING::from("explorer.exe"),
            Some(&params_ptr),
            None,
            SW_SHOWNORMAL,
        );
        if r.0 as isize > 32 {
            Ok(())
        } else {
            Err("打开资源管理器失败".into())
        }
    }
}

/// 文件的属性窗口（系统自带的那个），默认停在「快捷方式」那一页。
///
/// 系统的属性对话框永远从第一页（常规）开始，没有官方参数能指定页
/// （`SHObjectProperties` 的 page 参数早就不起作用，`SEE_MASK_INVOKEIDLIST`
/// 走 IContextMenu 的 properties 动词也一样）。而用户点「属性」要看的基本
/// 都是快捷方式本身（目标程序、参数、起始位置），所以弹出来之后替它点一下
/// 那个标签：见 `focus_shortcut_tab`。
pub fn properties(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err("文件已不存在".into());
    }
    // 先挂上监视线程再弹：ShellExecuteW 是同步的，对话框在自己的模态循环里
    // 跑，这一行不返回，只能另开线程去点标签。
    focus_shortcut_tab();
    unsafe {
        let params = windows::core::HSTRING::from(format!("\"{}\"", path));
        let params_ptr = windows::core::PCWSTR(params.as_ptr());
        let r = ShellExecuteW(
            None,
            &windows::core::HSTRING::from("properties"),
            &windows::core::HSTRING::from(path),
            Some(&params_ptr),
            None,
            SW_SHOWNORMAL,
        );
        if r.0 as isize > 32 {
            Ok(())
        } else {
            Err("打开属性失败".into())
        }
    }
}

/// 等属性对话框出现，然后把标签切到「快捷方式」。
///
/// 只在**自己进程**的 `#32770` 窗口里找，而且要求它有标签控件、标签里有
/// 「快捷方式 / Shortcut」—— 别的弹窗（比如我们自己的警告框）不会被误点；
/// 普通文件（exe、文档）没有这一页，等两秒就静默退出。
fn focus_shortcut_tab() {
    std::thread::spawn(|| unsafe {
        // 最多等 3 秒：慢机器上属性对话框可能要一秒多才把标签画出来
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let Some(sheet) = property_sheet() else { continue };
            let Some(tab) = tab_control(sheet) else { continue };
            let Some(idx) = shortcut_tab_index(tab) else { return };
            // 已经是这一页就别动（用户手快自己点过去的）
            let cur = SendMessageW(tab, 0x130B /* TCM_GETCURSEL */, None, None);
            if cur.0 == idx as isize {
                return;
            }
            if click_tab_item(tab, idx) {
                return;
            }
        }
    });
}

/// 本进程里第一个可见的对话框（`#32770`）
unsafe fn property_sheet() -> Option<HWND> {
    let mut found: Vec<HWND> = Vec::new();
    let _ = EnumWindows(
        Some(collect_sheets),
        LPARAM(&mut found as *mut Vec<HWND> as isize),
    );
    found.into_iter().next()
}

unsafe extern "system" fn collect_sheets(hwnd: HWND, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<HWND>);
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != GetCurrentProcessId() {
        return TRUE;
    }
    if class_name(hwnd) == "#32770" {
        out.push(hwnd);
    }
    TRUE
}

/// 对话框里的标签控件（`SysTabControl32`），递归找一层就够用
unsafe fn tab_control(parent: HWND) -> Option<HWND> {
    let mut found: Vec<HWND> = Vec::new();
    let _ = EnumChildWindows(
        Some(parent),
        Some(collect_tabs),
        LPARAM(&mut found as *mut Vec<HWND> as isize),
    );
    found.into_iter().next()
}

unsafe extern "system" fn collect_tabs(hwnd: HWND, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<HWND>);
    if class_name(hwnd) == "SysTabControl32" {
        out.push(hwnd);
    }
    TRUE
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// 「快捷方式」标签的位置；没有这一页返回 None
unsafe fn shortcut_tab_index(tab: HWND) -> Option<usize> {
    let n = SendMessageW(tab, 0x1304 /* TCM_GETITEMCOUNT */, None, None).0;
    for i in 0..n.max(0) as usize {
        let mut buf = [0u16; 64];
        let mut item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: windows::core::PWSTR(buf.as_mut_ptr()),
            cchTextMax: buf.len() as i32,
            ..Default::default()
        };
        SendMessageW(
            tab,
            TCM_GETITEMW,
            Some(WPARAM(i)),
            Some(LPARAM(&mut item as *mut TCITEMW as isize)),
        );
        let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        let text = String::from_utf16_lossy(&buf[..len]);
        if text.contains("快捷方式") || text.eq_ignore_ascii_case("shortcut") {
            return Some(i);
        }
    }
    None
}

/// 用一对鼠标消息点一下第 `idx` 个标签。
///
/// 不用 `TCM_SETCURSEL`：那个只挪高亮，属性页不会跟着换（页面靠
/// `TCN_SELCHANGE` 通知切换）。发真鼠标消息最省事，也不用动用户的光标。
unsafe fn click_tab_item(tab: HWND, idx: usize) -> bool {
    let mut rc = RECT::default();
    let ok = SendMessageW(
        tab,
        TCM_GETITEMRECT,
        Some(WPARAM(idx)),
        Some(LPARAM(&mut rc as *mut RECT as isize)),
    )
    .0;
    if ok == 0 {
        return false;
    }
    let x = (rc.left + rc.right) / 2;
    let y = (rc.top + rc.bottom) / 2;
    let lp = LPARAM(((y as isize) << 16) | (x as isize & 0xFFFF));
    SendMessageW(tab, WM_LBUTTONDOWN, Some(WPARAM(1)), Some(lp));
    SendMessageW(tab, WM_LBUTTONUP, Some(WPARAM(0)), Some(lp));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个 .lnk 出来（用 shell 自己那套 COM，所以造出来的和用户双击看到的完全一致）
    fn make_lnk(target: &str, args: &str, workdir: &str, out: &Path) -> bool {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let uninit = hr.is_ok();

            let ok = (|| {
                let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
                let persist: IPersistFile = link.cast().ok()?;
                let t = wide(target);
                let a = wide(args);
                let w = wide(workdir);
                link.SetPath(windows::core::PCWSTR(t.as_ptr())).ok()?;
                link.SetArguments(windows::core::PCWSTR(a.as_ptr())).ok()?;
                link.SetWorkingDirectory(windows::core::PCWSTR(w.as_ptr())).ok()?;
                let o = wide(&out.to_string_lossy());
                persist.Save(windows::core::PCWSTR(o.as_ptr()), true).ok()?;
                Some(())
            })();

            if uninit {
                CoUninitialize();
            }
            ok.is_some()
        }
    }

    #[test]
    fn 解析自己造的lnk() {
        let dir = std::env::temp_dir().join("lightbox_lnk_test");
        let _ = std::fs::create_dir_all(&dir);
        let lnk = dir.join("t.lnk");
        let _ = std::fs::remove_file(&lnk);

        let exe = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let wd = dir.to_string_lossy().to_string();
        assert!(make_lnk(&exe, "/c echo hi", &wd, &lnk), "造 .lnk 失败");

        let sc = resolve_lnk(&lnk).expect("解析 .lnk 失败");
        assert!(
            sc.target.eq_ignore_ascii_case(&exe),
            "目标不一致：{} vs {}",
            sc.target,
            exe
        );
        assert_eq!(sc.args, "/c echo hi", "参数不一致");
        assert!(
            sc.workdir.as_deref().unwrap_or("").eq_ignore_ascii_case(&wd),
            "工作目录不一致：{:?}",
            sc.workdir
        );

        let _ = std::fs::remove_file(&lnk);
    }

    #[test]
    fn 非lnk文件不解析() {
        assert!(resolve_lnk(Path::new("C:/Windows/System32/cmd.exe")).is_none());
        assert!(resolve_lnk(Path::new("C:/nope.txt")).is_none());
    }

    #[test]
    fn 带空格的路径会加引号() {
        assert_eq!(quote_arg("C:\\Program Files\\a\\b.exe"), "\"C:\\Program Files\\a\\b.exe\"");
        assert_eq!(quote_arg("\"C:\\a b.exe\""), "\"C:\\a b.exe\"");
        assert_eq!(quote_arg("C:\\a.exe"), "\"C:\\a.exe\"");
        assert_eq!(quote_arg("  C:\\a.exe  "), "\"C:\\a.exe\"");
    }

    #[test]
    fn 环境块剔掉了electron变量() {
        unsafe {
            std::env::set_var("ELECTRON_RUN_AS_NODE", "1");
            std::env::set_var("NODE_OPTIONS", "--inspect");
            let block = clean_env_block();
            let mut text = String::new();
            for part in block.split(|c| *c == 0) {
                if part.is_empty() {
                    continue;
                }
                text.push_str(&String::from_utf16_lossy(part));
                text.push('\n');
            }
            std::env::remove_var("ELECTRON_RUN_AS_NODE");
            std::env::remove_var("NODE_OPTIONS");
            assert!(!text.contains("ELECTRON_RUN_AS_NODE"), "环境块里还留着 ELECTRON_*");
            assert!(!text.contains("NODE_OPTIONS"), "环境块里还留着 NODE_OPTIONS");
            // 环境块必须双 NUL 结尾
            assert_eq!(*block.last().unwrap(), 0);
            assert_eq!(*block.get(block.len() - 2).unwrap(), 0);
        }
    }

    /// 环境块必须**完整**：GetEnvironmentStringsW 返回的是一整块双 NUL 结尾的数据，
    /// 扫到第一个 NUL 就停的话，子进程只拿到一个变量，启动后立刻退出。
    #[test]
    fn 环境块是完整的() {
        unsafe {
            let block = clean_env_block();
            let text: Vec<String> = block
                .split(|c| *c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf16_lossy(s))
                .collect();
            assert!(
                text.len() > 10,
                "环境块里只有 {} 个变量，环境没抄全：{text:?}",
                text.len()
            );
            assert!(
                text.iter().any(|s| s.to_ascii_uppercase().starts_with("PATH=")),
                "环境块里没有 PATH：{text:?}"
            );
            assert!(
                text.iter().any(|s| s.to_ascii_uppercase().starts_with("USERPROFILE=")),
                "环境块里没有 USERPROFILE：{text:?}"
            );
        }
    }

    /// 手动跑：`cargo test --release 属性对话框默认停在快捷方式页 -- --ignored --nocapture`
    ///
    /// 会真的弹一个属性对话框（约 3 秒后自己关掉），验证监视线程把标签点到了
    /// 「快捷方式」。平时不跑 —— 它需要桌面会话，跑测试的机器上会有窗口闪烁。
    #[test]
    #[ignore]
    fn 属性对话框默认停在快捷方式页() {
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

        let dir = std::env::temp_dir().join("lightbox_props_test");
        let _ = std::fs::create_dir_all(&dir);
        let lnk = dir.join("prop.lnk");
        let _ = std::fs::remove_file(&lnk);
        let exe = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let wd = dir.to_string_lossy().to_string();
        assert!(make_lnk(&exe, "", &wd, &lnk), "造 .lnk 失败");

        let path = lnk.to_string_lossy().to_string();
        let worker = std::thread::spawn(move || {
            let _ = properties(&path);
        });

        let mut sheet = None;
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            unsafe {
                if let Some(h) = property_sheet() {
                    if tab_control(h).is_some() {
                        sheet = Some(h);
                        break;
                    }
                }
            }
        }
        let sheet = sheet.expect("等不到属性对话框");
        // 留时间给监视线程点标签
        std::thread::sleep(std::time::Duration::from_millis(900));

        let (cur, want) = unsafe {
            let tab = tab_control(sheet).expect("找不到标签控件");
            (
                SendMessageW(tab, 0x130B /* TCM_GETCURSEL */, None, None).0,
                shortcut_tab_index(tab).expect("标签里没有「快捷方式」") as isize,
            )
        };
        unsafe {
            let _ = PostMessageW(
                Some(sheet),
                0x0010, /* WM_CLOSE */
                WPARAM(0),
                LPARAM(0),
            );
        }
        let _ = worker.join();
        let _ = std::fs::remove_file(&lnk);
        assert_eq!(cur, want, "属性页没停在「快捷方式」");
    }
}
