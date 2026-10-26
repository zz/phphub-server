//! Everything goes to standard error, one line per entry.

use std::io::Write;

/// Write one line with a single write call so lines from different threads
/// never interleave.
pub fn line(msg: &str) {
    let mut buf = Vec::with_capacity(msg.len() + 1);
    buf.extend_from_slice(msg.trim_end_matches('\n').as_bytes());
    buf.push(b'\n');
    let _ = std::io::stderr().lock().write_all(&buf);
}

#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {
        $crate::log::line(&format!("phphub: {}", format_args!($($arg)*)))
    };
}
