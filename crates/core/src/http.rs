//! HTTP/1.1: the server side that carries IPP, and a small client for the
//! freee API.

use std::io::{self, Write};
use std::time::Duration;

use crate::net::{self, Conn, Dialer};

use crate::stream::Stream;

const MAX_HEAD: usize = 16 * 1024;
/// How long an idle keep-alive connection is kept open.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RESPONSE: usize = 1024 * 1024;
const BODY_GROWTH: usize = 256 * 1024;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            content_type,
            body: body.into(),
            headers: Vec::new(),
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Response::new(status, "text/plain; charset=utf-8", body)
    }

    pub fn html(body: String) -> Self {
        Response::new(200, "text/html; charset=utf-8", body)
    }

    /// "See Other": where a browser goes after a form submission.
    pub fn redirect(location: &str) -> Self {
        let mut response = Response::text(303, "");
        response
            .headers
            .push(("Location".to_string(), location.to_string()));
        response
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// The request body is larger than the server accepts.
#[derive(Debug)]
pub struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body too large")
    }
}

impl std::error::Error for TooLarge {}

/// One client connection; requests are read one after another (keep-alive).
pub struct Connection {
    stream: Box<dyn Stream>,
    /// Bytes received but not yet consumed.
    buf: Vec<u8>,
}

impl Connection {
    pub fn new(stream: Box<dyn Stream>) -> Self {
        Connection {
            stream,
            buf: Vec::new(),
        }
    }

    pub fn peer(&self) -> String {
        self.stream.peer()
    }

    /// Reads more bytes into the buffer; `Ok(false)` means the peer closed.
    fn fill(&mut self, timeout: Duration) -> io::Result<bool> {
        let mut tmp = [0u8; 8192];
        let n = self.stream.read(&mut tmp, timeout)?;
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(n > 0)
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        let rest = self.buf.split_off(n);
        std::mem::replace(&mut self.buf, rest)
    }

    /// Reads up to and including the next CRLF and returns the line without it.
    fn read_line(&mut self) -> io::Result<String> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = self.take(pos + 2);
                return Ok(String::from_utf8_lossy(&line[..pos]).into_owned());
            }
            if self.buf.len() > MAX_HEAD {
                return Err(invalid("line too long"));
            }
            if !self.fill(READ_TIMEOUT)? {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    /// Moves exactly `n` body bytes into `out`.
    fn read_exact_into(&mut self, out: &mut Vec<u8>, mut n: usize) -> io::Result<()> {
        while n > 0 {
            if self.buf.is_empty() && !self.fill(READ_TIMEOUT)? {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let chunk = n.min(self.buf.len());
            // Grow in fixed steps: doubling a multi-megabyte buffer needs
            // three times its size for a moment, which small devices lack.
            if out.capacity() - out.len() < chunk {
                out.reserve_exact(chunk.max(BODY_GROWTH));
            }
            if chunk == self.buf.len() {
                out.append(&mut self.buf);
            } else {
                out.extend_from_slice(&self.take(chunk));
            }
            n -= chunk;
        }
        Ok(())
    }

    /// Reads the next request. `Ok(None)` means the client is done with the
    /// connection. A body over `max_body` fails with [`TooLarge`].
    pub fn read_request(&mut self, max_body: usize) -> io::Result<Option<Request>> {
        let end = loop {
            if let Some(pos) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            if self.buf.len() > MAX_HEAD {
                return Err(invalid("request head too large"));
            }
            match self.fill(IDLE_TIMEOUT) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(e) if net::is_timeout(&e) && self.buf.is_empty() => return Ok(None),
                Err(e) => return Err(e),
            }
        };
        let head = self.take(end + 4);
        let head =
            std::str::from_utf8(&head[..end]).map_err(|_| invalid("non-UTF-8 request head"))?;
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next().unwrap_or_default().split_whitespace();
        let (Some(method), Some(path)) = (request_line.next(), request_line.next()) else {
            return Err(invalid("bad request line"));
        };
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let mut request = Request {
            method: method.to_string(),
            path: path.to_string(),
            headers,
            body: Vec::new(),
        };

        let chunked = request
            .header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        let length = match request.header("content-length") {
            Some(v) => Some(
                v.parse::<usize>()
                    .map_err(|_| invalid("bad Content-Length"))?,
            ),
            None => None,
        };
        if length.is_some_and(|n| n > max_body) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, TooLarge));
        }
        // CUPS waits for this before sending the document.
        if request
            .header("expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
        {
            self.stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }

        if chunked {
            loop {
                let line = self.read_line()?;
                let size = line.split(';').next().unwrap_or_default().trim();
                let size =
                    usize::from_str_radix(size, 16).map_err(|_| invalid("bad chunk size"))?;
                if size == 0 {
                    // Trailers, if any, end with an empty line.
                    while !self.read_line()?.is_empty() {}
                    break;
                }
                if request.body.len().saturating_add(size) > max_body {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, TooLarge));
                }
                self.read_exact_into(&mut request.body, size)?;
                if !self.read_line()?.is_empty() {
                    return Err(invalid("missing CRLF after chunk"));
                }
            }
        } else if let Some(length) = length {
            request.body.reserve_exact(length);
            self.read_exact_into(&mut request.body, length)?;
        }
        Ok(Some(request))
    }

    pub fn respond(&mut self, response: &Response, keep_alive: bool) -> io::Result<()> {
        let reason = match response.status {
            200 => "OK",
            303 => "See Other",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
            response.status,
            reason,
            response.content_type,
            response.body.len(),
            if keep_alive { "keep-alive" } else { "close" }
        );
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        self.stream.write_all(head.as_bytes())?;
        self.stream.write_all(&response.body)
    }
}

/// Where an API lives: `https://host[:port]` or, for tests, `http://host:port`.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

impl Origin {
    pub fn parse(url: &str) -> Option<Origin> {
        let (tls, rest) = match url.split_once("://")? {
            ("https", rest) => (true, rest),
            ("http", rest) => (false, rest),
            _ => return None,
        };
        let authority = rest.trim_end_matches('/');
        if authority.is_empty() || authority.contains('/') {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, if tls { 443 } else { 80 }),
        };
        Some(Origin {
            tls,
            host: host.to_string(),
            port,
        })
    }

    pub fn url(&self, path: &str) -> String {
        let default_port = if self.tls { 443 } else { 80 };
        let scheme = if self.tls { "https" } else { "http" };
        if self.port == default_port {
            format!("{scheme}://{}{path}", self.host)
        } else {
            format!("{scheme}://{}:{}{path}", self.host, self.port)
        }
    }

    fn host_header(&self) -> String {
        let default_port = if self.tls { 443 } else { 80 };
        if self.port == default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Sends one request on a fresh connection and returns status and body.
/// The body is given in parts so that a large document is not copied.
pub fn request(
    dialer: &dyn Dialer,
    origin: &Origin,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[&[u8]],
) -> io::Result<(u16, Vec<u8>)> {
    let mut conn = if origin.tls {
        dialer.dial_tls(&origin.host, origin.port)?
    } else {
        dialer.dial_tcp(&origin.host, origin.port)?
    };
    let length: usize = body.iter().map(|part| part.len()).sum();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: freee-printer/{}\r\nAccept: application/json\r\nConnection: close\r\n",
        origin.host_header(),
        env!("CARGO_PKG_VERSION")
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if method != "GET" {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes())?;
    for part in body {
        // Small writes keep TLS records within what embedded stacks buffer.
        for chunk in part.chunks(4096) {
            conn.write_all(chunk)?;
        }
    }
    conn.flush()?;
    conn.set_read_timeout(Some(RESPONSE_TIMEOUT))?;
    read_response(conn.as_mut(), MAX_RESPONSE)
}

/// Reads a whole response; the connection is closed by the server afterwards.
fn read_response(conn: &mut dyn Conn, max: usize) -> io::Result<(u16, Vec<u8>)> {
    let mut raw = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let end = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if raw.len() > MAX_HEAD {
            return Err(invalid("response head too large"));
        }
        let n = conn.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "eof in response head",
            ));
        }
        raw.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1)?.parse::<u16>().ok())
        .ok_or_else(|| invalid("bad status line"))?;
    let header = |name: &str| {
        lines
            .clone()
            .filter_map(|line| line.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    let length = header("content-length").and_then(|v| v.parse::<usize>().ok());
    let chunked =
        header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));

    let mut body = raw.split_off(end + 4);
    let limit = match length {
        Some(len) if len > max => return Err(invalid("response too large")),
        Some(len) => len,
        None => max,
    };
    while body.len() < limit {
        let n = conn.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
        if chunked && body.ends_with(b"0\r\n\r\n") {
            break;
        }
    }
    match length {
        Some(len) => body.truncate(len),
        None if body.len() >= max => return Err(invalid("response too large")),
        None => {}
    }
    if chunked {
        Ok((status, dechunk(&body)?))
    } else {
        Ok((status, body))
    }
}

fn dechunk(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let line_end = raw[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| invalid("bad chunk"))?;
        let size_text = std::str::from_utf8(&raw[pos..pos + line_end]).unwrap_or_default();
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
                .map_err(|_| invalid("bad chunk size"))?;
        pos += line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        if pos + size > raw.len() {
            return Err(invalid("truncated chunk"));
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2;
    }
}

/// Percent-encodes a query or form value.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Decodes an `application/x-www-form-urlencoded` body.
pub fn parse_form(body: &[u8]) -> Vec<(String, String)> {
    let decode = |part: &[u8]| {
        let mut out = Vec::with_capacity(part.len());
        let mut i = 0;
        while i < part.len() {
            match part[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < part.len() => {
                    match u8::from_str_radix(
                        std::str::from_utf8(&part[i + 1..i + 3]).unwrap_or("zz"),
                        16,
                    ) {
                        Ok(byte) => {
                            out.push(byte);
                            i += 2;
                        }
                        Err(_) => out.push(b'%'),
                    }
                }
                byte => out.push(byte),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    body.split(|b| *b == b'&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = match pair.iter().position(|b| *b == b'=') {
                Some(pos) => (&pair[..pos], &pair[pos + 1..]),
                None => (pair, &b""[..]),
            };
            (decode(key), decode(value))
        })
        .collect()
}

pub fn form(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    /// Hands out the input in the given pieces and records what was written.
    struct Script {
        input: VecDeque<Vec<u8>>,
        output: Arc<Mutex<Vec<u8>>>,
    }

    impl Stream for Script {
        fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
            let Some(mut piece) = self.input.pop_front() else {
                return Ok(0);
            };
            let n = piece.len().min(buf.len());
            buf[..n].copy_from_slice(&piece[..n]);
            if n < piece.len() {
                self.input.push_front(piece.split_off(n));
            }
            Ok(n)
        }

        fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
            self.output.lock().unwrap().extend_from_slice(data);
            Ok(())
        }

        fn peer(&self) -> String {
            "test".into()
        }
    }

    fn connection(pieces: &[&[u8]]) -> (Connection, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let script = Script {
            input: pieces.iter().map(|p| p.to_vec()).collect(),
            output: output.clone(),
        };
        (Connection::new(Box::new(script)), output)
    }

    #[test]
    fn content_length_and_keep_alive() {
        let (mut conn, _) = connection(&[
            b"POST /ipp/print HTTP/1.1\r\nHost: a\r\nContent-Le",
            b"ngth: 5\r\n\r\nhelloGET / HTTP/1.1\r\n\r\n",
        ]);
        let first = conn.read_request(100).unwrap().unwrap();
        assert_eq!(
            (first.method.as_str(), first.path.as_str(), &first.body[..]),
            ("POST", "/ipp/print", &b"hello"[..])
        );
        assert_eq!(first.header("host"), Some("a"));
        let second = conn.read_request(100).unwrap().unwrap();
        assert_eq!((second.method.as_str(), second.body.len()), ("GET", 0));
        assert!(conn.read_request(100).unwrap().is_none());
    }

    #[test]
    fn chunked_with_expect_continue() {
        let (mut conn, output) = connection(&[
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n",
            b"4\r\nWiki\r\n6;ext=1\r\npe",
            b"dia \r\n0\r\nTrailer: x\r\n\r\n",
        ]);
        let request = conn.read_request(100).unwrap().unwrap();
        assert_eq!(request.body, b"Wikipedia ");
        assert_eq!(
            &output.lock().unwrap()[..],
            b"HTTP/1.1 100 Continue\r\n\r\n"
        );
    }

    #[test]
    fn oversized_bodies_are_rejected() {
        let (mut conn, _) = connection(&[b"POST / HTTP/1.1\r\nContent-Length: 101\r\n\r\n"]);
        let error = conn.read_request(100).err().unwrap();
        assert!(error.get_ref().is_some_and(|e| e.is::<TooLarge>()));
        let (mut conn, _) =
            connection(&[b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n65\r\n"]);
        assert!(conn.read_request(100).is_err());
    }

    #[test]
    fn truncated_body_is_an_error() {
        let (mut conn, _) = connection(&[b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc"]);
        assert!(conn.read_request(100).is_err());
    }

    struct Replay(std::io::Cursor<Vec<u8>>);

    impl Read for Replay {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for Replay {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Conn for Replay {
        fn set_read_timeout(&mut self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn responses() {
        let mut conn = Replay(std::io::Cursor::new(
            b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\n\r\nhelloX".to_vec(),
        ));
        assert_eq!(
            read_response(&mut conn, 100).unwrap(),
            (201, b"hello".to_vec())
        );
        let mut conn = Replay(std::io::Cursor::new(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n".to_vec(),
        ));
        assert_eq!(
            read_response(&mut conn, 100).unwrap(),
            (200, b"abcde".to_vec())
        );
        let mut conn = Replay(std::io::Cursor::new(
            b"HTTP/1.1 500 Oops\r\n\r\nuntil eof".to_vec(),
        ));
        assert_eq!(
            read_response(&mut conn, 100).unwrap(),
            (500, b"until eof".to_vec())
        );
        let mut conn = Replay(std::io::Cursor::new(
            b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\nx".to_vec(),
        ));
        assert!(read_response(&mut conn, 100).is_err());
    }

    #[test]
    fn origins() {
        let origin = Origin::parse("https://api.freee.co.jp").unwrap();
        assert_eq!(
            (origin.tls, origin.host.as_str(), origin.port),
            (true, "api.freee.co.jp", 443)
        );
        assert_eq!(origin.url("/x"), "https://api.freee.co.jp/x");
        let origin = Origin::parse("http://127.0.0.1:18080/").unwrap();
        assert_eq!(
            (origin.tls, origin.port, origin.host_header().as_str()),
            (false, 18080, "127.0.0.1:18080")
        );
        assert!(Origin::parse("ftp://x").is_none());
        assert!(Origin::parse("https://x/path").is_none());
    }

    #[test]
    fn form_decoding() {
        let fields = parse_form(b"client_id=a%20b&secret=x%2By+z&empty=&code");
        assert_eq!(fields[0], ("client_id".to_string(), "a b".to_string()));
        assert_eq!(fields[1], ("secret".to_string(), "x+y z".to_string()));
        assert_eq!(fields[2], ("empty".to_string(), String::new()));
        assert_eq!(fields[3], ("code".to_string(), String::new()));
        assert_eq!(parse_form(b"name=%E3%81%82%")[0].1, "あ%");
    }

    #[test]
    fn form_encoding() {
        assert_eq!(
            form(&[("a", "urn:ietf:wg:oauth:2.0:oob"), ("b", "x y&z")]),
            "a=urn%3Aietf%3Awg%3Aoauth%3A2.0%3Aoob&b=x%20y%26z"
        );
    }
}
