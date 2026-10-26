//! HTTP/1.1 with keepalive. Static files go out with sendfile; everything
//! else is queued for a PHP thread.

use std::net::SocketAddr;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering::*};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, watch};
use tokio::time::timeout_at;

use crate::config::Config;
use crate::logln;
use crate::metrics::METRICS;
use crate::php::Pool;
use crate::php::job::{self, Reply};
use crate::statics;

const MAX_HEAD: usize = 64 << 10;
const MAX_HEADERS: usize = 128;
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Server {
    pub cfg: &'static Config,
    pub pool: &'static Pool,
    pub shutdown: watch::Receiver<bool>,
    pub inflight: AtomicUsize,
}

/// One parsed request.
pub struct Req {
    pub head: Bytes,
    pub method: Range<usize>,
    pub target: Range<usize>,
    pub minor: u8,
    pub headers: Vec<(Range<usize>, Range<usize>)>,
}

impl Req {
    pub fn method(&self) -> &[u8] {
        &self.head[self.method.clone()]
    }

    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| self.head[n.clone()].eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| &self.head[v.clone()])
    }

    fn has_token(&self, name: &str, token: &str) -> bool {
        self.headers
            .iter()
            .filter(|(n, _)| self.head[n.clone()].eq_ignore_ascii_case(name.as_bytes()))
            .any(|(_, v)| {
                self.head[v.clone()]
                    .split(|&b| b == b',')
                    .any(|t| trim(t).eq_ignore_ascii_case(token.as_bytes()))
            })
    }

    fn is_head(&self) -> bool {
        self.method() == b"HEAD"
    }
}

struct InflightGuard<'a>(&'a AtomicUsize);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, AcqRel);
    }
}

enum Parsed {
    Done(Req, usize),
    Partial,
    Bad(u16),
}

fn parse_head(buf: &[u8]) -> Parsed {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut r = httparse::Request::new(&mut headers);
    let base = buf.as_ptr() as usize;
    let off = |s: &[u8]| {
        let start = s.as_ptr() as usize - base;
        start..start + s.len()
    };
    match r.parse(buf) {
        Ok(httparse::Status::Complete(n)) => {
            let method = off(r.method.unwrap_or("").as_bytes());
            let target = off(r.path.unwrap_or("").as_bytes());
            let minor = r.version.unwrap_or(1);
            let headers = r.headers.iter().map(|h| (off(h.name.as_bytes()), off(h.value))).collect();
            Parsed::Done(Req { head: Bytes::new(), method, target, minor, headers }, n)
        }
        Ok(httparse::Status::Partial) => Parsed::Partial,
        Err(httparse::Error::TooManyHeaders) => Parsed::Bad(431),
        Err(_) => Parsed::Bad(400),
    }
}

pub async fn serve_conn(mut stream: TcpStream, remote: SocketAddr, local: SocketAddr, srv: &'static Server) {
    let _ = stream.set_nodelay(true);
    let mut shutdown = srv.shutdown.clone();
    let mut buf = BytesMut::with_capacity(8 << 10);

    loop {
        // Read a full request head.
        let (mut req, head_len) = loop {
            if !buf.is_empty() {
                match parse_head(&buf) {
                    Parsed::Done(req, n) => break (req, n),
                    Parsed::Partial => {}
                    Parsed::Bad(status) => {
                        let _ = simple(&mut stream, status, 1, false, false).await;
                        return;
                    }
                }
                if buf.len() >= MAX_HEAD {
                    let _ = simple(&mut stream, 431, 1, false, false).await;
                    return;
                }
            }
            let read = if buf.is_empty() {
                if *shutdown.borrow() {
                    return;
                }
                tokio::select! {
                    r = tokio::time::timeout(IDLE_TIMEOUT, stream.read_buf(&mut buf)) => r,
                    _ = shutdown.changed() => return,
                }
            } else {
                tokio::time::timeout(HEAD_TIMEOUT, stream.read_buf(&mut buf)).await
            };
            match read {
                Ok(Ok(n)) if n > 0 => {}
                _ => return,
            }
        };

        srv.inflight.fetch_add(1, AcqRel);
        let _guard = InflightGuard(&srv.inflight);
        let received = Instant::now();
        let deadline = tokio::time::Instant::now() + srv.cfg.php_timeout;

        req.head = buf.split_to(head_len).freeze();
        let minor = req.minor;
        let mut keepalive = if minor == 0 {
            req.has_token("connection", "keep-alive")
        } else {
            !req.has_token("connection", "close")
        };

        // Body framing. Both headers at once is a smuggling attempt.
        let chunked = req.header("transfer-encoding").is_some();
        if chunked && !req.has_token("transfer-encoding", "chunked") {
            let _ = simple(&mut stream, 501, minor, false, req.is_head()).await;
            return;
        }
        let content_length = match content_length(&req) {
            Ok(v) => v,
            Err(()) => {
                let _ = simple(&mut stream, 400, minor, false, req.is_head()).await;
                return;
            }
        };
        if chunked && content_length.is_some() {
            let _ = simple(&mut stream, 400, minor, false, req.is_head()).await;
            return;
        }
        let limit = srv.cfg.body_limit;
        if content_length.is_some_and(|n| n > limit) {
            let _ = simple(&mut stream, 413, minor, false, req.is_head()).await;
            return;
        }
        let wants_body = chunked || content_length.is_some_and(|n| n > 0);
        if wants_body && req.has_token("expect", "100-continue") && buf.is_empty() {
            if stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.is_err() {
                return;
            }
        }
        let body = if chunked {
            match read_chunked(&mut stream, &mut buf, limit, deadline).await {
                Ok(b) => b,
                Err(BodyErr::TooLarge) => {
                    let _ = simple(&mut stream, 413, minor, false, req.is_head()).await;
                    return;
                }
                Err(BodyErr::Bad) => {
                    let _ = simple(&mut stream, 400, minor, false, req.is_head()).await;
                    return;
                }
                Err(BodyErr::Eof) => return,
            }
        } else if let Some(n) = content_length.filter(|&n| n > 0) {
            let n = n as usize;
            while buf.len() < n {
                buf.reserve(n - buf.len());
                match timeout_at(deadline, stream.read_buf(&mut buf)).await {
                    Ok(Ok(r)) if r > 0 => {}
                    _ => return,
                }
            }
            buf.split_to(n).freeze()
        } else {
            Bytes::new()
        };

        if *shutdown.borrow() {
            keepalive = false;
        }

        let ok = route(&mut stream, srv, req, body, remote, local, received, deadline, keepalive).await;
        match ok {
            Some(true) if !*shutdown.borrow() => {}
            _ => return,
        }
        // Keep the buffer small between requests.
        if buf.is_empty() && buf.capacity() > 64 << 10 {
            buf = BytesMut::with_capacity(8 << 10);
        }
    }
}

fn content_length(req: &Req) -> Result<Option<u64>, ()> {
    let mut found: Option<u64> = None;
    for (n, v) in &req.headers {
        if req.head[n.clone()].eq_ignore_ascii_case(b"content-length") {
            let s = std::str::from_utf8(trim(&req.head[v.clone()])).map_err(|_| ())?;
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(());
            }
            let n: u64 = s.parse().map_err(|_| ())?;
            if found.is_some_and(|f| f != n) {
                return Err(());
            }
            found = Some(n);
        }
    }
    Ok(found)
}

enum BodyErr {
    TooLarge,
    Bad,
    Eof,
}

async fn read_more(stream: &mut TcpStream, buf: &mut BytesMut, deadline: tokio::time::Instant) -> Result<(), BodyErr> {
    match timeout_at(deadline, stream.read_buf(buf)).await {
        Ok(Ok(n)) if n > 0 => Ok(()),
        _ => Err(BodyErr::Eof),
    }
}

async fn read_chunked(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    limit: u64,
    deadline: tokio::time::Instant,
) -> Result<Bytes, BodyErr> {
    let mut out = BytesMut::new();
    loop {
        let line_end = loop {
            if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                break i;
            }
            if buf.len() > 4096 {
                return Err(BodyErr::Bad);
            }
            read_more(stream, buf, deadline).await?;
        };
        let line = &buf[..line_end];
        let size_part = line.split(|&b| b == b';').next().unwrap_or(b"");
        let size_str = std::str::from_utf8(trim(size_part)).map_err(|_| BodyErr::Bad)?;
        let size = usize::from_str_radix(size_str, 16).map_err(|_| BodyErr::Bad)?;
        buf.advance(line_end + 2);

        if size == 0 {
            // Trailers end with an empty line.
            loop {
                let i = loop {
                    if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                        break i;
                    }
                    if buf.len() > 8192 {
                        return Err(BodyErr::Bad);
                    }
                    read_more(stream, buf, deadline).await?;
                };
                buf.advance(i + 2);
                if i == 0 {
                    return Ok(out.freeze());
                }
            }
        }
        if (out.len() + size) as u64 > limit {
            return Err(BodyErr::TooLarge);
        }
        while buf.len() < size + 2 {
            buf.reserve(size + 2 - buf.len());
            read_more(stream, buf, deadline).await?;
        }
        if &buf[size..size + 2] != b"\r\n" {
            return Err(BodyErr::Bad);
        }
        out.extend_from_slice(&buf[..size]);
        buf.advance(size + 2);
    }
}

/// Returns Some(keepalive) when the connection may continue.
#[allow(clippy::too_many_arguments)]
async fn route(
    stream: &mut TcpStream,
    srv: &'static Server,
    req: Req,
    body: Bytes,
    remote: SocketAddr,
    local: SocketAddr,
    received: Instant,
    deadline: tokio::time::Instant,
    keepalive: bool,
) -> Option<bool> {
    let minor = req.minor;
    let is_head = req.is_head();
    let target = &req.head[req.target.clone()];

    // Origin form, or absolute form from a proxy.
    let mut path_start = req.target.start;
    if target.starts_with(b"http://") || target.starts_with(b"https://") {
        let after = target.iter().position(|&b| b == b':').unwrap() + 3;
        match target[after..].iter().position(|&b| b == b'/') {
            Some(p) => path_start += after + p,
            None => {
                return simple(stream, 400, minor, false, is_head).await.ok().map(|_| false);
            }
        }
    }
    let rel = &req.head[path_start..req.target.end];
    if !rel.starts_with(b"/") {
        return simple(stream, 400, minor, false, is_head).await.ok().map(|_| false);
    }
    let q = rel.iter().position(|&b| b == b'?');
    let path_range = path_start..path_start + q.unwrap_or(rel.len());
    let query = q.map(|q| path_start + q + 1..req.target.end);

    let Some(decoded) = percent_decode(&req.head[path_range.clone()]) else {
        return simple(stream, 400, minor, false, is_head).await.ok().map(|_| false);
    };

    // Dot segments: .env, .git, ., .. — never served, never run.
    if decoded.split(|&b| b == b'/').any(|seg| seg.first() == Some(&b'.')) {
        METRICS.rejected_dotfiles.fetch_add(1, Relaxed);
        return simple(stream, 403, minor, keepalive, is_head).await.ok().map(|_| keepalive);
    }

    match statics::lookup(&srv.cfg.server_root, &decoded) {
        statics::Lookup::File(path, meta) => {
            let m = req.method();
            if m != b"GET" && m != b"HEAD" {
                let r = write_response(stream, 405, minor, keepalive, is_head, b"Allow: GET, HEAD\r\n", b"").await;
                return r.ok().map(|_| keepalive);
            }
            METRICS.static_requests.fetch_add(1, Relaxed);
            return statics::serve(stream, &req, &path, &meta, keepalive).await.ok().map(|_| keepalive);
        }
        statics::Lookup::AddSlash => {
            let mut loc = b"Location: ".to_vec();
            loc.extend_from_slice(&req.head[path_range.clone()]);
            loc.push(b'/');
            if let Some(q) = &query {
                loc.push(b'?');
                loc.extend_from_slice(&req.head[q.clone()]);
            }
            loc.extend_from_slice(b"\r\n");
            let r = write_response(stream, 301, minor, keepalive, is_head, &loc, b"").await;
            return r.ok().map(|_| keepalive);
        }
        statics::Lookup::Php => {}
    }

    // PHP.
    let seq = srv.pool.next_seq();
    let (tx, rx) = oneshot::channel();
    let parts = job::Parts {
        head: req.head.clone(),
        method: req.method.clone(),
        target: req.target.clone(),
        path: path_range,
        query,
        minor,
        headers: &req.headers,
        body,
        remote,
        local,
    };
    let job = job::build(parts, srv.cfg, seq, tx, received);
    let shared = job.shared.clone();
    srv.pool.submit(job);

    let reply = match timeout_at(deadline, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            // The thread dropped the request without answering.
            return simple(stream, 502, minor, false, is_head).await.ok().map(|_| false);
        }
        Err(_) => {
            timed_out(srv, &req, &shared, seq, received);
            return simple(stream, 504, minor, false, is_head).await.ok().map(|_| false);
        }
    };

    match reply {
        Reply::Full { status, head, php_len, body } => {
            let mut h = head;
            if is_head {
                if let Some(l) = php_len {
                    h.extend_from_slice(format!("Content-Length: {l}\r\n").as_bytes());
                }
            }
            let r = write_php(stream, status, minor, keepalive, is_head, &h, &body).await;
            r.ok().map(|_| keepalive)
        }
        Reply::Stream { status, head, php_len, mut rx } => {
            let bodyless = is_head || status == 204 || status == 304 || status < 200;
            let chunked = !bodyless && php_len.is_none() && minor == 1;
            let keep = keepalive && (bodyless || php_len.is_some() || chunked);
            let mut h = status_line(status, minor);
            push_date(&mut h);
            h.extend_from_slice(&head);
            if let Some(l) = php_len {
                h.extend_from_slice(format!("Content-Length: {l}\r\n").as_bytes());
            } else if chunked {
                h.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
            }
            connection_header(&mut h, minor, keep);
            h.extend_from_slice(b"\r\n");
            METRICS.http(status);
            if stream.write_all(&h).await.is_err() {
                return None;
            }
            loop {
                match timeout_at(deadline, rx.recv()).await {
                    Ok(Some(chunk)) => {
                        if bodyless || chunk.is_empty() {
                            continue;
                        }
                        let res = if chunked {
                            let mut c = format!("{:x}\r\n", chunk.len()).into_bytes();
                            c.extend_from_slice(&chunk);
                            c.extend_from_slice(b"\r\n");
                            stream.write_all(&c).await
                        } else {
                            stream.write_all(&chunk).await
                        };
                        if res.is_err() {
                            return None;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        timed_out(srv, &req, &shared, seq, received);
                        return None;
                    }
                }
            }
            if chunked && stream.write_all(b"0\r\n\r\n").await.is_err() {
                return None;
            }
            Some(keep)
        }
    }
}

fn timed_out(srv: &Server, req: &Req, shared: &job::JobShared, seq: u64, received: Instant) {
    let thread = srv.pool.abandon(shared, seq);
    let path = String::from_utf8_lossy(&req.head[req.target.clone()]);
    let path = path.split('?').next().unwrap_or("");
    let method = String::from_utf8_lossy(req.method());
    let where_ = match thread {
        Some(t) => format!("thread {t}"),
        None => "queue".to_string(),
    };
    logln!(
        "request timed out (PHP_TIMEOUT={}s) in {where_}: method={method} path={path} script={} time={:.3}s",
        srv.cfg.php_timeout.as_secs(),
        srv.pool.script(),
        received.elapsed().as_secs_f64()
    );
}

fn percent_decode(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        if b == b'%' {
            let hex = |c: u8| (c as char).to_digit(16);
            let hi = hex(*s.get(i + 1)?)?;
            let lo = hex(*s.get(i + 2)?)?;
            let v = (hi * 16 + lo) as u8;
            if v == 0 {
                return None;
            }
            out.push(v);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Some(out)
}

pub fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !b.is_ascii_whitespace()).map(|i| i + 1).unwrap_or(start);
    &s[start..end.max(start)]
}

// ------------------------------------------------------------- responses

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        418 => "I'm a teapot",
        422 => "Unprocessable Content",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

pub fn status_line(status: u16, minor: u8) -> Vec<u8> {
    let mut h = Vec::with_capacity(512);
    h.extend_from_slice(if minor == 0 { b"HTTP/1.0 " } else { b"HTTP/1.1 " });
    h.extend_from_slice(status.to_string().as_bytes());
    h.push(b' ');
    h.extend_from_slice(reason(status).as_bytes());
    h.extend_from_slice(b"\r\n");
    h
}

thread_local! {
    static DATE: std::cell::RefCell<(u64, [u8; 29])> = const { std::cell::RefCell::new((0, [0; 29])) };
}

pub fn push_date(h: &mut Vec<u8>) {
    let now = std::time::SystemTime::now();
    let secs = now.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    DATE.with(|d| {
        let mut d = d.borrow_mut();
        if d.0 != secs {
            let s = httpdate::fmt_http_date(now);
            let b = s.as_bytes();
            if b.len() == 29 {
                d.1.copy_from_slice(b);
            }
            d.0 = secs;
        }
        h.extend_from_slice(b"Date: ");
        h.extend_from_slice(&d.1);
        h.extend_from_slice(b"\r\n");
    });
}

pub fn connection_header(h: &mut Vec<u8>, minor: u8, keepalive: bool) {
    if !keepalive {
        h.extend_from_slice(b"Connection: close\r\n");
    } else if minor == 0 {
        h.extend_from_slice(b"Connection: keep-alive\r\n");
    }
}

fn bodyless(status: u16) -> bool {
    status < 200 || status == 204 || status == 304
}

/// A response generated here: empty body.
pub async fn simple(stream: &mut TcpStream, status: u16, minor: u8, keepalive: bool, is_head: bool) -> std::io::Result<()> {
    write_response(stream, status, minor, keepalive, is_head, b"", b"").await
}

pub async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    minor: u8,
    keepalive: bool,
    is_head: bool,
    extra: &[u8],
    body: &[u8],
) -> std::io::Result<()> {
    let mut h = status_line(status, minor);
    push_date(&mut h);
    h.extend_from_slice(extra);
    if !bodyless(status) {
        h.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    connection_header(&mut h, minor, keepalive);
    h.extend_from_slice(b"\r\n");
    if !is_head && !bodyless(status) {
        h.extend_from_slice(body);
    }
    METRICS.http(status);
    stream.write_all(&h).await
}

async fn write_php(
    stream: &mut TcpStream,
    status: u16,
    minor: u8,
    keepalive: bool,
    is_head: bool,
    head: &[u8],
    body: &[u8],
) -> std::io::Result<()> {
    let mut h = status_line(status, minor);
    push_date(&mut h);
    h.extend_from_slice(head);
    if !bodyless(status) && !is_head {
        h.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    connection_header(&mut h, minor, keepalive);
    h.extend_from_slice(b"\r\n");
    METRICS.http(status);
    if is_head || bodyless(status) {
        return stream.write_all(&h).await;
    }
    if body.len() <= 64 << 10 {
        h.extend_from_slice(body);
        stream.write_all(&h).await
    } else {
        stream.write_all(&h).await?;
        stream.write_all(body).await
    }
}
