//! 开机自启：直接读写 HKCU\Software\Microsoft\Windows\CurrentVersion\Run。
//! 刻意不调用 reg.exe —— 外部进程在某些安全策略下会被拦，而且会闪黑框。
#![allow(dead_code)]

use crate::win32::*;
use std::ffi::c_void;

const SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "LightBox";

fn subkey_wide() -> Vec<u16> {
    wide(SUBKEY)
}

fn value_wide() -> Vec<u16> {
    wide(VALUE)
}

/// 读当前登记的启动命令；不存在返回 None
pub fn command() -> Option<String> {
    unsafe {
        let mut hkey: HKEY = 0;
        let rc = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey_wide().as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        );
        if rc != ERROR_SUCCESS || hkey == 0 {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut cb: u32 = (buf.len() * 2) as u32;
        let mut ty: u32 = 0;
        let rc = RegQueryValueExW(
            hkey,
            value_wide().as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr() as *mut u8,
            &mut cb,
        );
        RegCloseKey(hkey);
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
        let mut hkey: HKEY = 0;
        let mut disp: u32 = 0;
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey_wide().as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            std::ptr::null_mut::<c_void>(),
            &mut hkey,
            &mut disp,
        );
        if rc != ERROR_SUCCESS || hkey == 0 {
            return Err(format!("打开注册表 Run 键失败（错误码 {rc}）"));
        }

        let result = if on {
            let exe = std::env::current_exe().map_err(|e| format!("找不到程序路径：{e}"))?;
            let cmd = format!("\"{}\"", exe.display());
            let data: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let rc = RegSetValueExW(
                hkey,
                value_wide().as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            );
            if rc == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("写入开机自启失败（错误码 {rc}）"))
            }
        } else {
            let rc = RegDeleteValueW(hkey, value_wide().as_ptr());
            if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("取消开机自启失败（错误码 {rc}）"))
            }
        };
        RegCloseKey(hkey);
        result
    }
}
