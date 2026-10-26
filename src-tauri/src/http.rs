//! 最小 HTTPS 客户端：只做一件事 —— GET 一个文本。
//!
//! 为什么手写 FFI 而不用 reqwest / ureq：本项目的 Win32 调用全是手写声明
//! （见 win32.rs 顶部的说明），不引第三方 crate 就不用为「一个 GET 请求」
//! 多编一遍 rustls / hyper，也避免版本耦合。
//!
//! WinHTTP 的好处：走系统 TLS（证书链和系统一致）、走系统代理设置、
//! 不会像 WinINet 那样在断网时弹「拨号连接」对话框。
//!
//! 除了 GET 本身，这里还替调用方挡掉两类「看起来能开、程序却拿不到数据」的坑：
//!
//! 1. **301/302 跳转**：WinHTTP 默认**不**自动跟随，会直接把 301 的（空）响应体
//!    交出来。域名转发服务（`while.cc.cd` 就是）全靠 301 指到真实主机，
//!    所以这里自己循环跟随，最多 5 跳。
//! 2. **JS 反爬挑战**：`while.hyperphp.com` 这类免费虚拟主机会先返回一段
//!    `<script src="/aes.js">`，要浏览器用 AES 解出 `__test` cookie 才给真内容。
//!    这里用系统自带的 BCrypt 复刻那段解密（AES-128-CBC，无填充），拿到 cookie
//!    再请求一次。见 `challenge_cookie()`。
#![allow(non_snake_case)]

use std::ffi::c_void;

type HINTERNET = *mut c_void;

/// WinHTTP 的访问类型：自动使用系统代理（Win8.1+）
const ACCESS_AUTOMATIC_PROXY: u32 = 4;
const FLAG_SECURE: u32 = 0x0080_0000;
/// 状态码（配合 QUERY_FLAG_NUMBER 取成 u32）
const QUERY_STATUS_CODE: u32 = 19;
/// 全部响应头（CRLF 分隔），自己从中找 Location
const QUERY_RAW_HEADERS_CRLF: u32 = 22;
const QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

/// 最多跟几跳重定向
const MAX_HOPS: usize = 5;

#[link(name = "winhttp")]
extern "system" {
    fn WinHttpOpen(
        agent: *const u16,
        access_type: u32,
        proxy: *const u16,
        bypass: *const u16,
        flags: u32,
    ) -> HINTERNET;
    fn WinHttpConnect(session: HINTERNET, host: *const u16, port: u16, reserved: u32) -> HINTERNET;
    fn WinHttpOpenRequest(
        connect: HINTERNET,
        verb: *const u16,
        object: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept: *const *const u16,
        flags: u32,
    ) -> HINTERNET;
    fn WinHttpSendRequest(
        request: HINTERNET,
        headers: *const u16,
        headers_len: u32,
        optional: *const c_void,
        optional_len: u32,
        total_len: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: HINTERNET, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: HINTERNET,
        info_level: u32,
        name: *const u16,
        buffer: *mut c_void,
        buffer_len: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(
        request: HINTERNET,
        buffer: *mut c_void,
        to_read: u32,
        read: *mut u32,
    ) -> i32;
    fn WinHttpSetTimeouts(
        session: HINTERNET,
        resolve: i32,
        connect: i32,
        send: i32,
        receive: i32,
    ) -> i32;
    fn WinHttpCloseHandle(handle: HINTERNET) -> i32;
}

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptOpenAlgorithmProvider(
        ph_algorithm: *mut *mut c_void,
        alg_id: *const u16,
        implementation: *const u16,
        flags: u32,
    ) -> i32;
    fn BCryptSetProperty(
        object: *mut c_void,
        property: *const u16,
        input: *const u8,
        input_len: u32,
        flags: u32,
    ) -> i32;
    fn BCryptGenerateSymmetricKey(
        h_algorithm: *mut c_void,
        ph_key: *mut *mut c_void,
        key_object: *mut u8,
        key_object_len: u32,
        secret: *const u8,
        secret_len: u32,
        flags: u32,
    ) -> i32;
    fn BCryptDecrypt(
        h_key: *mut c_void,
        input: *const u8,
        input_len: u32,
        padding_info: *const c_void,
        iv: *mut u8,
        iv_len: u32,
        output: *mut u8,
        output_len: u32,
        result: *mut u32,
        flags: u32,
    ) -> i32;
    fn BCryptDestroyKey(h_key: *mut c_void) -> i32;
    fn BCryptCloseAlgorithmProvider(h_algorithm: *mut c_void, flags: u32) -> i32;
}

/// 句柄的 RAII 包装：任何一条 `?` 提前返回都能把句柄关掉。
struct Handle(HINTERNET);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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
fn request_once(
    url: &str,
    cookie: Option<&str>,
    timeout_ms: u32,
    max_bytes: usize,
) -> Result<Resp, String> {
    let (secure, host, port, path) = split_url(url)?;
    unsafe {
        let agent = wide("LightBox-Update");
        let session = Handle(WinHttpOpen(
            agent.as_ptr(),
            ACCESS_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("WinHTTP 初始化失败".into());
        }
        let t = timeout_ms as i32;
        WinHttpSetTimeouts(session.0, t, t, t, t);

        let host_w = wide(&host);
        let connect = Handle(WinHttpConnect(session.0, host_w.as_ptr(), port, 0));
        if connect.0.is_null() {
            return Err(format!("连不上服务器：{host}"));
        }

        let verb = wide("GET");
        let path_w = wide(&path);
        let flags = if secure { FLAG_SECURE } else { 0 };
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            verb.as_ptr(),
            path_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            flags,
        ));
        if request.0.is_null() {
            return Err("创建请求失败".into());
        }
        let extra = cookie.map(|c| wide(&format!("Cookie: {c}\r\n")));
        let (hdr_ptr, hdr_len) = match &extra {
            Some(h) => (h.as_ptr(), (h.len() - 1) as u32),
            None => (std::ptr::null(), 0),
        };
        if WinHttpSendRequest(
            request.0,
            hdr_ptr,
            hdr_len,
            std::ptr::null(),
            0,
            0,
            0,
        ) == 0
        {
            return Err("发送请求失败（网络不通？）".into());
        }
        if WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0 {
            return Err("读取响应失败（超时或证书有问题）".into());
        }

        let mut code: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            QUERY_STATUS_CODE | QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut code as *mut u32 as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
        );

        let location = header_value(&raw_headers(request.0), "location");

        let mut out: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let mut read: u32 = 0;
            let ok = WinHttpReadData(
                request.0,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut read,
            );
            if ok == 0 || read == 0 {
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
unsafe fn raw_headers(request: HINTERNET) -> String {
    let mut len: u32 = 0;
    WinHttpQueryHeaders(
        request,
        QUERY_RAW_HEADERS_CRLF,
        std::ptr::null(),
        std::ptr::null_mut(),
        &mut len,
        std::ptr::null_mut(),
    );
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize / 2) + 2];
    let mut buf_len = (buf.len() * 2) as u32;
    WinHttpQueryHeaders(
        request,
        QUERY_RAW_HEADERS_CRLF,
        std::ptr::null(),
        buf.as_mut_ptr() as *mut c_void,
        &mut buf_len,
        std::ptr::null_mut(),
    );
    let used = (buf_len as usize / 2).min(buf.len());
    String::from_utf16_lossy(&buf[..used])
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

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// AES-CBC 解密（无填充），返回小写 hex —— 完全复刻挑战页里
/// `toHex(slowAES.decrypt(c, 2, a, b))` 的结果。
fn aes_cbc_decrypt_hex(key_hex: &str, iv_hex: &str, data_hex: &str) -> Option<String> {
    let key = hex_to_bytes(key_hex)?;
    let iv = hex_to_bytes(iv_hex)?;
    let data = hex_to_bytes(data_hex)?;
    if key.len() != 16 || iv.len() != 16 || data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    unsafe {
        let alg_name = wide("AES");
        let mut h_alg: *mut c_void = std::ptr::null_mut();
        if BCryptOpenAlgorithmProvider(&mut h_alg, alg_name.as_ptr(), std::ptr::null(), 0) < 0 {
            return None;
        }
        let prop = wide("ChainingMode");
        let val: Vec<u16> = "ChainingModeCBC"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut ok = BCryptSetProperty(
            h_alg,
            prop.as_ptr(),
            val.as_ptr() as *const u8,
            (val.len() * 2) as u32,
            0,
        ) >= 0;
        let mut h_key: *mut c_void = std::ptr::null_mut();
        if ok {
            ok = BCryptGenerateSymmetricKey(
                h_alg,
                &mut h_key,
                std::ptr::null_mut(),
                0,
                key.as_ptr(),
                key.len() as u32,
                0,
            ) >= 0;
        }
        let mut plain: Vec<u8> = Vec::new();
        if ok {
            let mut buf = vec![0u8; data.len()];
            let mut iv_buf = iv.clone();
            let mut done: u32 = 0;
            let st = BCryptDecrypt(
                h_key,
                data.as_ptr(),
                data.len() as u32,
                std::ptr::null(),
                iv_buf.as_mut_ptr(),
                iv_buf.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut done,
                0,
            );
            if st >= 0 {
                buf.truncate(done as usize);
                plain = buf;
            }
            BCryptDestroyKey(h_key);
        }
        BCryptCloseAlgorithmProvider(h_alg, 0);
        if plain.is_empty() {
            return None;
        }
        Some(plain.iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// 是不是那段 JS 反爬挑战页
fn is_challenge(body: &str) -> bool {
    body.contains("slowAES") && body.contains("toNumbers")
}

/// 从挑战页里解出应该带上的 cookie（`__test=<32位hex>`）
fn challenge_cookie(body: &str) -> Option<String> {
    let nums = to_numbers(body);
    if nums.len() < 3 {
        return None;
    }
    let value = aes_cbc_decrypt_hex(&nums[0], &nums[1], &nums[2])?;
    let name = cookie_name(body).unwrap_or_else(|| "__test".to_string());
    Some(format!("{name}={value}"))
}

/// 抓出挑战页里所有 `toNumbers("....")` 的十六进制串
fn to_numbers(body: &str) -> Vec<String> {
    const NEEDLE: &str = "toNumbers(\"";
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(NEEDLE) {
        let after = &rest[i + NEEDLE.len()..];
        let Some(j) = after.find('"') else { break };
        let hex = &after[..j];
        if !hex.is_empty() && hex.len() % 2 == 0 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            out.push(hex.to_string());
        }
        rest = &after[j..];
    }
    out
}

/// 挑战页里写的是 `document.cookie="__test="+...`，cookie 名从这儿取
fn cookie_name(body: &str) -> Option<String> {
    const NEEDLE: &str = "document.cookie=\"";
    let i = body.find(NEEDLE)?;
    let after = &body[i + NEEDLE.len()..];
    let j = after.find('=')?;
    let name = &after[..j];
    let ok = !name.is_empty()
        && name.len() < 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then(|| name.to_string())
}

/// GET 一个文本资源，自动跟随 301/302 跳转、自动通过 AES 反爬挑战。
///
/// `timeout_ms` 同时用作解析/连接/发送/接收超时（**每次请求**都重新计时）；
/// 超过 `max_bytes` 直接判失败（清单文件只有几百字节，防止被喂垃圾）。
pub fn get_text(url: &str, timeout_ms: u32, max_bytes: usize) -> Result<String, String> {
    let mut current = url.to_string();
    let mut cookie: Option<String> = None;
    let mut tried_cookie = false;

    for _hop in 0..=(MAX_HOPS + 2) {
        let r = request_once(&current, cookie.as_deref(), timeout_ms, max_bytes)?;

        // 跳转：自己跟，WinHTTP 默认不跟
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

        // 反爬挑战：解一次 cookie 重试；再被挡就认输
        if is_challenge(&r.body) {
            if tried_cookie {
                return Err("站点要求 JS 验证，且验证没有通过".into());
            }
            cookie = challenge_cookie(&r.body)
                .ok_or_else(|| "站点要求 JS 验证，解不开挑战参数".to_string())?
                .into();
            tried_cookie = true;
            continue;
        }
        return Ok(r.body);
    }
    Err("跳转次数过多".into())
}
