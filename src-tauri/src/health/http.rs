//! One HTTP GET for a session that named an http(s) URL.
//!
//! The result is only what that request returned. It is not a second opinion
//! about [`super::port_open`], and it does not say which process accepted the
//! connection. Nothing here stops a process, restarts a session, or asks for
//! an administrator.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};

use super::{HttpProbe, PROBE_TIMEOUT};

/// Issue one GET when `raw` is an http(s) URL. `None` means no request was sent.
pub(super) fn probe(raw: &str) -> Option<HttpProbe> {
    let target = HttpTarget::parse(raw)?;
    Some(send(&target))
}

struct HttpTarget {
    secure: bool,
    /// Host without IPv6 brackets.
    host: String,
    port: u16,
    /// Path and query, beginning with `/`, as the URL serialized them.
    path: String,
}

impl HttpTarget {
    fn parse(raw: &str) -> Option<Self> {
        let parsed = url::Url::parse(raw.trim()).ok()?;
        let secure = match parsed.scheme() {
            "https" => true,
            "http" => false,
            _ => return None,
        };
        let host = bare_host(parsed.host_str()?).to_owned();
        if host.is_empty() {
            return None;
        }
        // A URL that spells its port as `:0` names no port at all: on the
        // secure path `0` reaches `WinHttpConnect`, whose documented reading
        // is "the default port", and the probe would answer a service on 443
        // in this URL's name. That is not a request the Hub can make honestly,
        // so no probe is issued.
        if parsed.port() == Some(0) {
            return None;
        }
        let port = parsed.port_or_known_default()?;
        let path = request_path(&parsed)?;
        Some(Self {
            secure,
            host,
            port,
            path,
        })
    }

    fn host_header(&self) -> String {
        let displayed = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default_port = if self.secure { 443 } else { 80 };
        if self.port == default_port {
            displayed
        } else {
            format!("{displayed}:{}", self.port)
        }
    }

    fn socket_addr(&self) -> Option<SocketAddr> {
        if let Ok(ip) = self.host.parse::<IpAddr>() {
            return Some(SocketAddr::new(ip, self.port));
        }
        if self.host.eq_ignore_ascii_case("localhost") {
            return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port));
        }
        // A name the URL named. The connect and the read are bounded; the
        // resolver std exposes is not, which is why the usual config is an
        // address rather than a name (`127.0.0.1`).
        let mut addresses = (self.host.as_str(), self.port).to_socket_addrs().ok()?;
        addresses.next()
    }
}

fn bare_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}

/// The path and query from the URL's own serialization, never a decoded form
/// that could put a raw space or newline into the request line.
fn request_path(parsed: &url::Url) -> Option<String> {
    let (_, after_scheme) = parsed.as_str().split_once("://")?;
    let path = match after_scheme.find('/') {
        Some(index) => {
            let with_query = &after_scheme[index..];
            with_query
                .split_once('#')
                .map(|(path, _)| path)
                .unwrap_or(with_query)
        }
        None => "/",
    };
    if path.is_empty()
        || !path.starts_with('/')
        || path.len() > 2048
        || path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return None;
    }
    Some(path.to_owned())
}

fn send(target: &HttpTarget) -> HttpProbe {
    if target.secure {
        return send_secure(target);
    }
    send_cleartext(target)
}

fn send_cleartext(target: &HttpTarget) -> HttpProbe {
    let Some(address) = target.socket_addr() else {
        return HttpProbe::failed();
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&address, PROBE_TIMEOUT) else {
        return HttpProbe::failed();
    };
    if stream.set_read_timeout(Some(PROBE_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(PROBE_TIMEOUT)).is_err()
    {
        return HttpProbe::failed();
    }
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: LocalConsoleHub\r\n\r\n",
        path = target.path,
        host = target.host_header(),
    );
    if stream.write_all(request.as_bytes()).is_err() || stream.flush().is_err() {
        return HttpProbe::failed();
    }
    match read_status(&mut stream) {
        Some(status) => HttpProbe::from_status(status),
        None => HttpProbe::failed(),
    }
}

fn read_status(stream: &mut TcpStream) -> Option<u16> {
    let mut buf = [0u8; 256];
    let mut filled = 0usize;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                if buf[..filled].contains(&b'\n') {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    let line = buf[..filled].split(|&byte| byte == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim();
    let mut parts = line.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

#[cfg(windows)]
fn send_secure(target: &HttpTarget) -> HttpProbe {
    winhttp::exchange(target)
}

#[cfg(not(windows))]
fn send_secure(_target: &HttpTarget) -> HttpProbe {
    // No TLS stack in this build. The GET did not complete; that is a failed
    // probe, recorded beside the TCP reading rather than instead of it.
    HttpProbe::failed()
}

#[cfg(windows)]
mod winhttp {
    use super::{HttpProbe, HttpTarget, PROBE_TIMEOUT};
    use std::os::raw::c_void;

    type HInternet = *mut c_void;

    const ACCESS_NO_PROXY: u32 = 1;
    const FLAG_SECURE: u32 = 0x0080_0000;
    const QUERY_STATUS_CODE: u32 = 19;
    const QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
    const OPTION_REDIRECT_POLICY: u32 = 88;
    const REDIRECT_POLICY_NEVER: u32 = 0;

    #[link(name = "winhttp")]
    extern "system" {
        fn WinHttpOpen(
            agent: *const u16,
            access_type: u32,
            proxy: *const u16,
            bypass: *const u16,
            flags: u32,
        ) -> HInternet;
        fn WinHttpSetTimeouts(
            session: HInternet,
            resolve: i32,
            connect: i32,
            send: i32,
            receive: i32,
        ) -> i32;
        fn WinHttpConnect(
            session: HInternet,
            server: *const u16,
            port: u16,
            reserved: u32,
        ) -> HInternet;
        fn WinHttpOpenRequest(
            connect: HInternet,
            verb: *const u16,
            object_name: *const u16,
            version: *const u16,
            referrer: *const u16,
            accept_types: *const *const u16,
            flags: u32,
        ) -> HInternet;
        fn WinHttpSetOption(
            internet: HInternet,
            option: u32,
            buffer: *const c_void,
            length: u32,
        ) -> i32;
        fn WinHttpSendRequest(
            request: HInternet,
            headers: *const u16,
            headers_length: u32,
            optional: *const c_void,
            optional_length: u32,
            total_length: u32,
            context: usize,
        ) -> i32;
        fn WinHttpReceiveResponse(request: HInternet, reserved: *mut c_void) -> i32;
        fn WinHttpQueryHeaders(
            request: HInternet,
            info: u32,
            name: *const u16,
            buffer: *mut c_void,
            length: *mut u32,
            index: *mut u32,
        ) -> i32;
        fn WinHttpCloseHandle(internet: HInternet) -> i32;
    }

    struct Handle(HInternet);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: this guard owns the handle and nothing else closes it.
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    pub(super) fn exchange(target: &HttpTarget) -> HttpProbe {
        let timeout = i32::try_from(PROBE_TIMEOUT.as_millis()).unwrap_or(i32::MAX);
        let agent = wide("LocalConsoleHub");
        let host = wide(&target.host);
        let verb = wide("GET");
        let path = wide(&target.path);
        // SAFETY: every string is NUL-terminated and lives until the call that
        // copies it returns. Handles are closed by `Handle`. A null proxy,
        // version, referrer, and accept list are the documented "none" values.
        // Redirects are disabled so the status is the one this GET returned,
        // and certificate failures stay failures — nothing here ignores them.
        unsafe {
            let session = WinHttpOpen(
                agent.as_ptr(),
                ACCESS_NO_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
            if session.is_null() {
                return HttpProbe::failed();
            }
            let session = Handle(session);
            if WinHttpSetTimeouts(session.0, timeout, timeout, timeout, timeout) == 0 {
                return HttpProbe::failed();
            }
            let connect = WinHttpConnect(session.0, host.as_ptr(), target.port, 0);
            if connect.is_null() {
                return HttpProbe::failed();
            }
            let connect = Handle(connect);
            let flags = if target.secure { FLAG_SECURE } else { 0 };
            let request = WinHttpOpenRequest(
                connect.0,
                verb.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                flags,
            );
            if request.is_null() {
                return HttpProbe::failed();
            }
            let request = Handle(request);
            let mut policy = REDIRECT_POLICY_NEVER;
            if WinHttpSetOption(
                request.0,
                OPTION_REDIRECT_POLICY,
                &mut policy as *mut u32 as *const c_void,
                u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4),
            ) == 0
            {
                return HttpProbe::failed();
            }
            if WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0
                || WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0
            {
                return HttpProbe::failed();
            }
            let mut status = 0u32;
            let mut length = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4);
            if WinHttpQueryHeaders(
                request.0,
                QUERY_STATUS_CODE | QUERY_FLAG_NUMBER,
                std::ptr::null(),
                &mut status as *mut u32 as *mut c_void,
                &mut length,
                std::ptr::null_mut(),
            ) == 0
            {
                return HttpProbe::failed();
            }
            u16::try_from(status)
                .map(HttpProbe::from_status)
                .unwrap_or_else(|_| HttpProbe::failed())
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16()
            .filter(|unit| *unit != 0)
            .chain([0])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL that spells `:0` names no port to probe: handing `0` to the
    /// secure path's connect would dial the scheme's default port in this
    /// URL's name. A URL without a port still resolves to that default.
    #[test]
    fn an_explicit_zero_port_is_not_probeable() {
        assert!(HttpTarget::parse("https://127.0.0.1:0/").is_none());
        assert!(HttpTarget::parse("http://127.0.0.1:0/status").is_none());
        assert_eq!(HttpTarget::parse("https://127.0.0.1/").unwrap().port, 443);
        assert_eq!(HttpTarget::parse("http://127.0.0.1/").unwrap().port, 80);
    }
}
