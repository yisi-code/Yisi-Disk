//! 发一个 GET 请求，用 Windows 自带的 WinHTTP（系统代理设置照常生效，跟随 https 之间的跳转，不许从 https
//! 跳到 http）。目前只有程序内更新在用：只在用户点按时调用，程序别的时候不联网。

/// 请求失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("地址不对：{0}")]
    BadUrl(String),
    #[error("{step}：{source}")]
    Network {
        step: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("服务器答复 {0}")]
    Status(u32),
    #[error("内容超过 {0} 字节")]
    TooLarge(usize),
    #[error("调用方中止了下载")]
    Stopped,
    #[error("这个平台不支持")]
    Unsupported,
}

/// 拆开 `https://主机[:端口]/路径` 或 `http://…`：(是否 https, 主机, 端口, 路径)。
fn split_url(url: &str) -> Result<(bool, &str, u16, &str), HttpError> {
    let bad = || HttpError::BadUrl(url.to_owned());
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(bad());
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| bad())?),
        None => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok((secure, host, port, if path.is_empty() { "/" } else { path }))
}

/// GET `url`，返回不超过 `limit` 字节的响应体；状态码不是 200 时报 [`HttpError::Status`]。`headers` 每项一行
/// （不带换行），`user_agent` 是客户端名；每收到一段数据调用一次 `received`，参数是到目前为止收到的字节数，
/// 它返回 `false` 时中止下载（[`HttpError::Stopped`]），例如调用方嫌太慢、要换一个地址。
#[cfg(windows)]
pub fn http_get(
    url: &str,
    limit: usize,
    headers: &[&str],
    user_agent: &str,
    received: &mut dyn FnMut(usize) -> bool,
) -> Result<Vec<u8>, HttpError> {
    use windows_sys::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_FLAG_SECURE,
        WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders,
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
        WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    };

    /// WinHTTP 句柄，离开作用域时关闭。
    struct Handle(*mut core::ffi::c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 句柄由 WinHTTP 打开，只在这里关闭一次。
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() };
    let failed = |step: &'static str| HttpError::Network {
        step,
        source: std::io::Error::last_os_error(),
    };

    let (secure, host, port, path) = split_url(url)?;
    let agent = wide(user_agent);
    // SAFETY: 字符串以 0 结尾、在调用期间有效；代理参数为空表示按系统设置。返回的句柄交给 Handle 关闭。
    let session = Handle(unsafe {
        WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    });
    if session.0.is_null() {
        return Err(failed("WinHttpOpen"));
    }
    // SAFETY: session 有效；四个超时以毫秒计（解析、连接、发送、接收）。
    unsafe { WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 60_000) };
    let host_wide = wide(host);
    // SAFETY: session 有效，主机名以 0 结尾、在调用期间有效。
    let connection = Handle(unsafe { WinHttpConnect(session.0, host_wide.as_ptr(), port, 0) });
    if connection.0.is_null() {
        return Err(failed("WinHttpConnect"));
    }
    let verb = wide("GET");
    let object = wide(path);
    // SAFETY: connection 有效；字符串以 0 结尾；HTTP 版本、引用页、接受类型为空表示默认。
    let request = Handle(unsafe {
        WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            object.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            if secure { WINHTTP_FLAG_SECURE } else { 0 },
        )
    });
    if request.0.is_null() {
        return Err(failed("WinHttpOpenRequest"));
    }
    for header in headers {
        let line = wide(&format!("{header}\r\n"));
        // SAFETY: request 有效；头以 0 结尾，长度给 u32::MAX 表示按 0 结尾计算。
        if unsafe {
            WinHttpAddRequestHeaders(request.0, line.as_ptr(), u32::MAX, WINHTTP_ADDREQ_FLAG_ADD)
        } == 0
        {
            return Err(failed("WinHttpAddRequestHeaders"));
        }
    }
    // SAFETY: request 有效；不带额外的头与请求体。
    if unsafe { WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) } == 0
    {
        return Err(failed("WinHttpSendRequest"));
    }
    // SAFETY: request 有效，请求已发出；保留参数为空。
    if unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) } == 0 {
        return Err(failed("WinHttpReceiveResponse"));
    }
    let mut status = 0u32;
    let mut length = std::mem::size_of::<u32>() as u32;
    // SAFETY: 按数字取状态码，写进本地变量 status，缓冲区长度如实给出。
    let queried = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&raw mut status).cast(),
            &mut length,
            std::ptr::null_mut(),
        )
    };
    if queried == 0 {
        return Err(failed("WinHttpQueryHeaders"));
    }
    if status != 200 {
        return Err(HttpError::Status(status));
    }
    let mut body = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let mut read = 0u32;
        // SAFETY: chunk 在调用期间有效，长度如实给出；read 是本地输出参数。
        let ok = unsafe {
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            )
        };
        if ok == 0 {
            return Err(failed("WinHttpReadData"));
        }
        if read == 0 {
            return Ok(body);
        }
        body.extend_from_slice(chunk.get(..read as usize).unwrap_or_default());
        if body.len() > limit {
            return Err(HttpError::TooLarge(limit));
        }
        if !received(body.len()) {
            return Err(HttpError::Stopped);
        }
    }
}

#[cfg(not(windows))]
pub fn http_get(
    url: &str,
    _limit: usize,
    _headers: &[&str],
    _user_agent: &str,
    _received: &mut dyn FnMut(usize) -> bool,
) -> Result<Vec<u8>, HttpError> {
    split_url(url)?;
    Err(HttpError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_split_into_scheme_host_port_and_path() {
        assert_eq!(
            split_url("https://api.github.com/repos/x/releases/latest").unwrap(),
            (true, "api.github.com", 443, "/repos/x/releases/latest")
        );
        assert_eq!(
            split_url("http://127.0.0.1:8123/latest").unwrap(),
            (false, "127.0.0.1", 8123, "/latest")
        );
        assert_eq!(split_url("https://example.com").unwrap().3, "/");
        for bad in ["ftp://x/y", "https://:80/", "http://h:port/"] {
            assert!(split_url(bad).is_err(), "{bad}");
        }
    }
}
