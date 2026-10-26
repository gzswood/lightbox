//! 打开快捷方式：先把 .lnk 解析成「目标程序 + 参数 + 工作目录」再启动，
//! 而不是把 .lnk 直接丢给 ShellExecute。
//!
//! 为什么要多这一步：
//! 1. **工作目录**。.lnk 里没有记录工作目录时，ShellExecute 会把「调用方」的当前目录
//!    传给新进程。LightBox 被从别处启动时，cwd 可能是任意目录，于是那些依赖 cwd
//!    的软件（Electron 一类最常见）会在启动阶段直接退出，表现出来就是「点了没反应」。
//!    这里显式给目标程序自己的目录，和资源管理器双击的行为对齐。
//! 2. **失效能被发现**。目标程序已经不在了（卸载/换盘）时，可以明确告诉用户，
//!    而不是弹一个系统错误框或者干脆什么都不发生。
//! 3. **参数**。个别快捷方式靠参数区分模式（--silent、--profile 之类），
//!    解析出来原样传下去。
//!
//! 解析失败（.url、自定义协议、指向 UWP 应用的 .lnk）就退回原来的做法：
//! 把文件本身交给 ShellExecute。

use crate::win32::*;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Shortcut {
    pub target: String,
    pub args: String,
    pub workdir: Option<String>,
}

// ---------------- 手写 COM：IShellLinkW / IPersistFile ----------------
//
// 只用四个方法（Release / GetPath / GetWorkingDirectory / GetArguments），
// 所以按 vtable 顺序摆好指针就够了，不需要完整的接口定义。
// 顺序必须和 shellapi.h 一致：IUnknown(3) → GetPath(3) → GetIDList → SetIDList
// → GetDescription → SetDescription → GetWorkingDirectory → SetWorkingDirectory
// → GetArguments ...

type QueryInterfaceFn =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32;
type AddRefFn = unsafe extern "system" fn(*mut c_void) -> u32;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type GetPathFn =
    unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> i32;
type GetStrFn = unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> i32;
type PersistLoadFn = unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32;

#[repr(C)]
struct ShellLinkVtbl {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
    get_path: GetPathFn,
    get_id_list: *const c_void,
    set_id_list: *const c_void,
    get_description: *const c_void,
    set_description: *const c_void,
    get_working_directory: GetStrFn,
    set_working_directory: *const c_void,
    get_arguments: GetStrFn,
}

#[repr(C)]
struct PersistFileVtbl {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
    get_class_id: *const c_void,
    is_dirty: *const c_void,
    load: PersistLoadFn,
}

unsafe fn take_str(buf: &[u16]) -> Option<String> {
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
        let hr = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        // S_FALSE / S_OK 都是本次调用初始化成功的，要配对 CoUninitialize；
        // RPC_E_CHANGED_MODE 表示线程已经在别的套间里，只能用，不能反初始化。
        let uninit = hr >= 0;

        let mut out: Option<Shortcut> = None;
        let mut link: *mut c_void = std::ptr::null_mut();
        if CoCreateInstance(
            &CLSID_SHELLLINK,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_ISHELLLINKW,
            &mut link,
        ) == S_OK
            && !link.is_null()
        {
            let lv = *(link as *mut *const ShellLinkVtbl);
            let mut persist: *mut c_void = std::ptr::null_mut();
            if ((*lv).query_interface)(link, &IID_IPERSISTFILE, &mut persist) == S_OK
                && !persist.is_null()
            {
                let pv = *(persist as *mut *const PersistFileVtbl);
                let wpath = wide(&path.to_string_lossy());
                if ((*pv).load)(persist, wpath.as_ptr(), STGM_READ) == S_OK {
                    let mut buf = vec![0u16; 2048];
                    let mut sc = Shortcut::default();
                    if ((*lv).get_path)(link, buf.as_mut_ptr(), buf.len() as i32, std::ptr::null_mut(), 0)
                        == S_OK
                    {
                        sc.target = take_str(&buf).unwrap_or_default();
                    }
                    if !sc.target.is_empty() {
                        let mut abuf = vec![0u16; 2048];
                        if ((*lv).get_arguments)(link, abuf.as_mut_ptr(), abuf.len() as i32) == S_OK {
                            sc.args = take_str(&abuf).unwrap_or_default();
                        }
                        let mut wbuf = vec![0u16; 2048];
                        if ((*lv).get_working_directory)(link, wbuf.as_mut_ptr(), wbuf.len() as i32)
                            == S_OK
                        {
                            sc.workdir = take_str(&wbuf);
                        }
                        out = Some(sc);
                    }
                }
                ((*pv).release)(persist);
            }
            ((*lv).release)(link);
        }

        if uninit {
            CoUninitialize();
        }
        out
    }
}

/// 构造一份「干净」的环境块：去掉 ELECTRON_* / NODE_OPTIONS。
///
/// 为什么必须做这件事：Electron 应用会读 `ELECTRON_RUN_AS_NODE`，一旦这个变量存在，
/// 它们就不再启动界面，而是退化成纯 Node 进程然后立刻退出 —— 用户看到的就是
/// 「点了一下，什么都没发生」。LightBox 的开机自启走 HKCU\Run（环境是干净的），
/// 但如果是被某个带这套变量的终端/开发工具拉起来的，子进程会把它们全继承下去。
/// 这类变量只对「我们想启动的那个程序」有害，所以启动前统一滤掉。
unsafe fn clean_env_block() -> Vec<u16> {
    let raw = GetEnvironmentStringsW();
    let mut items: Vec<String> = Vec::new();
    if !raw.is_null() {
        let mut i = 0isize;
        loop {
            let start = raw.offset(i);
            if *start == 0 {
                break;
            }
            let mut len = 0isize;
            while *raw.offset(i + len) != 0 {
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
        FreeEnvironmentStringsW(raw);
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

/// 用 CreateProcess 直接拉起目标可执行文件（可执行文件才适用）。
unsafe fn create_process(
    exe: &str,
    args: &str,
    workdir: Option<&str>,
) -> Result<(), (u32, String)> {
    let mut cmdline: Vec<u16> = Vec::new();
    cmdline.push(b'"' as u16);
    cmdline.extend(exe.encode_utf16());
    cmdline.push(b'"' as u16);
    if !args.is_empty() {
        cmdline.push(' ' as u16);
        cmdline.extend(args.encode_utf16());
    }
    cmdline.push(0);

    let app = wide(exe);
    let dir = workdir.map(wide);
    let mut si = STARTUPINFOW::default();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESHOWWINDOW;
    si.wShowWindow = SW_SHOWNORMAL;
    let mut pi = PROCESS_INFORMATION::default();
    let mut env = clean_env_block();

    let ok = CreateProcessW(
        app.as_ptr(),
        cmdline.as_mut_ptr(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        0,
        CREATE_UNICODE_ENVIRONMENT,
        env.as_mut_ptr() as *mut c_void,
        dir.as_ref().map(|d| d.as_ptr()).unwrap_or(std::ptr::null()),
        &mut si,
        &mut pi,
    );
    if ok == 0 {
        let code = GetLastError();
        return Err((code, format!("CreateProcess 失败（错误码 {code}）")));
    }
    // 起来之后给 500ms 看一眼：进程如果这么快就退出，多半是配置/权限问题。
    // 注意退出码 0 不算失败 —— 单实例程序（Motrix 这类）检测到已有实例时
    // 就会干净退出，这是正常的。
    //
    // 这个观察**必须在后台线程做**：命令调用方（前端）是「点图标 → 启动 → 收起
    // 面板」，在这里同步等 500ms 会让面板的收起粘滞半秒。移到线程后命令立即
    // 返回，观察结果只写日志（面板已收起，toast 无处可显）。
    let h_process = pi.hProcess;
    unsafe {
        CloseHandle(pi.hThread);
    }
    std::thread::spawn(move || {
        let waited = unsafe { WaitForSingleObject(h_process, 500) };
        if waited == 0 {
            let mut code: u32 = 0;
            if unsafe { GetExitCodeProcess(h_process, &mut code) } != 0
                && code != STILL_ACTIVE
                && code != 0
            {
                eprintln!("程序启动后立即退出（退出码 {code}）");
            }
        }
        unsafe {
            CloseHandle(h_process);
        }
    });
    Ok(())
}

/// 启动一个快捷方式（.lnk）或任意可执行/文档。
///
/// 返回 Err 时带具体原因，前端会直接 toast 给用户。
pub fn open(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err("文件已不存在".into());
    }

    if let Some(sc) = resolve_lnk(p) {
        let target = PathBuf::from(&sc.target);
        if !target.exists() {
            return Err(format!("快捷方式指向的程序已不存在：{}", sc.target));
        }
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
            let r = unsafe { create_process(&sc.target, &sc.args, workdir.as_deref()) };
            match r {
                Ok(()) => return Ok(()),
                // 需要管理员权限 / 被策略拦：交给 shell，它会弹 UAC 或给出系统提示
                Err((ERROR_ELEVATION_REQUIRED, _)) => {}
                Err((_, msg)) => {
                    // 目标存在但拉不起来（缺 DLL 之类）。先试 shell，再决定要不要报错。
                    if shell_exec(&sc.target, &sc.args, workdir.as_deref()) {
                        return Ok(());
                    }
                    return Err(msg);
                }
            }
        } else if shell_exec(&sc.target, &sc.args, workdir.as_deref()) {
            return Ok(());
        }
        // 解析出来但直接拉起失败（例如目标需要 shell 处理），
        // 退回把 .lnk 整体交给 shell，和资源管理器双击完全一致。
    }

    let r = unsafe {
        let op = wide("open");
        let file = wide(path);
        ShellExecuteW(0, op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1)
    };
    if r as isize > 32 {
        Ok(())
    } else {
        Err(match r as isize {
            2 => "找不到文件或关联程序".to_string(),
            3 => "找不到路径".to_string(),
            5 => "拒绝访问".to_string(),
            31 => "没有关联的打开方式".to_string(),
            _ => format!("启动失败（ShellExecute 返回 {r}）"),
        })
    }
}

fn shell_exec(file: &str, args: &str, workdir: Option<&str>) -> bool {
    unsafe {
        let op = wide("open");
        let f = wide(file);
        let a = if args.is_empty() { None } else { Some(wide(args)) };
        let d = workdir.map(wide);
        let r = ShellExecuteW(
            0,
            op.as_ptr(),
            f.as_ptr(),
            a.as_ref().map(|w| w.as_ptr()).unwrap_or(std::ptr::null()),
            d.as_ref().map(|w| w.as_ptr()).unwrap_or(std::ptr::null()),
            1,
        );
        r as isize > 32
    }
}
