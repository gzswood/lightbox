//! 开机自启：直接读写 HKCU\Software\Microsoft\Windows\CurrentVersion\Run。
//!
//! 刻意不调用 reg.exe —— 外部进程在某些安全策略下会被拦，而且会闪黑框。

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE,
};

use crate::util::wide;

const SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "LightBox";

/// 读当前登记的启动命令；不存在返回 None
pub fn command() -> Option<String> {
    unsafe {
        let subkey = wide(SUBKEY);
        let value = wide(VALUE);
        let mut hkey = HKEY::default();
        let rc = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut hkey,
        );
        if rc != ERROR_SUCCESS {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut cb: u32 = (buf.len() * 2) as u32;
        let mut ty = REG_VALUE_TYPE::default();
        let rc = RegQueryValueExW(
            hkey,
            windows::core::PCWSTR(value.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut cb),
        );
        let _ = RegCloseKey(hkey);
        if rc != ERROR_SUCCESS {
            return None;
        }
        let len = (cb as usize / 2).min(buf.len());
        let end = buf[..len].iter().position(|c| *c == 0).unwrap_or(len);
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

pub fn enabled() -> bool {
    command().map(|v| !v.trim().is_empty()).unwrap_or(false)
}

pub fn set(on: bool) -> Result<(), String> {
    unsafe {
        let subkey = wide(SUBKEY);
        let value = wide(VALUE);
        let mut hkey = HKEY::default();
        let rc: WIN32_ERROR = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey.as_ptr()),
            None,
            None,
            Default::default(),
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            None,
            &mut hkey,
            None,
        );
        if rc != ERROR_SUCCESS {
            return Err(format!("打开注册表 Run 键失败（错误码 {}）", rc.0));
        }

        let result = if on {
            let exe = std::env::current_exe().map_err(|e| format!("找不到程序路径：{e}"))?;
            let cmd = format!("\"{}\"", exe.display());
            let data: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes: Vec<u8> = data.iter().flat_map(|c| c.to_le_bytes()).collect();
            let rc = RegSetValueExW(
                hkey,
                windows::core::PCWSTR(value.as_ptr()),
                None,
                REG_SZ,
                Some(&bytes),
            );
            if rc == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("写入开机自启失败（错误码 {}）", rc.0))
            }
        } else {
            let rc = RegDeleteValueW(hkey, windows::core::PCWSTR(value.as_ptr()));
            if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("取消开机自启失败（错误码 {}）", rc.0))
            }
        };
        let _ = RegCloseKey(hkey);
        result
    }
}
