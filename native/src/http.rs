//! 最小 HTTP 客户端：只做一件事 —— GET 一个文本。
//!
//! 为什么用 WinHTTP 而不引 reqwest / ureq：本项目的 Win32 调用全走
//! `windows` crate，不引第三方 crate 就不用为「一个 GET 请求」多编一遍
//! rustls / hyper，也避免版本耦合。
//!
//! WinHTTP 的好处：走系统 TLS（证书链和系统一致）、走系统代理设置、
//! 不会像 WinINet 那样在断网时弹「拨号连接」对话框。
//!
//! 有一个坑必须自己补：WinHTTP **默认不跟随跳转**，会把 301/302 的空响应体
//! 直接交出来。域名转发服务全靠 301 指到真实主机，Gitee 的 `raw` 地址也是
//! 302 到 `raw.giteeusercontent.com`，所以这里自己循环跟随，最多 5 跳。

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_RAW_HEADERS_CRLF, WINHTTP_QUERY_STATUS_CODE,
};
use windows::core::PCWSTR;

use crate::util::wide;

/// 最多跟几跳重定向
const MAX_HOPS: usize = 5;

/// 句柄的 RAII 包装：任何一条 `?` 提前返回都能把句柄关掉。
struct Handle(*mut c_void);

impl Handle {
    fn new(p: *mut c_void) -> Option<Self> {
        if p.is_null() {
            None
        } else {
            Some(Handle(p))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// 把 URL 拆成 (是否 https, 主机, 端口, 路径含查询串)
fn split_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("地址必须以 http:// 或 https:// 开头".into());
    };
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if host_port.is_empty() {
        return Err("地址缺少主机名".into());
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>().map_err(|_| "端口号不合法".to_string())?,
        ),
        None => (host_port, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err("地址缺少主机名".into());
    }
    Ok((secure, host.to_string(), port, path.to_string()))
}

/// 把 Location 头（可能是绝对地址，也可能是 `/xxx` 相对路径）补成完整地址
fn join_url(base: &str, loc: &str) -> String {
    let loc = loc.trim();
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let (secure, host, port, path) = match split_url(base) {
        Ok(v) => v,
        Err(_) => return loc.to_string(),
    };
    let scheme = if secure { "https" } else { "http" };
    let default_port = (secure && port == 443) || (!secure && port == 80);
    let origin = if default_port {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    };
    if loc.starts_with('/') {
        return format!("{origin}{loc}");
    }
    // 相对路径：以当前路径的目录为基准
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    format!("{origin}{dir}{loc}")
}

struct Resp {
    code: u32,
    location: String,
    body: String,
}

/// 发一次 GET（不跟跳转），返回状态码 + Location + 响应体
fn request_once(url: &str, timeout_ms: u32, max_bytes: usize) -> Result<Resp, String> {
    let (secure, host, port, path) = split_url(url)?;
    unsafe {
        let agent = wide("LightBox-Update");
        let session = Handle::new(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            None,
            None,
            0,
        ))
        .ok_or("WinHTTP 初始化失败")?;
        let t = timeout_ms as i32;
        let _ = WinHttpSetTimeouts(session.0, t, t, t, t);

        let host_w = wide(&host);
        let connect = Handle::new(WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), port, 0))
            .ok_or(format!("连不上服务器：{host}"))?;

        let verb = wide("GET");
        let path_w = wide(&path);
        let flags = if secure {
            WINHTTP_FLAG_SECURE
        } else {
            windows::Win32::Networking::WinHttp::WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = Handle::new(WinHttpOpenRequest(
            connect.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            None,
            None,
            std::ptr::null(),
            flags,
        ))
        .ok_or("创建请求失败")?;

        WinHttpSendRequest(request.0, None, None, 0, 0, 0)
            .map_err(|_| "发送请求失败（网络不通？）")?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|_| "读取响应失败（超时或证书有问题）")?;

        let mut code: u32 = 0;
        let mut len: u32 = std::mem::size_of::<u32>() as u32;
        let _ = WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            None,
            Some(&mut code as *mut u32 as *mut c_void),
            &mut len,
            std::ptr::null_mut(),
        );

        let location = header_value(&raw_headers(request.0), "location");

        let mut out: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let mut read: u32 = 0;
            let ok = WinHttpReadData(request.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut read);
            if ok.is_err() || read == 0 {
                break;
            }
            out.extend_from_slice(&buf[..read as usize]);
            if out.len() > max_bytes {
                return Err("响应内容过大".into());
            }
        }
        Ok(Resp {
            code,
            location,
            body: String::from_utf8_lossy(&out).to_string(),
        })
    }
}

/// 取全部响应头（CRLF 分隔）。WinHttpQueryHeaders 要求先问长度再取内容。
unsafe fn raw_headers(request: *mut c_void) -> String {
    let mut len: u32 = 0;
    let _ = WinHttpQueryHeaders(
        request,
        WINHTTP_QUERY_RAW_HEADERS_CRLF,
        None,
        None,
        &mut len,
        std::ptr::null_mut(),
    );
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize / 2) + 2];
    let mut buf_len = (buf.len() * 2) as u32;
    let _ = WinHttpQueryHeaders(
        request,
        WINHTTP_QUERY_RAW_HEADERS_CRLF,
        None,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut buf_len,
        std::ptr::null_mut(),
    );
    let used = (buf_len as usize / 2).min(buf.len());
    crate::util::from_wide(&buf[..used])
}

/// 从头块里取某个头的值（不区分大小写）
fn header_value(headers: &str, name: &str) -> String {
    for line in headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// GET 一个文本资源，自动跟随 301/302 跳转。
///
/// `timeout_ms` 同时用作解析/连接/发送/接收超时（**每次请求**都重新计时）；
/// 超过 `max_bytes` 直接判失败（清单文件只有几百字节，防止被喂垃圾）。
pub fn get_text(url: &str, timeout_ms: u32, max_bytes: usize) -> Result<String, String> {
    let mut current = url.to_string();
    for _hop in 0..=MAX_HOPS {
        let r = request_once(&current, timeout_ms, max_bytes)?;
        if matches!(r.code, 301 | 302 | 303 | 307 | 308) {
            if r.location.is_empty() {
                return Err(format!("服务器返回 HTTP {}，但没有跳转地址", r.code));
            }
            current = join_url(&current, &r.location);
            continue;
        }
        if r.code != 200 {
            return Err(format!("服务器返回 HTTP {}", r.code));
        }
        return Ok(r.body);
    }
    Err("跳转次数过多".into())
}
