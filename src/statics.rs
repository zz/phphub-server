//! Files under the document root that PHP never sees.

use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::TcpStream;

use crate::http::{Req, connection_header, push_date, status_line, trim};
use crate::metrics::METRICS;

pub enum Lookup {
    File(PathBuf, Metadata),
    /// A directory with index.html, asked for without the trailing slash.
    AddSlash,
    /// Run the entry script.
    Php,
    /// File mode: run this PHP file under the document root.
    PhpFile(PhpFile),
}

pub struct PhpFile {
    /// Absolute path, as asked for (not the symlink target).
    pub filename: Vec<u8>,
    /// Site path of the script, e.g. `/info3.php`.
    pub name: Vec<u8>,
    /// What follows the script in the path, e.g. `/extra`; may be empty.
    pub path_info: Vec<u8>,
}

/// `decoded` is the percent-decoded path; dot segments were rejected earlier.
/// `file_mode` lets existing PHP files under the root be picked as the script;
/// `real_root` is the root with symlinks resolved, when known.
pub fn lookup(root: &str, decoded: &[u8], file_mode: bool, real_root: Option<&Path>) -> Lookup {
    let mut full = Vec::with_capacity(root.len() + decoded.len() + 12);
    full.extend_from_slice(root.as_bytes());
    full.extend_from_slice(decoded);
    let path = Path::new(OsStr::from_bytes(&full));

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            if file_mode {
                if let Some(f) = split_path_info(root, decoded, real_root) {
                    return Lookup::PhpFile(f);
                }
            }
            return Lookup::Php;
        }
    };
    if meta.is_file() {
        if is_php(decoded) {
            if file_mode && inside_root(path, root, real_root) {
                return Lookup::PhpFile(PhpFile { filename: full, name: decoded.to_vec(), path_info: Vec::new() });
            }
            return Lookup::Php;
        }
        return Lookup::File(path.to_path_buf(), meta);
    }
    if meta.is_dir() {
        let index = path.join("index.html");
        if let Ok(m) = std::fs::metadata(&index) {
            if m.is_file() {
                if !decoded.ends_with(b"/") {
                    return Lookup::AddSlash;
                }
                return Lookup::File(index, m);
            }
        }
    }
    Lookup::Php
}

/// `/info3.php/extra` where `/info3.php` is a file: script plus PATH_INFO.
/// Only tried when the full path does not exist, and only at segments that
/// end in a PHP extension, so pretty URLs cost no extra stat.
fn split_path_info(root: &str, decoded: &[u8], real_root: Option<&Path>) -> Option<PhpFile> {
    let mut seg_start = 1;
    for (i, &b) in decoded.iter().enumerate().skip(1) {
        if b != b'/' {
            continue;
        }
        let seg = &decoded[seg_start..i];
        seg_start = i + 1;
        if !is_php(seg) {
            continue;
        }
        let mut full = Vec::with_capacity(root.len() + i);
        full.extend_from_slice(root.as_bytes());
        full.extend_from_slice(&decoded[..i]);
        let path = Path::new(OsStr::from_bytes(&full));
        match std::fs::metadata(path) {
            Ok(m) if m.is_file() => {
                if !inside_root(path, root, real_root) {
                    return None;
                }
                return Some(PhpFile { name: decoded[..i].to_vec(), path_info: decoded[i..].to_vec(), filename: full });
            }
            Ok(m) if m.is_dir() => continue,
            _ => return None,
        }
    }
    None
}

/// The real path, symlinks resolved, must stay under the document root.
fn inside_root(path: &Path, root: &str, real_root: Option<&Path>) -> bool {
    let Ok(real) = std::fs::canonicalize(path) else { return false };
    match real_root {
        Some(r) => real.starts_with(r),
        None => std::fs::canonicalize(root).is_ok_and(|r| real.starts_with(r)),
    }
}

/// PHP source is never sent as a file.
fn is_php(path: &[u8]) -> bool {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    match name.iter().rposition(|&b| b == b'.') {
        Some(dot) => {
            let ext = &name[dot + 1..];
            [&b"php"[..], b"phtml", b"phar", b"inc"].iter().any(|e| ext.eq_ignore_ascii_case(e))
        }
        None => false,
    }
}

pub fn content_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "xml" => "application/xml",
        "txt" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "apng" => "image/apng",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn mtime(meta: &Metadata) -> SystemTime {
    meta.modified().unwrap_or(UNIX_EPOCH)
}

fn etag(meta: &Metadata) -> String {
    let secs = mtime(meta).duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("\"{secs:x}-{:x}\"", meta.len())
}

enum Range {
    Full,
    Part(u64, u64),
    Unsatisfiable,
}

fn parse_range(v: &[u8], size: u64) -> Range {
    let Ok(s) = std::str::from_utf8(trim(v)) else { return Range::Full };
    let Some(spec) = s.strip_prefix("bytes=") else { return Range::Full };
    if spec.contains(',') {
        return Range::Full; // multiple ranges: send the whole file
    }
    let Some((a, b)) = spec.trim().split_once('-') else { return Range::Full };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let Ok(n) = b.parse::<u64>() else { return Range::Full };
        if n == 0 || size == 0 {
            return Range::Unsatisfiable;
        }
        let n = n.min(size);
        return Range::Part(size - n, size - 1);
    }
    let Ok(start) = a.parse::<u64>() else { return Range::Full };
    if start >= size {
        return Range::Unsatisfiable;
    }
    let end = if b.is_empty() {
        size - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) if e >= start => e.min(size - 1),
            _ => return Range::Full,
        }
    };
    Range::Part(start, end)
}

pub async fn serve(stream: &mut TcpStream, req: &Req, path: &Path, meta: &Metadata, keepalive: bool) -> std::io::Result<()> {
    let minor = req.minor;
    let is_head = req.method() == b"HEAD";
    let size = meta.len();
    let tag = etag(meta);
    let modified = mtime(meta);
    let last_modified = httpdate::fmt_http_date(modified);

    let not_modified = match req.header("if-none-match") {
        Some(v) => {
            let v = trim(v);
            v == b"*" || v.split(|&b| b == b',').any(|t| {
                let t = trim(t);
                let t = t.strip_prefix(b"W/").unwrap_or(t);
                t == tag.as_bytes()
            })
        }
        None => req
            .header("if-modified-since")
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|v| httpdate::parse_http_date(v.trim()).ok())
            .is_some_and(|since| {
                let m = modified.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let s = since.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs();
                m <= s
            }),
    };

    let mut h = Vec::with_capacity(320);
    let common = |h: &mut Vec<u8>| {
        push_date(h);
        h.extend_from_slice(b"Last-Modified: ");
        h.extend_from_slice(last_modified.as_bytes());
        h.extend_from_slice(b"\r\nETag: ");
        h.extend_from_slice(tag.as_bytes());
        h.extend_from_slice(b"\r\n");
    };

    if not_modified {
        h.extend_from_slice(&status_line(304, minor));
        common(&mut h);
        connection_header(&mut h, minor, keepalive);
        h.extend_from_slice(b"\r\n");
        METRICS.http(304);
        return write_all(stream, &h, false).await;
    }

    let range = match (req.header("range"), req.header("if-range")) {
        (Some(r), None) => parse_range(r, size),
        _ => Range::Full,
    };
    let (status, start, len) = match range {
        Range::Full => (200, 0, size),
        Range::Part(a, b) => (206, a, b - a + 1),
        Range::Unsatisfiable => {
            h.extend_from_slice(&status_line(416, minor));
            push_date(&mut h);
            h.extend_from_slice(format!("Content-Range: bytes */{size}\r\nContent-Length: 0\r\n").as_bytes());
            connection_header(&mut h, minor, keepalive);
            h.extend_from_slice(b"\r\n");
            METRICS.http(416);
            return write_all(stream, &h, false).await;
        }
    };

    let file = File::open(path)?;

    h.extend_from_slice(&status_line(status, minor));
    common(&mut h);
    h.extend_from_slice(b"Content-Type: ");
    h.extend_from_slice(content_type(path).as_bytes());
    h.extend_from_slice(b"\r\nAccept-Ranges: bytes\r\n");
    if status == 206 {
        h.extend_from_slice(format!("Content-Range: bytes {start}-{}/{size}\r\n", start + len - 1).as_bytes());
    }
    h.extend_from_slice(format!("Content-Length: {len}\r\n").as_bytes());
    connection_header(&mut h, minor, keepalive);
    h.extend_from_slice(b"\r\n");
    METRICS.http(status);

    if is_head || len == 0 {
        return write_all(stream, &h, false).await;
    }
    write_all(stream, &h, true).await?;
    send_file(stream, &file, start, len).await
}

/// `more` asks the kernel to hold the packet for the file data that follows.
#[cfg(target_os = "linux")]
async fn write_all(stream: &mut TcpStream, mut data: &[u8], more: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use tokio::io::Interest;
    let fd = stream.as_raw_fd();
    let flags = libc::MSG_NOSIGNAL | if more { libc::MSG_MORE } else { 0 };
    while !data.is_empty() {
        stream.writable().await?;
        match stream.try_io(Interest::WRITABLE, || {
            let n = unsafe { libc::send(fd, data.as_ptr() as *const libc::c_void, data.len(), flags) };
            if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
        }) {
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn send_file(stream: &mut TcpStream, file: &File, start: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use tokio::io::Interest;
    let out = stream.as_raw_fd();
    let inp = file.as_raw_fd();
    let mut off = start as libc::off_t;
    let mut left = len;
    while left > 0 {
        stream.writable().await?;
        let want = left.min(1 << 30) as usize;
        match stream.try_io(Interest::WRITABLE, || {
            let n = unsafe { libc::sendfile(out, inp, &mut off, want) };
            if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
        }) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => left -= n as u64,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn write_all(stream: &mut TcpStream, data: &[u8], _more: bool) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    stream.write_all(data).await
}

#[cfg(not(target_os = "linux"))]
async fn send_file(stream: &mut TcpStream, file: &File, start: u64, len: u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    use tokio::io::AsyncWriteExt;
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(start))?;
    let mut left = len;
    let mut buf = vec![0u8; 64 << 10];
    while left > 0 {
        let n = f.read(&mut buf[..left.min(buf.len() as u64) as usize])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        stream.write_all(&buf[..n]).await?;
        left -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_never_static() {
        assert!(is_php(b"/index.php"));
        assert!(is_php(b"/a/b.PHP"));
        assert!(is_php(b"/x.phtml"));
        assert!(!is_php(b"/style.css"));
        assert!(!is_php(b"/php"));
    }

    #[test]
    fn file_mode_lookup() {
        let dir = std::env::temp_dir().join(format!("phphub-lookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("info3.php"), "<?php").unwrap();
        std::fs::write(root.join("sub/a.php"), "<?php").unwrap();
        std::fs::write(root.join("style.css"), "x").unwrap();
        std::fs::write(dir.join("outside.php"), "<?php").unwrap();
        std::os::unix::fs::symlink(dir.join("outside.php"), root.join("link.php")).unwrap();
        let r = root.to_str().unwrap();
        let real = std::fs::canonicalize(&root).unwrap();
        let rr = Some(real.as_path());

        match lookup(r, b"/info3.php", true, rr) {
            Lookup::PhpFile(f) => {
                assert_eq!(f.name, b"/info3.php");
                assert!(f.path_info.is_empty());
                assert!(f.filename.ends_with(b"/root/info3.php"));
            }
            _ => panic!("info3.php should run"),
        }
        match lookup(r, b"/sub/a.php/x/y", true, rr) {
            Lookup::PhpFile(f) => {
                assert_eq!(f.name, b"/sub/a.php");
                assert_eq!(f.path_info, b"/x/y");
            }
            _ => panic!("PATH_INFO split"),
        }
        assert!(matches!(lookup(r, b"/info3.php", false, rr), Lookup::Php));
        assert!(matches!(lookup(r, b"/missing.php", true, rr), Lookup::Php));
        assert!(matches!(lookup(r, b"/album/123", true, rr), Lookup::Php));
        assert!(matches!(lookup(r, b"/sub/", true, rr), Lookup::Php));
        assert!(matches!(lookup(r, b"/link.php", true, rr), Lookup::Php));
        assert!(matches!(lookup(r, b"/style.css", true, rr), Lookup::File(..)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ranges() {
        assert!(matches!(parse_range(b"bytes=0-9", 100), Range::Part(0, 9)));
        assert!(matches!(parse_range(b"bytes=90-", 100), Range::Part(90, 99)));
        assert!(matches!(parse_range(b"bytes=-10", 100), Range::Part(90, 99)));
        assert!(matches!(parse_range(b"bytes=100-", 100), Range::Unsatisfiable));
        assert!(matches!(parse_range(b"bytes=0-1,5-6", 100), Range::Full));
    }
}
