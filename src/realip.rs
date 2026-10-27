//! Visitor address from a trusted proxy (docs/client-ip.md).
//!
//! `TRUSTED_PROXIES` is compiled into a table once at startup. Per request
//! there is at most one table match (the TCP peer) and one IP parse (the
//! header value); nothing here reads the environment or resolves names.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// Headers `REAL_IP_HEADER` may name, in their canonical spelling.
const HEADERS: [&str; 2] = ["X-Real-Ip", "CF-Connecting-IP"];

pub struct RealIp {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
    /// The canonical header name, compared case-insensitively.
    header: &'static str,
}

impl RealIp {
    /// `proxies`: `TRUSTED_PROXIES`, comma or whitespace separated.
    /// `header`: `REAL_IP_HEADER`, empty for the default.
    pub fn parse(proxies: &str, header: &str) -> Result<RealIp, String> {
        let header = header.trim();
        let header = if header.is_empty() {
            HEADERS[0]
        } else {
            HEADERS.iter().find(|h| h.eq_ignore_ascii_case(header)).copied().ok_or_else(|| {
                format!("REAL_IP_HEADER must be X-Real-Ip or CF-Connecting-IP: {header}")
            })?
        };
        let mut t = RealIp { v4: Vec::new(), v6: Vec::new(), header };
        for part in proxies.split(|c: char| c == ',' || c.is_ascii_whitespace()).filter(|s| !s.is_empty()) {
            let bad = || format!("TRUSTED_PROXIES has an invalid CIDR: {part}");
            let (addr, bits) = match part.split_once('/') {
                Some((a, b)) => {
                    if b.is_empty() || !b.bytes().all(|c| c.is_ascii_digit()) {
                        return Err(bad());
                    }
                    (a, Some(b.parse::<u32>().map_err(|_| bad())?))
                }
                None => (part, None),
            };
            match addr.parse::<IpAddr>().map_err(|_| bad())? {
                IpAddr::V4(a) => {
                    let bits = bits.unwrap_or(32);
                    if bits > 32 {
                        return Err(bad());
                    }
                    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
                    t.v4.push((u32::from(a) & mask, mask));
                }
                IpAddr::V6(a) => {
                    let bits = bits.unwrap_or(128);
                    if bits > 128 {
                        return Err(bad());
                    }
                    let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
                    t.v6.push((u128::from(a) & mask, mask));
                }
            }
        }
        Ok(t)
    }

    /// No proxy is trusted: REMOTE_ADDR is always the TCP peer.
    pub fn disabled(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }

    pub fn header(&self) -> &'static str {
        self.header
    }

    pub fn trusts(&self, peer: IpAddr) -> bool {
        match peer.to_canonical() {
            IpAddr::V4(a) => {
                let a = u32::from(a);
                self.v4.iter().any(|&(net, mask)| a & mask == net)
            }
            IpAddr::V6(a) => {
                let a = u128::from(a);
                self.v6.iter().any(|&(net, mask)| a & mask == net)
            }
        }
    }

    /// Short summary for the startup line.
    pub fn describe(&self) -> String {
        let n = self.v4.len() + self.v6.len();
        format!("{n} network{}", if n == 1 { "" } else { "s" })
    }
}

/// The header value as a single IP, or None: empty, a list, a port,
/// brackets or anything else that is not exactly one address.
pub fn single_ip(value: &[u8]) -> Option<IpAddr> {
    let s = std::str::from_utf8(value).ok()?.trim_matches(|c| c == ' ' || c == '\t');
    if s.is_empty() || s.len() > 45 {
        return None;
    }
    s.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

static WARNED: AtomicBool = AtomicBool::new(false);

/// A trusted peer sent a header that is not a single IP. Logged once per
/// process so a broken client cannot fill the error log.
pub fn warn_invalid(header: &str, value: &[u8]) {
    if !WARNED.swap(true, Relaxed) {
        let v: String = String::from_utf8_lossy(&value[..value.len().min(80)]).escape_default().collect();
        crate::logln!(
            "warning: {header} from a trusted proxy is not a single IP: \"{v}\"; REMOTE_ADDR stays the TCP peer (logged once)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn empty_trusts_nothing() {
        let t = RealIp::parse("", "").unwrap();
        assert!(t.disabled());
        assert!(!t.trusts(ip("10.0.0.1")));
        assert_eq!(t.header(), "X-Real-Ip");
        assert!(RealIp::parse(" ,  ,", "").unwrap().disabled());
    }

    #[test]
    fn cidrs() {
        let t = RealIp::parse("10.0.0.0/8, 172.16.0.0/12\n192.168.1.7 fd00::/8", "").unwrap();
        assert!(!t.disabled());
        assert!(t.trusts(ip("10.255.1.2")));
        assert!(t.trusts(ip("172.31.0.1")));
        assert!(!t.trusts(ip("172.32.0.1")));
        assert!(t.trusts(ip("192.168.1.7")));
        assert!(!t.trusts(ip("192.168.1.8")));
        assert!(t.trusts(ip("fd12::1")));
        assert!(!t.trusts(ip("fe80::1")));
        // IPv4-mapped peers match the IPv4 table.
        assert!(t.trusts(ip("::ffff:10.1.2.3")));
        // Host bits in the network are ignored, as in nginx.
        assert!(RealIp::parse("10.1.2.3/8", "").unwrap().trusts(ip("10.9.9.9")));
        assert!(RealIp::parse("0.0.0.0/0", "").unwrap().trusts(ip("8.8.8.8")));
        assert!(!RealIp::parse("0.0.0.0/0", "").unwrap().trusts(ip("::1")));
        assert!(RealIp::parse("::/0", "").unwrap().trusts(ip("::1")));
    }

    #[test]
    fn bad_cidrs_name_the_entry() {
        for bad in ["10.0.0.0/33", "10.0.0/8", "10.0.0.0/", "10.0.0.0/x", "fd00::/129", "example.com", "10.0.0.0/+8"] {
            let e = RealIp::parse(&format!("192.168.0.0/16,{bad}"), "").err().unwrap();
            assert!(e.ends_with(bad), "{e}");
        }
    }

    #[test]
    fn headers() {
        assert_eq!(RealIp::parse("", "x-real-ip").unwrap().header(), "X-Real-Ip");
        assert_eq!(RealIp::parse("", " cf-connecting-ip ").unwrap().header(), "CF-Connecting-IP");
        assert!(RealIp::parse("", "X-Forwarded-For").is_err());
        assert!(RealIp::parse("", "True-Client-IP").is_err());
    }

    #[test]
    fn values() {
        assert_eq!(single_ip(b"1.2.3.4"), Some(ip("1.2.3.4")));
        assert_eq!(single_ip(b" 1.2.3.4\t"), Some(ip("1.2.3.4")));
        assert_eq!(single_ip(b"2001:db8::1"), Some(ip("2001:db8::1")));
        assert_eq!(single_ip(b"::ffff:1.2.3.4"), Some(ip("1.2.3.4")));
        for bad in [&b""[..], b" ", b"1.2.3.4, 5.6.7.8", b"1.2.3.4:80", b"[2001:db8::1]", b"[2001:db8::1]:80",
            b"fe80::1%eth0", b"unknown", b"1.2.3", b"01.2.3.4", b"1.2.3.4\r\n"]
        {
            assert_eq!(single_ip(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }
}
