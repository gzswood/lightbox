//! 托盘图标。
//!
//! 托盘图标需要一个窗口来接收回调消息，所以它挂在主窗口上。

use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_SETVERSION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, LoadImageW, HICON, IMAGE_FLAGS, IMAGE_ICON, LR_DEFAULTSIZE,
};

use crate::util::wide;

/// 托盘图标发给我们的回调消息
pub const WM_TRAY: u32 = 0x8000 + 1;
/// 托盘图标 id（同一个窗口只有一个）
pub const TRAY_ID: u32 = 1;

/// 我们自己的资源里那个图标（app.rc 里的 `1 ICON`）
const RES_ICON: u16 = 1;

unsafe impl Send for Tray {}
unsafe impl Sync for Tray {}

pub struct Tray {
    pub nid: NOTIFYICONDATAW,
    /// 自己 CreateIconIndirect 出来的图标要自己销毁（资源里的图标由系统管，不能删）
    owned: Option<HICON>,
}

/// v0.9.33 用的托盘图标：32×32 PNG 内嵌进 exe，运行时解码成 HICON。
///
/// 之前直接用 exe 资源里的应用图标（圆角方块 + 白色方框），和旧版托盘
/// （蓝底 + 白色盒子剪影）不是同一个图，所以看起来「换了图标」。
fn load_tray_png() -> Option<HICON> {
    let bmp = crate::icons::decode_png_bytes(include_bytes!("../assets/tray.png"))?;
    crate::icons::bmp_to_hicon(&bmp)
}

fn load_app_icon() -> Option<HICON> {
    unsafe {
        // hInstance 必须是「自己这个 exe 的模块句柄」：传 None 是去系统资源里找，
        // 那个图标不存在，结果就是托盘图标位置一片空白。
        let hinst = GetModuleHandleW(None).ok()?;
        // 资源名要走 MAKEINTRESOURCE：直接把 id 当指针用（不是指向变量的指针）
        let name = windows::core::PCWSTR(RES_ICON as usize as *const u16);
        LoadImageW(
            Some(hinst.into()),
            name,
            IMAGE_ICON,
            0,
            0,
            IMAGE_FLAGS(LR_DEFAULTSIZE.0),
        )
        .ok()
        .map(|h| HICON(h.0))
    }
}

impl Tray {
    /// 把托盘图标挂到窗口上
    pub fn install(hwnd: HWND, tip: &str) -> Result<Tray, String> {
        // 优先用旧版那枚 PNG；实在解不出来再退回 exe 资源图标
        let (icon, owned) = match load_tray_png() {
            Some(h) => (Some(h), Some(h)),
            None => (load_app_icon(), None),
        };
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            ..Default::default()
        };
        if let Some(h) = icon {
            nid.hIcon = h;
        } else {
            crate::log::info("托盘图标加载失败，托盘里会是一个空白图标");
        }
        write_tip(&mut nid, tip);
        unsafe {
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                if let Some(h) = owned {
                    let _ = DestroyIcon(h);
                }
                return Err("创建托盘图标失败".into());
            }
            // 4 = NOTIFYICON_VERSION_4，消息参数是 LOWORD(lParam)=事件、HIWORD=图标 id
            nid.Anonymous.uVersion = 4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
        Ok(Tray { nid, owned })
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid);
            if let Some(h) = self.owned.take() {
                let _ = DestroyIcon(h);
            }
        }
    }
}

fn write_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let w = wide(tip);
    let n = w.len().min(nid.szTip.len() - 1);
    nid.szTip = [0u16; 128];
    nid.szTip[..n].copy_from_slice(&w[..n]);
}
