//! A request on its way to a PHP thread, and the response coming back.

use std::ffi::c_char;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use super::ffi::{HubRequest, HubStr};
use crate::config::Config;

pub const OWNER_NONE: usize = usize::MAX;
pub const OWNER_CANCELLED: usize = usize::MAX - 1;

/// Shared between the connection task and the PHP thread.
pub struct JobShared {
    /// PHP thread index once a thread took the job.
    pub owner: AtomicUsize,
}

pub enum Reply {
    Full { status: u16, head: Vec<u8>, php_len: Option<u64>, body: Vec<u8> },
    Stream { status: u16, head: Vec<u8>, php_len: Option<u64>, rx: mpsc::Receiver<Bytes> },
}

pub struct Job {
    pub seq: u64,
    /// Boxed: C keeps a pointer to it while the request is attached.
    pub creq: Box<HubRequest>,
    pub body: Bytes,
    pub received: Instant,
    pub shared: Arc<JobShared>,
    pub tx: Option<oneshot::Sender<Reply>>,
    head: Bytes,
    method: Range<usize>,
    path: Range<usize>,
    // Backing storage for the pointers in `creq`.
    _arena: Vec<u8>,
    _strs: Vec<HubStr>,
}

// The raw pointers in `creq` point into `head`, `_arena`, `_strs` and
// 'static config strings. Those heap buffers move with the Job.
unsafe impl Send for Job {}

impl Job {
    pub fn method(&self) -> &str {
        std::str::from_utf8(&self.head[self.method.clone()]).unwrap_or("?")
    }

    /// Path without the query string, for log lines.
    pub fn path(&self) -> String {
        String::from_utf8_lossy(&self.head[self.path.clone()]).into_owned()
    }

    /// Claim for a PHP thread. False when the connection already gave up.
    pub fn claim(&self, idx: usize) -> bool {
        self.shared.owner.compare_exchange(OWNER_NONE, idx, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }
}

/// The parsed request head, by ranges into `head`.
pub struct Parts<'a> {
    pub head: Bytes,
    pub method: Range<usize>,
    pub target: Range<usize>,
    pub path: Range<usize>,
    pub query: Option<Range<usize>>,
    pub minor: u8,
    pub headers: &'a [(Range<usize>, Range<usize>)],
    pub body: Bytes,
    pub remote: SocketAddr,
    pub local: SocketAddr,
}

enum Src {
    Arena(usize, usize),
    Head(usize, usize),
    Static(&'static [u8]),
}

pub fn build(p: Parts, cfg: &'static Config, seq: u64, tx: oneshot::Sender<Reply>, received: Instant) -> Job {
    let head = &p.head[..];
    let mut arena: Vec<u8> = Vec::with_capacity(256 + p.target.len() * 2);

    let method = push(&mut arena, &head[p.method.clone()]);
    let uri = push(&mut arena, &head[p.target.clone()]);
    let query = match &p.query {
        Some(q) => push(&mut arena, &head[q.clone()]),
        None => push(&mut arena, b""),
    };

    let mut content_type = None;
    let mut content_length = None;
    let mut authorization = None;
    let mut cookies: Vec<Range<usize>> = Vec::new();
    let mut host: Option<Range<usize>> = None;
    // Headers passed as HTTP_*: (name, value source)
    let mut pass: Vec<(Range<usize>, Src)> = Vec::with_capacity(p.headers.len());

    for (i, (n, v)) in p.headers.iter().enumerate() {
        let name = &head[n.clone()];
        if name.eq_ignore_ascii_case(b"content-type") {
            content_type = Some(v.clone());
            continue;
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            content_length = Some(v.clone());
            continue;
        }
        // CGI cannot tell "X_A" from "X-A"; drop underscores like nginx.
        // Proxy: is the httpoxy hole.
        if name.contains(&b'_') || name.eq_ignore_ascii_case(b"proxy") {
            continue;
        }
        if name.eq_ignore_ascii_case(b"cookie") {
            cookies.push(v.clone());
            continue;
        }
        if name.eq_ignore_ascii_case(b"authorization") {
            authorization = Some(v.clone());
        }
        if name.eq_ignore_ascii_case(b"host") && host.is_none() {
            host = Some(v.clone());
        }
        // Repeated header: join the values like CGI does.
        if p.headers[..i].iter().any(|(pn, _)| head[pn.clone()].eq_ignore_ascii_case(name)) {
            continue;
        }
        let dups: Vec<&Range<usize>> = p.headers[i + 1..]
            .iter()
            .filter(|(pn, _)| head[pn.clone()].eq_ignore_ascii_case(name))
            .map(|(_, pv)| pv)
            .collect();
        if dups.is_empty() {
            pass.push((n.clone(), Src::Head(v.start, v.len())));
        } else {
            let mut joined = head[v.clone()].to_vec();
            for d in dups {
                joined.extend_from_slice(b", ");
                joined.extend_from_slice(&head[d.clone()]);
            }
            let (o, l) = push(&mut arena, &joined);
            pass.push((n.clone(), Src::Arena(o, l)));
        }
    }

    let cookie = if cookies.is_empty() {
        None
    } else {
        let mut joined = Vec::new();
        for (i, c) in cookies.iter().enumerate() {
            if i > 0 {
                joined.extend_from_slice(b"; ");
            }
            joined.extend_from_slice(&head[c.clone()]);
        }
        Some(push(&mut arena, &joined))
    };
    let content_type = content_type.map(|r| push(&mut arena, &head[r]));
    let content_length_str = content_length.map(|r| push(&mut arena, &head[r]));
    let authorization = authorization.map(|r| push(&mut arena, &head[r]));

    let remote_addr = push(&mut arena, p.remote.ip().to_string().as_bytes());
    let remote_port = push(&mut arena, p.remote.port().to_string().as_bytes());
    let server_addr = push(&mut arena, p.local.ip().to_string().as_bytes());
    let server_port = push(&mut arena, p.local.port().to_string().as_bytes());
    let server_name = match &host {
        Some(h) => {
            let h = &head[h.clone()];
            // Strip the port, keeping [v6] literals intact.
            let end = if h.starts_with(b"[") {
                h.iter().position(|&b| b == b']').map(|i| i + 1).unwrap_or(h.len())
            } else {
                h.iter().position(|&b| b == b':').unwrap_or(h.len())
            };
            let (o, l) = push(&mut arena, &h[..end]);
            Src::Arena(o, l)
        }
        None => Src::Arena(server_addr.0, server_addr.1),
    };

    let protocol: &'static [u8] = if p.minor == 0 { b"HTTP/1.0" } else { b"HTTP/1.1" };

    let mut vars: Vec<(&'static [u8], Src)> = vec![
        (b"REQUEST_METHOD", Src::Arena(method.0, method.1)),
        (b"REQUEST_URI", Src::Arena(uri.0, uri.1)),
        (b"QUERY_STRING", Src::Arena(query.0, query.1)),
        (b"SCRIPT_NAME", Src::Static(cfg.script_name.as_bytes())),
        (b"SCRIPT_FILENAME", Src::Static(cfg.worker_script.as_bytes())),
        (b"PHP_SELF", Src::Static(cfg.script_name.as_bytes())),
        (b"DOCUMENT_URI", Src::Static(cfg.script_name.as_bytes())),
        (b"DOCUMENT_ROOT", Src::Static(cfg.server_root.as_bytes())),
        (b"SERVER_PROTOCOL", Src::Static(protocol)),
        (b"GATEWAY_INTERFACE", Src::Static(b"CGI/1.1")),
        (b"SERVER_SOFTWARE", Src::Static(b"PHPHub")),
        (b"REQUEST_SCHEME", Src::Static(b"http")),
        (b"REMOTE_ADDR", Src::Arena(remote_addr.0, remote_addr.1)),
        (b"REMOTE_PORT", Src::Arena(remote_port.0, remote_port.1)),
        (b"SERVER_ADDR", Src::Arena(server_addr.0, server_addr.1)),
        (b"SERVER_PORT", Src::Arena(server_port.0, server_port.1)),
        (b"SERVER_NAME", server_name),
    ];
    if let Some((o, l)) = content_type {
        vars.push((b"CONTENT_TYPE", Src::Arena(o, l)));
    }
    if let Some((o, l)) = content_length_str {
        vars.push((b"CONTENT_LENGTH", Src::Arena(o, l)));
    }

    // The arena is complete; turn offsets into pointers.
    let base = arena.as_ptr();
    let hb = head.as_ptr();
    let resolve = |s: &Src| -> HubStr {
        match *s {
            Src::Arena(o, l) => HubStr { ptr: (unsafe { base.add(o) }) as *const c_char, len: l },
            Src::Head(o, l) => HubStr { ptr: (unsafe { hb.add(o) }) as *const c_char, len: l },
            Src::Static(b) => HubStr { ptr: b.as_ptr() as *const c_char, len: b.len() },
        }
    };
    let cstr = |o: Option<(usize, usize)>| -> *const c_char {
        match o {
            Some((o, _)) => (unsafe { base.add(o) }) as *const c_char,
            None => std::ptr::null(),
        }
    };

    let mut strs: Vec<HubStr> = Vec::with_capacity((pass.len() + vars.len()) * 2);
    for (n, v) in &pass {
        strs.push(HubStr { ptr: (unsafe { hb.add(n.start) }) as *const c_char, len: n.len() });
        strs.push(resolve(v));
    }
    let num_headers = pass.len();
    for (k, v) in &vars {
        strs.push(HubStr { ptr: k.as_ptr() as *const c_char, len: k.len() });
        strs.push(resolve(v));
    }
    let num_vars = vars.len();

    let content_length: i64 = content_length_str
        .map(|(o, l)| std::str::from_utf8(&arena[o..o + l]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(-1))
        .unwrap_or(-1);

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let creq = Box::new(HubRequest {
        seq,
        method: cstr(Some(method)),
        uri: cstr(Some(uri)),
        query: cstr(Some(query)),
        content_type: cstr(content_type),
        content_length,
        cookie: cstr(cookie),
        authorization: cstr(authorization),
        headers: strs.as_ptr(),
        num_headers,
        vars: unsafe { strs.as_ptr().add(num_headers * 2) },
        num_vars,
        request_time: now,
        proto_num: if p.minor == 0 { 1000 } else { 1001 },
    });

    Job {
        seq,
        creq,
        body: p.body,
        received,
        shared: Arc::new(JobShared { owner: AtomicUsize::new(OWNER_NONE) }),
        tx: Some(tx),
        method: p.method,
        path: p.path,
        head: p.head,
        _arena: arena,
        _strs: strs,
    }
}

/// Copy NUL-terminated into the arena; returns (offset, length).
fn push(arena: &mut Vec<u8>, bytes: &[u8]) -> (usize, usize) {
    let off = arena.len();
    arena.extend_from_slice(bytes);
    arena.push(0);
    (off, bytes.len())
}
