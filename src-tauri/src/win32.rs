//! 最小 Win32 FFI 声明：只声明 LightBox 真正用到的函数，避免引入 windows crate 的版本耦合。
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

pub type HWND = isize;
pub type HICON = isize;
pub type HBITMAP = isize;
pub type HDC = isize;
pub type HGDIOBJ = isize;

pub const GWL_STYLE: i32 = -16;
pub const GWL_EXSTYLE: i32 = -20;
/// 对**顶层**窗口来说这个索引设的是「owner」（被拥有的窗口永远压在 owner 之上），
/// 对子窗口才是 parent。Win11 的贴条靠它常驻任务栏之上（见 desktop::pin_to_taskbar）。
pub const GWLP_HWNDPARENT: i32 = -8;
/// 子类化时替换窗口过程用
pub const GWLP_WNDPROC: i32 = -4;
/// 计算非客户区大小。wParam != 0 时直接返回 0 = 「客户区就是整个窗口矩形」
pub const WM_NCCALCSIZE: u32 = 0x0083;
/// GetWindow 的 uCmd：取 owner 窗口
pub const GW_OWNER: u32 = 4;

pub const WS_CHILD: isize = 0x4000_0000;
pub const WS_POPUP: isize = 0x8000_0000;
pub const WS_VISIBLE: isize = 0x1000_0000;
pub const WS_CLIPCHILDREN: isize = 0x0200_0000;
pub const WS_CLIPSIBLINGS: isize = 0x0400_0000;
pub const WS_CAPTION: isize = 0x00C0_0000;
pub const WS_THICKFRAME: isize = 0x0004_0000;
pub const WS_SYSMENU: isize = 0x0008_0000;
pub const WS_MINIMIZEBOX: isize = 0x0002_0000;
pub const WS_MAXIMIZEBOX: isize = 0x0001_0000;
pub const WS_EX_LAYERED: isize = 0x0008_0000;
pub const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
pub const WS_EX_TOPMOST: isize = 0x0000_0008;
pub const WS_EX_NOACTIVATE: isize = 0x0800_0000;

/// SetLayeredWindowAttributes 的 dwFlags：按 alpha 值整体混合（整窗半透明）
pub const LWA_ALPHA: u32 = 0x0000_0002;

pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_FRAMECHANGED: u32 = 0x0020;
pub const SWP_SHOWWINDOW: u32 = 0x0040;
/// 「跨线程摆位不要等」：目标窗口属于别的线程时，SetWindowPos 默认会
/// SendMessage 过去等对方处理完才返回 —— 对方要是正卡在模态循环里，调用方
/// 就永远等下去（胶囊团拖动卡死的根源）。带上这一位，请求投递完立刻返回。
pub const SWP_ASYNCWINDOWPOS: u32 = 0x4000;
pub const HWND_TOP: isize = 0;
pub const HWND_BOTTOM: isize = 1;
pub const HWND_TOPMOST: isize = -1;

pub const SHGFI_ICON: u32 = 0x0000_0100;
pub const SHGFI_LARGEICON: u32 = 0x0000_0000;
pub const SHGFI_USEFILEATTRIBUTES: u32 = 0x0000_0010;

pub const DI_NORMAL: u32 = 0x0003;
pub const DIB_RGB_COLORS: u32 = 0;
pub const BI_RGB: u32 = 0;

/// GetPixel 取不到颜色时的返回值
pub const CLR_INVALID: u32 = 0xFFFF_FFFF;
/// 虚拟屏幕（所有显示器拼起来的那块）的左上角与尺寸
pub const SM_XVIRTUALSCREEN: i32 = 76;
pub const SM_YVIRTUALSCREEN: i32 = 77;
pub const SM_CXVIRTUALSCREEN: i32 = 78;
pub const SM_CYVIRTUALSCREEN: i32 = 79;
/// 显示器的块数（改显示模式时它会跟着变，用来做「显示配置变了」的指纹）
pub const SM_CMONITORS: i32 = 80;

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

#[repr(C)]
pub struct BITMAPINFOHEADER {
    pub biSize: u32,
    pub biWidth: i32,
    pub biHeight: i32,
    pub biPlanes: u16,
    pub biBitCount: u16,
    pub biCompression: u32,
    pub biSizeImage: u32,
    pub biXPelsPerMeter: i32,
    pub biYPelsPerMeter: i32,
    pub biClrUsed: u32,
    pub biClrImportant: u32,
}

#[repr(C)]
pub struct ICONINFO {
    pub fIcon: i32,
    pub xHotspot: u32,
    pub yHotspot: u32,
    pub hbmMask: HBITMAP,
    pub hbmColor: HBITMAP,
}

#[repr(C)]
pub struct SHFILEINFOW {
    pub hIcon: HICON,
    pub iIcon: i32,
    pub dwAttributes: u32,
    pub szDisplayName: [u16; 260],
    pub szTypeName: [u16; 80],
}

impl Default for SHFILEINFOW {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// `ShellExecuteExW` 的参数结构（64 位下 112 字节）。
///
/// 存在的理由只有一条：**「属性」对话框必须走它**。
/// `ShellExecuteW` 没有 `lpVerb="properties"` 这条路径，拿不到
/// 「快捷方式 / 兼容性 / 详细信息」那几页，只能看到普通文件属性。
///
/// `hIcon` / `hProcess` 在头文件里是 union，这里按句柄宽度摆 flat 字段即可
/// —— 我们只用 `lpVerb` / `lpFile` / `fMask` / `hwnd` 四个，其余保持 0。
#[repr(C)]
pub struct SHELLEXECUTEINFOW {
    pub cbSize: u32,
    pub fMask: u32,
    pub hwnd: HWND,
    pub lpVerb: *const u16,
    pub lpFile: *const u16,
    pub lpParameters: *const u16,
    pub lpDirectory: *const u16,
    pub nShow: i32,
    pub hInstApp: isize,
    pub lpIDList: *mut c_void,
    pub lpClass: *const u16,
    pub hkeyClass: HKEY,
    pub dwHotKey: u32,
    pub hIcon: isize,
    pub hProcess: isize,
}

impl Default for SHELLEXECUTEINFOW {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// FOLDERID_Desktop = {B4BFCC3A-DB2C-424C-B029-7FE99A87C641}
pub const FOLDERID_DESKTOP: GUID = GUID {
    data1: 0xB4BF_CC3A,
    data2: 0xDB2C,
    data3: 0x424C,
    data4: [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41],
};

#[link(name = "user32")]
extern "system" {
    pub fn FindWindowW(lpClassName: *const u16, lpWindowName: *const u16) -> HWND;
    pub fn FindWindowExW(
        hWndParent: HWND,
        hWndChildAfter: HWND,
        lpszClass: *const u16,
        lpszWindow: *const u16,
    ) -> HWND;
    pub fn SetParent(hWndChild: HWND, hWndNewParent: HWND) -> HWND;
    /// 注意：`SetParent` **不会**补 `WS_CHILD` 样式，所以对挂到 Progman 的窗口
    /// `GetParent()` 返回 0。判断父子链一律用 `GetAncestor(GA_PARENT)`。
    pub fn GetParent(hWnd: HWND) -> HWND;
    /// 桌面窗口：顶层窗口的 `GA_PARENT` 就是它
    pub fn GetDesktopWindow() -> HWND;
    /// 客户区坐标 → 屏幕坐标（配合 GetParent 换算子窗口的 SetWindowPos 坐标）
    pub fn ClientToScreen(hWnd: HWND, lpPoint: *mut POINT) -> i32;
    pub fn IsWindow(hWnd: HWND) -> i32;
    pub fn IsWindowVisible(hWnd: HWND) -> i32;
    /// 显示/隐藏窗口。截「盒子背后的桌面」时要先把它藏一下。
    pub fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
    pub fn GetWindowRect(hWnd: HWND, lpRect: *mut RECT) -> i32;
    /// 取 owner / 前一个窗口等关系（GW_OWNER 用来确认贴条还挂在任务栏名下）
    pub fn GetWindow(hWnd: HWND, uCmd: u32) -> HWND;
    pub fn SetWindowPos(
        hWnd: HWND,
        hWndInsertAfter: HWND,
        X: i32,
        Y: i32,
        cx: i32,
        cy: i32,
        uFlags: u32,
    ) -> i32;
    pub fn SetWindowLongPtrW(hWnd: HWND, nIndex: i32, dwNewLong: isize) -> isize;
    pub fn GetWindowLongPtrW(hWnd: HWND, nIndex: i32) -> isize;
    /// 子类化窗口过程之后，把消息转给被换下来的原过程
    pub fn CallWindowProcW(
        lpPrevWndFunc: isize,
        hWnd: HWND,
        Msg: u32,
        wParam: usize,
        lParam: isize,
    ) -> isize;
    pub fn DefWindowProcW(hWnd: HWND, Msg: u32, wParam: usize, lParam: isize) -> isize;
    /// 把窗口裁成任意形状：成功后 rgn 归系统所有，不要自己 DeleteObject
    pub fn SetWindowRgn(hWnd: HWND, hRgn: isize, bRedraw: i32) -> i32;
    /// 读窗口当前的裁剪区域（会**拷贝**进 hRgn，这个副本用完要 DeleteObject）
    pub fn GetWindowRgn(hWnd: HWND, hRgn: isize) -> i32;
    pub fn DestroyIcon(hIcon: HICON) -> i32;
    pub fn DrawIconEx(
        hdc: HDC,
        xLeft: i32,
        yTop: i32,
        hIcon: HICON,
        cxWidth: i32,
        cyWidth: i32,
        istepIfAniCur: u32,
        hbrFlickerFreeDraw: isize,
        diFlags: u32,
    ) -> i32;
    pub fn GetDC(hWnd: HWND) -> HDC;
    pub fn ReleaseDC(hWnd: HWND, hDC: HDC) -> i32;
    /// 取屏幕 DC 上某个物理像素的颜色（COLORREF；取不到返回 CLR_INVALID = -1）
    pub fn GetPixel(hDC: HDC, x: i32, y: i32) -> u32;
    /// 屏幕 DC（"DISPLAY"）：从它 BitBlt 就能截屏
    pub fn CreateDCW(
        pwszDriver: *const u16,
        pwszDevice: *const u16,
        pszPort: *const u16,
        pdm: *mut c_void,
    ) -> HDC;
    pub fn GetForegroundWindow() -> HWND;
    pub fn SetForegroundWindow(hWnd: HWND) -> i32;
    /// 整窗透明度：需要窗口带 WS_EX_LAYERED。alpha 0~255。
    pub fn SetLayeredWindowAttributes(hWnd: HWND, crKey: u32, bAlpha: u8, dwFlags: u32) -> i32;
    pub fn GetWindowThreadProcessId(hWnd: HWND, lpdwProcessId: *mut u32) -> u32;
    pub fn AttachThreadInput(idAttach: u32, idAttachTo: u32, fAttach: i32) -> i32;
    pub fn GetAncestor(hWnd: HWND, gaFlags: u32) -> HWND;
    pub fn GetCursorPos(lpPoint: *mut POINT) -> i32;
    /// 解除/设置光标裁剪。系统拖动**子窗口**时会把光标夹在父窗口客户区里
    /// （任务栏胶囊就属于这种），于是「往上拖出去」在物理上办不到 —— 传 NULL 解除。
    pub fn ClipCursor(lpRect: *const RECT) -> i32;
    /// 鼠标左键是否还按着（0x8000 = 当前按下）。判定「系统拖动有没有结束」用：
    /// 拖动期间收不到 pointerup，只能问系统。
    pub fn GetAsyncKeyState(vKey: i32) -> i16;
    pub fn WindowFromPoint(Point: POINT) -> HWND;
    /// 线程的 GUI 状态。`GetCapture()` 是**按调用线程**返回的（跨线程永远拿不到），
    /// 所以「那个窗口的线程还在系统拖动循环里吗」只能问这里（看 flags 的 GUI_INMOVESIZE）。
    pub fn GetGUIThreadInfo(idThread: u32, pgui: *mut GUITHREADINFO) -> i32;
    pub fn PostMessageW(hWnd: HWND, Msg: u32, wParam: usize, lParam: isize) -> i32;
    pub fn GetClassNameW(hWnd: HWND, lpClassName: *mut u16, nMaxCount: i32) -> i32;
    pub fn RegisterClipboardFormatW(lpszFormat: *const u16) -> u32;
    /// 跨进程发消息要带超时：Explorer 卡住时不能把本进程一起拖死
    pub fn SendMessageTimeoutW(
        hWnd: HWND,
        Msg: u32,
        wParam: usize,
        lParam: isize,
        fuFlags: u32,
        uTimeout: u32,
        lpdwResult: *mut usize,
    ) -> isize;
}

pub const WM_COMMAND: u32 = 0x0111;
/// 取消模态状态（菜单循环、系统拖动/缩放循环都靠它退出）
pub const WM_CANCELMODE: u32 = 0x001F;
/// 伪造一个「左键松开」把系统拖动循环顶出来（个别机器上 WM_CANCELMODE 不管用）
pub const WM_LBUTTONUP: u32 = 0x0202;
/// SHELLDLL_DefView 的「刷新」命令（等价于在桌面上按 F5）
pub const FCIDM_SHVIEW_REFRESH: usize = 0x7103;
/// 目标窗口挂起时立刻返回，不等待
pub const SMTO_ABORTIFHUNG: u32 = 0x0002;

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

pub const GA_PARENT: u32 = 1;

/// 线程 GUI 状态（`GetGUIThreadInfo`）。只用到 `flags` 和 `hwndMoveSize`。
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct GUITHREADINFO {
    pub cbSize: u32,
    pub flags: u32,
    pub hwndActive: HWND,
    pub hwndFocus: HWND,
    pub hwndCapture: HWND,
    pub hwndMenuOwner: HWND,
    pub hwndMoveSize: HWND,
    pub hwndCaret: HWND,
    pub rcCaret: RECT,
}

/// 该线程正在系统「移动 / 缩放」循环里（拖动窗口期间就是这个状态）
pub const GUI_INMOVESIZE: u32 = 0x0000_0002;

/// DWM 非客户区渲染策略：2 = DWMWA_NCRENDERING_POLICY，1 = DWMNCRP_DISABLED
pub const DWMWA_NCRENDERING_POLICY: u32 = 2;
pub const DWMNCRP_DISABLED: u32 = 1;

/// Win11 才有的两个外观属性（更老的系统上调用会返回错误，无害）。
/// 33 = DWMWA_WINDOW_CORNER_PREFERENCE（1 = 不要圆角，圆角我们自己用窗口区域裁）
/// 34 = DWMWA_BORDER_COLOR（0xFFFFFFFE = DWMWA_COLOR_NONE，去掉那圈 1px 描边）
pub const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
pub const DWMWCP_DONOTROUND: u32 = 1;
pub const DWMWA_BORDER_COLOR: u32 = 34;
pub const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

#[repr(C)]
pub struct MARGINS {
    pub cxLeftWidth: i32,
    pub cxRightWidth: i32,
    pub cyTopHeight: i32,
    pub cyBottomHeight: i32,
}

#[link(name = "dwmapi")]
extern "system" {
    pub fn DwmSetWindowAttribute(
        hwnd: HWND,
        dwAttribute: u32,
        pvAttribute: *const c_void,
        cbAttribute: u32,
    ) -> i32;
    pub fn DwmExtendFrameIntoClientArea(hwnd: HWND, pMarInset: *const MARGINS) -> i32;
}

pub const ERROR_ALREADY_EXISTS: u32 = 183;

/// `ShowWindow` 的命令值
pub const SW_HIDE: i32 = 0;
/// 显示但不激活：盒子藏在桌面层，重新显示时不能把前台抢走
pub const SW_SHOWNOACTIVATE: i32 = 4;

// ---------------- 打开文件对话框（comdlg32） ----------------
//
// 只为「选一个 .ico」用，没必要为它拉 tauri-plugin-dialog（多一个依赖 + 前端 IPC 一趟）。
#[repr(C)]
pub struct OPENFILENAMEW {
    pub lStructSize: u32,
    pub hwndOwner: HWND,
    pub hInstance: isize,
    pub lpstrFilter: *const u16,
    pub lpstrCustomFilter: *mut u16,
    pub nMaxCustFilter: u32,
    pub nFilterIndex: u32,
    pub lpstrFile: *mut u16,
    pub nMaxFile: u32,
    pub lpstrFileTitle: *mut u16,
    pub nMaxFileTitle: u32,
    pub lpstrInitialDir: *const u16,
    pub lpstrTitle: *const u16,
    pub Flags: u32,
    pub nFileOffset: u16,
    pub nFileExtension: u16,
    pub lpstrDefExt: *const u16,
    pub lCustData: isize,
    pub lpfnHook: *mut c_void,
    pub lpTemplateName: *const u16,
    pub pvReserved: *mut c_void,
    pub dwReserved: u32,
    pub FlagsEx: u32,
}

impl Default for OPENFILENAMEW {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// 文件必须存在
pub const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
/// 路径必须存在
pub const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
/// 只读勾选框去掉
pub const OFN_HIDEREADONLY: u32 = 0x0000_0004;
/// 不改当前工作目录（很重要：改了会让后续相对路径全乱）
pub const OFN_NOCHANGEDIR: u32 = 0x0000_0008;
pub const OFN_EXPLORER: u32 = 0x0008_0000;

#[link(name = "comdlg32")]
extern "system" {
    pub fn GetOpenFileNameW(ofn: *mut OPENFILENAMEW) -> i32;
}

// ---------------- 从 .ico 文件直接拿图标 ----------------
pub const IMAGE_ICON: u32 = 1;
pub const LR_LOADFROMFILE: u32 = 0x0000_0010;

#[link(name = "user32")]
extern "system" {
    /// 简单的提示框。用于「选错文件」这类只在弹窗当下需要告知、且此时前端菜单已经收起来的情况
    pub fn MessageBoxW(hwnd: HWND, text: *const u16, caption: *const u16, utype: u32) -> i32;
    /// 直接从磁盘上的 .ico 加载图标，并缩放到指定尺寸
    pub fn LoadImageW(
        hinst: isize,
        name: *const u16,
        typ: u32,
        cx: i32,
        cy: i32,
        fuLoad: u32,
    ) -> isize;
}

pub const MB_OK: u32 = 0;
pub const MB_ICONWARNING: u32 = 0x0000_0030;
pub const MB_SETFOREGROUND: u32 = 0x0001_0000;
pub const MB_TOPMOST: u32 = 0x0004_0000;

pub type HKEY = isize;
/// HKEY_CURRENT_USER = (HKEY)0x80000001
pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001u32 as isize;
pub const KEY_QUERY_VALUE: u32 = 0x0001;
pub const KEY_SET_VALUE: u32 = 0x0002;
pub const REG_SZ: u32 = 1;
pub const ERROR_SUCCESS: i32 = 0;
pub const ERROR_FILE_NOT_FOUND: i32 = 2;

#[link(name = "advapi32")]
extern "system" {
    pub fn RegCreateKeyExW(
        hKey: HKEY,
        lpSubKey: *const u16,
        Reserved: u32,
        lpClass: *const u16,
        dwOptions: u32,
        samDesired: u32,
        lpSecurityAttributes: *mut c_void,
        phkResult: *mut HKEY,
        lpdwDisposition: *mut u32,
    ) -> i32;
    pub fn RegOpenKeyExW(
        hKey: HKEY,
        lpSubKey: *const u16,
        ulOptions: u32,
        samDesired: u32,
        phkResult: *mut HKEY,
    ) -> i32;
    pub fn RegSetValueExW(
        hKey: HKEY,
        lpValueName: *const u16,
        Reserved: u32,
        dwType: u32,
        lpData: *const u8,
        cbData: u32,
    ) -> i32;
    pub fn RegDeleteValueW(hKey: HKEY, lpValueName: *const u16) -> i32;
    pub fn RegQueryValueExW(
        hKey: HKEY,
        lpValueName: *const u16,
        lpReserved: *mut u32,
        lpType: *mut u32,
        lpData: *mut u8,
        lpcbData: *mut u32,
    ) -> i32;
    pub fn RegCloseKey(hKey: HKEY) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    /// 命名互斥体：第二个实例启动时 GetLastError 会是 ERROR_ALREADY_EXISTS
    pub fn CreateMutexW(
        lpMutexAttributes: *mut c_void,
        bInitialOwner: i32,
        lpName: *const u16,
    ) -> isize;
    pub fn GetLastError() -> u32;
    pub fn GetCurrentThreadId() -> u32;
    pub fn GetCurrentProcessId() -> u32;
    pub fn GlobalAlloc(uFlags: u32, dwBytes: usize) -> isize;
    pub fn GlobalLock(hMem: isize) -> *mut c_void;
    pub fn GlobalUnlock(hMem: isize) -> i32;
    pub fn GlobalFree(hMem: isize) -> isize;
    /// 启动进程（自己给环境块，见 shortcut.rs 里为什么必须过滤 ELECTRON_*）
    pub fn CreateProcessW(
        lpApplicationName: *const u16,
        lpCommandLine: *mut u16,
        lpProcessAttributes: *mut c_void,
        lpThreadAttributes: *mut c_void,
        bInheritHandles: i32,
        dwCreationFlags: u32,
        lpEnvironment: *mut c_void,
        lpCurrentDirectory: *const u16,
        lpStartupInfo: *mut STARTUPINFOW,
        lpProcessInformation: *mut PROCESS_INFORMATION,
    ) -> i32;
    pub fn GetEnvironmentStringsW() -> *mut u16;
    pub fn FreeEnvironmentStringsW(block: *mut u16) -> i32;
    pub fn CloseHandle(h: isize) -> i32;
    pub fn WaitForSingleObject(h: isize, ms: u32) -> u32;
    pub fn GetExitCodeProcess(h: isize, code: *mut u32) -> i32;
}

#[repr(C)]
pub struct STARTUPINFOW {
    pub cb: u32,
    pub lpReserved: *mut u16,
    pub lpDesktop: *mut u16,
    pub lpTitle: *mut u16,
    pub dwX: u32,
    pub dwY: u32,
    pub dwXSize: u32,
    pub dwYSize: u32,
    pub dwXCountChars: u32,
    pub dwYCountChars: u32,
    pub dwFillAttribute: u32,
    pub dwFlags: u32,
    pub wShowWindow: u16,
    pub cbReserved2: u16,
    pub lpReserved2: *mut u8,
    pub hStdInput: isize,
    pub hStdOutput: isize,
    pub hStdError: isize,
}

impl Default for STARTUPINFOW {
    fn default() -> Self {
        // SAFETY: 全零 + cb 是本结构唯一合法的初始状态
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
pub struct PROCESS_INFORMATION {
    pub hProcess: isize,
    pub hThread: isize,
    pub dwProcessId: u32,
    pub dwThreadId: u32,
}

impl Default for PROCESS_INFORMATION {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

pub const STARTF_USESHOWWINDOW: u32 = 0x0000_0001;
pub const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
pub const SW_SHOWNORMAL: u16 = 1;
/// CreateProcess 需要管理员权限时的错误码
pub const ERROR_ELEVATION_REQUIRED: u32 = 740;
pub const STILL_ACTIVE: u32 = 259;

#[link(name = "gdi32")]
extern "system" {
    pub fn CreateRoundRectRgn(
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        w: i32,
        h: i32,
    ) -> isize;
    /// 临时区域（只用来接收 GetWindowRgn 的拷贝）
    pub fn CreateRectRgn(x1: i32, y1: i32, x2: i32, y2: i32) -> isize;
    /// 区域的外接矩形
    pub fn GetRgnBox(hrgn: isize, lprc: *mut RECT) -> i32;
    pub fn CreateCompatibleDC(hdc: HDC) -> HDC;
    pub fn CreateDIBSection(
        hdc: HDC,
        pbmi: *const BITMAPINFOHEADER,
        usage: u32,
        ppvBits: *mut *mut c_void,
        hSection: isize,
        offset: u32,
    ) -> HBITMAP;
    pub fn SelectObject(hdc: HDC, h: HGDIOBJ) -> HGDIOBJ;
    pub fn DeleteObject(ho: HGDIOBJ) -> i32;
    pub fn DeleteDC(hdc: HDC) -> i32;
    /// 位块传送：截屏 / 拷贝位图
    pub fn BitBlt(
        hdcDest: HDC,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        hdcSrc: HDC,
        x1: i32,
        y1: i32,
        rop: u32,
    ) -> i32;
    /// 屏幕/显示器 DC 的每英寸点数（老系统上取缩放的唯一办法）
    pub fn GetDeviceCaps(hdc: HDC, index: i32) -> i32;
}

/// `GetDeviceCaps` 的「每英寸逻辑像素」索引：96 = 100%，120 = 125%
pub const LOGPIXELSX: i32 = 88;

#[link(name = "user32")]
extern "system" {
    /// 窗口**当前所在显示器**的 DPI（Win10 1607+）。96 = 100%，120 = 125%。
    ///
    /// 注意它跟 `GetDpiForSystem` 不一样：多屏混合缩放时，窗口拖到另一块屏上，
    /// 这个值会跟着变 —— 正是我们要的。
    pub fn GetDpiForWindow(hWnd: HWND) -> u32;
    /// 主屏尺寸（物理像素）
    pub fn GetSystemMetrics(index: i32) -> i32;
}

pub const SM_CXSCREEN: i32 = 0;
pub const SM_CYSCREEN: i32 = 1;

/// 显示器句柄
pub type HMONITOR = isize;

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct MONITORINFO {
    pub cb_size: u32,
    pub rc_monitor: RECT,
    pub rc_work: RECT,
    pub flags: u32,
}

/// `MonitorFromPoint` 第二参：不管点落在哪都返回**主显示器**
pub const MONITOR_DEFAULTTOPRIMARY: u32 = 1;
/// `MonitorFromPoint` 第二参：返回点所在（或最近）的显示器
pub const MONITOR_DEFAULTTONEAREST: u32 = 2;
/// `GetDpiForMonitor` 的 dpiType：有效 DPI（已含用户在显示设置里的缩放）
pub const MDT_EFFECTIVE_DPI: i32 = 0;

#[link(name = "user32")]
extern "system" {
    /// 点所在的显示器句柄（坐标是**物理**像素）
    pub fn MonitorFromPoint(pt: POINT, flags: u32) -> HMONITOR;
    /// 显示器信息：整体矩形、工作区（已排除任务栏）、是否主屏
    pub fn GetMonitorInfoW(hmon: HMONITOR, info: *mut MONITORINFO) -> i32;
}

#[link(name = "shcore")]
extern "system" {
    /// **显示器**的 DPI（Win8.1+）。
    ///
    /// 盒子是 `Progman` 的子窗口，而子窗口的 DPI 跟随父窗口（Explorer 是
    /// system-DPI aware，一直是主屏的 DPI）—— 所以 `GetDpiForWindow` 对盒子
    /// 永远返回主屏的值，拖到另一块缩放不同的屏上就会把窗口算小，
    /// 内容被圆角区域裁掉（表现：「字体下半截不见了」）。
    /// 按显示器查 DPI 才不受窗口层级影响。
    pub fn GetDpiForMonitor(
        hmon: HMONITOR,
        dpi_type: i32,
        dpi_x: *mut u32,
        dpi_y: *mut u32,
    ) -> i32;
}

/// 直接拷贝源像素（截屏用）
pub const SRCCOPY: u32 = 0x00CC_0020;

#[link(name = "shell32")]
extern "system" {
    pub fn SHGetFileInfoW(
        pszPath: *const u16,
        dwFileAttributes: u32,
        psfi: *mut SHFILEINFOW,
        cbFileInfo: u32,
        uFlags: u32,
    ) -> usize;
    pub fn SHGetKnownFolderPath(
        rfid: *const GUID,
        dwFlags: u32,
        hToken: isize,
        ppszPath: *mut *mut u16,
    ) -> i32;
    pub fn ShellExecuteW(
        hwnd: HWND,
        lpOperation: *const u16,
        lpFile: *const u16,
        lpParameters: *const u16,
        lpDirectory: *const u16,
        nShowCmd: i32,
    ) -> isize;
    /// 路径 → PIDL（shell 项标识符），用于构造拖拽数据对象
    pub fn SHParseDisplayName(
        pszName: *const u16,
        pbc: *mut c_void,
        ppidl: *mut *mut c_void,
        sfgaoIn: u32,
        psfgaoOut: *mut u32,
    ) -> i32;
    /// 由一个 shell 项直接造出标准 IDataObject（自带 CF_HDROP / Shell IDList）
    pub fn SHCreateDataObject(
        pidlFolder: *const c_void,
        cidl: u32,
        apidl: *const *const c_void,
        pdtInner: *mut c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> i32;
    /// 拆开 PIDL：拿到父文件夹的 IShellFolder 和最后一段子 PIDL
    pub fn SHBindToParent(
        pidl: *const c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
        ppidlLast: *mut *mut c_void,
    ) -> i32;
    /// 通知 shell「这个路径变了」。不通知的话桌面图标要等 Explorer 自己
    /// 的文件监视轮询（实测约 1 秒）才会消失，拖进去的手感就断了。
    pub fn SHChangeNotify(
        wEventId: i32,
        uFlags: u32,
        dwItem1: *const c_void,
        dwItem2: *const c_void,
    );
    /// 打开某个 shell 项所在的文件夹并选中它。
    ///
    /// 资源管理器右键「打开文件所在位置」内部走的就是它：不新起 explorer 进程、
    /// 优先复用已经开着的窗口，所以不会闪任务栏黑框。
    /// `cidl = 0` 时 `pidlFolder` 当成绝对 PIDL 用，函数自己找父文件夹。
    pub fn SHOpenFolderAndSelectItems(
        pidlFolder: *const c_void,
        cidl: u32,
        apidl: *const *const c_void,
        dwFlags: u32,
    ) -> i32;
    /// 带 verb 的执行入口（`properties` 只能从这里进）
    pub fn ShellExecuteExW(psei: *mut SHELLEXECUTEINFOW) -> i32;
}

pub const SHCNE_CREATE: i32 = 0x0000_0001;
pub const SHCNE_DELETE: i32 = 0x0000_0002;
/// 整个目录内容变了（比逐条 DELETE 更强，Explorer 会重新枚举该目录）
pub const SHCNE_UPDATEDIR: i32 = 0x0000_1000;
pub const SHCNF_PATHW: u32 = 0x0005;

/// 通过 shell 的 IDList 通道调用 verb。
/// 不加这个，`lpVerb = "properties"` 只会落到「文件属性」的兜底实现上，
/// `.lnk` 就拿不到「快捷方式 / 兼容性」那几页。
pub const SEE_MASK_INVOKEIDLIST: u32 = 0x0000_000C;
/// 整个执行过程在本次调用内完成，不依赖调用线程之后还泵消息。
/// 属性对话框会在里面一直阻塞到用户关掉 —— 调用方必须放在独立线程。
pub const SEE_MASK_NOASYNC: u32 = 0x0000_0100;

#[link(name = "ole32")]
extern "system" {
    pub fn CoTaskMemFree(pv: *mut c_void);
    pub fn OleInitialize(pvReserved: *mut c_void) -> i32;
    /// 解析 .lnk 要自己进 STA：任务跑在 runtime 线程上，那里没有初始化过 COM
    pub fn CoInitializeEx(pvReserved: *mut c_void, dwCoInit: u32) -> i32;
    pub fn CoUninitialize();
    pub fn CoCreateInstance(
        rclsid: *const GUID,
        pUnkOuter: *mut c_void,
        dwClsContext: u32,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> i32;
    /// 进入 OLE 拖拽模态循环，直到放下或取消；期间自己泵消息
    pub fn DoDragDrop(
        pDataObj: *mut c_void,
        pDropSource: *mut c_void,
        dwOKEffects: u32,
        pdwEffect: *mut u32,
    ) -> i32;
}

pub const DROPEFFECT_COPY: u32 = 0x1;
pub const DROPEFFECT_MOVE: u32 = 0x2;
pub const DROPEFFECT_LINK: u32 = 0x4;

pub const S_OK: i32 = 0;
pub const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;
pub const DRAGDROP_S_DROP: i32 = 0x0004_0100;
pub const DRAGDROP_S_CANCEL: i32 = 0x0004_0101;
pub const DRAGDROP_S_USEDEFAULTCURSORS: i32 = 0x0004_0102;
pub const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;

/// {0000010E-0000-0000-C000-000000000046} IID_IDataObject
pub const IID_IDATAOBJECT: GUID = GUID {
    data1: 0x0000_010E,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// {000214E6-0000-0000-C000-000000000046} IID_IShellFolder
pub const IID_ISHELLFOLDER: GUID = GUID {
    data1: 0x0002_14E6,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// {00000103-0000-0000-C000-000000000046} IID_IEnumFORMATETC
pub const IID_IENUMFORMATETC: GUID = GUID {
    data1: 0x0000_0103,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// {00000121-0000-0000-C000-000000000046} IID_IDropSource
pub const IID_IDROPSOURCE: GUID = GUID {
    data1: 0x0000_0121,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// {00000000-0000-0000-C000-000000000046} IID_IUnknown
pub const IID_IUNKNOWN: GUID = GUID {
    data1: 0,
    data2: 0,
    data3: 0,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// {00021401-0000-0000-C000-000000000046} CLSID_ShellLink
pub const CLSID_SHELLLINK: GUID = GUID {
    data1: 0x0002_1401,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// {000214F9-0000-0000-C000-000000000046} IID_IShellLinkW
pub const IID_ISHELLLINKW: GUID = GUID {
    data1: 0x0002_14F9,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// {0000010B-0000-0000-C000-000000000046} IID_IPersistFile
pub const IID_IPERSISTFILE: GUID = GUID {
    data1: 0x0000_010B,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

pub const CLSCTX_INPROC_SERVER: u32 = 1;
pub const COINIT_APARTMENTTHREADED: u32 = 2;
/// IPersistFile::Load 的访问模式，STGM_READ
pub const STGM_READ: u32 = 0;

pub fn guid_eq(a: *const GUID, b: &GUID) -> bool {
    if a.is_null() {
        return false;
    }
    unsafe {
        let pa = std::slice::from_raw_parts(a as *const u8, 16);
        let pb = std::slice::from_raw_parts(b as *const GUID as *const u8, 16);
        pa == pb
    }
}

/// 转成以 0 结尾的 UTF-16 缓冲区
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[repr(C)]
pub struct OSVERSIONINFOW {
    pub dwOSVersionInfoSize: u32,
    pub dwMajorVersion: u32,
    pub dwMinorVersion: u32,
    pub dwBuildNumber: u32,
    pub dwPlatformId: u32,
    pub szCSDVersion: [u16; 128],
}

#[link(name = "ntdll")]
extern "system" {
    /// 真实系统版本。**不要用 `GetVersionExW`**：进程 manifest 里没声明 Win10 兼容时
    /// 它会把 Win11 谎报成 6.2，拿不到 build 号。`RtlGetVersion` 不受 manifest 影响。
    pub fn RtlGetVersion(lpVersionInformation: *mut OSVERSIONINFOW) -> i32;
}
