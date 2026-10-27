//! Settings from the environment. Bad values stop the process at startup.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The repository is always mounted here; nothing outside it is served.
pub const APP_BASE: &str = "/var/www/html";

/// `PHPHUB_MODE`, fixed at startup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunMode {
    /// Unset: a script that calls phphub_handle_request() is a worker,
    /// anything else runs per request.
    Auto,
    Worker,
    /// The entry script runs once per request.
    Index,
    /// The requested .php file runs if it exists, otherwise the entry.
    File,
}

impl RunMode {
    pub fn parse(v: Option<&str>) -> Result<RunMode, String> {
        match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") => Ok(RunMode::Auto),
            Some("worker") => Ok(RunMode::Worker),
            Some("index") => Ok(RunMode::Index),
            Some("file") => Ok(RunMode::File),
            Some(_) => Err("PHPHUB_MODE must be worker, index, or file".into()),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RunMode::Auto => "auto",
            RunMode::Worker => "worker",
            RunMode::Index => "index",
            RunMode::File => "file",
        }
    }
}

pub struct Config {
    pub mode: RunMode,
    pub listen_port: u16,
    pub metrics_port: u16,
    pub server_root: String,
    pub worker_script: String,
    /// NUL-terminated copy for C.
    pub worker_script_c: std::ffi::CString,
    /// SERVER_ROOT with symlinks resolved, if it exists at startup.
    pub server_root_real: Option<PathBuf>,
    /// `SCRIPT_NAME` / `PHP_SELF` for requests that go to PHP.
    pub script_name: String,
    pub workers: usize,
    pub body_limit: u64,
    pub max_requests: u64,
    pub php_timeout: Duration,
    pub display_errors: bool,
    pub slowlog: Option<Duration>,
    /// TRUSTED_PROXIES and REAL_IP_HEADER, compiled once.
    pub real_ip: crate::realip::RealIp,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let mode = RunMode::parse(std::env::var("PHPHUB_MODE").ok().as_deref())?;
        let server_root = env_or("SERVER_ROOT", APP_BASE);
        check_app_path("SERVER_ROOT", &server_root)?;
        let server_root = server_root.trim_end_matches('/').to_string();
        let server_root = if server_root.is_empty() { "/".into() } else { server_root };
        if !(server_root == APP_BASE || server_root.starts_with(&format!("{APP_BASE}/"))) {
            return Err(format!("SERVER_ROOT must be inside {APP_BASE}: {server_root}"));
        }

        let script = match std::env::var("WORKER_SCRIPT") {
            Ok(s) if !s.is_empty() => {
                if s.starts_with('/') {
                    s
                } else {
                    format!("{server_root}/{s}")
                }
            }
            _ => format!("{server_root}/index.php"),
        };
        check_app_path("WORKER_SCRIPT", &script)?;
        let script = resolve_inside_base("WORKER_SCRIPT", &script)?;

        let script_name = match script.strip_prefix(&format!("{server_root}/")) {
            Some(rel) => format!("/{rel}"),
            None => format!(
                "/{}",
                Path::new(&script).file_name().and_then(|n| n.to_str()).unwrap_or("index.php")
            ),
        };

        let workers = match std::env::var("WORKERS") {
            Ok(v) if !v.is_empty() => {
                let n: usize = v.parse().map_err(|_| format!("WORKERS is not a number: {v}"))?;
                if n == 0 {
                    return Err("WORKERS must be at least 1".into());
                }
                n
            }
            _ => default_workers(),
        };

        let body_limit = parse_size(&env_or("BODY_LIMIT", "32m"))
            .ok_or_else(|| "BODY_LIMIT must look like 32m, 512k or 1048576".to_string())?;
        let max_requests = parse_u64("MAX_REQUESTS", 1000)?;
        let timeout = env_or("PHP_TIMEOUT", "60");
        let timeout: u64 = match timeout.trim().parse() {
            Ok(n) if n >= 1 => n,
            _ => return Err(format!("PHP_TIMEOUT must be a whole number of seconds, at least 1: {timeout}")),
        };
        let slowlog = env_or("SLOWLOG", "0");
        let slowlog: f64 = slowlog.parse().map_err(|_| format!("SLOWLOG is not a number of seconds: {slowlog}"))?;
        let metrics_port = parse_u64("METRICS_PORT", 9100)?;
        let listen_port = parse_u64("PHPHUB_PORT", 80)?;

        let real_ip = crate::realip::RealIp::parse(
            &std::env::var("TRUSTED_PROXIES").unwrap_or_default(),
            &std::env::var("REAL_IP_HEADER").unwrap_or_default(),
        )?;

        let server_root_real = std::fs::canonicalize(&server_root).ok();
        Ok(Config {
            mode,
            listen_port: port(listen_port, "PHPHUB_PORT")?,
            metrics_port: port(metrics_port, "METRICS_PORT")?,
            server_root,
            worker_script_c: std::ffi::CString::new(script.clone()).map_err(|_| "WORKER_SCRIPT has a NUL byte")?,
            server_root_real,
            worker_script: script,
            script_name,
            workers,
            body_limit,
            max_requests,
            php_timeout: Duration::from_secs(timeout),
            display_errors: env_or("DISPLAY_ERRORS", "0") == "1",
            slowlog: if slowlog > 0.0 { Some(Duration::from_secs_f64(slowlog)) } else { None },
            real_ip,
        })
    }

    /// php.ini settings PHPHub owns. They override php.ini and conf.d.
    pub fn ini_entries(&self) -> String {
        let d = if self.display_errors { 1 } else { 0 };
        format!(
            "expose_php=0\n\
             display_errors={d}\n\
             display_startup_errors={d}\n\
             zlib.output_compression=0\n\
             post_max_size={limit}\n\
             upload_max_filesize={limit}\n",
            limit = self.body_limit
        )
    }

    /// Defaults that php.ini and conf.d may still change.
    pub fn ini_defaults(&self) -> String {
        "log_errors=1\n\
         html_errors=0\n\
         max_execution_time=0\n\
         opcache.enable=1\n\
         opcache.validate_timestamps=1\n\
         opcache.revalidate_freq=2\n"
            .to_string()
    }
}

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

fn parse_u64(key: &str, default: u64) -> Result<u64, String> {
    let v = env_or(key, &default.to_string());
    v.parse().map_err(|_| format!("{key} is not a number: {v}"))
}

fn port(v: u64, key: &str) -> Result<u16, String> {
    if v == 0 || v > 65535 {
        return Err(format!("{key} is not a port: {v}"));
    }
    Ok(v as u16)
}

/// Absolute, only `[A-Za-z0-9/_.-]`, no `..`.
fn check_app_path(key: &str, p: &str) -> Result<(), String> {
    if !p.starts_with('/') {
        return Err(format!("{key} must be an absolute path: {p}"));
    }
    if !p.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b)) {
        return Err(format!("{key} may only contain letters, digits, '/', '_', '.' and '-': {p}"));
    }
    if p.split('/').any(|seg| seg == "..") || p.contains("..") {
        return Err(format!("{key} must not contain '..': {p}"));
    }
    if !(p == APP_BASE || p.starts_with(&format!("{APP_BASE}/"))) {
        return Err(format!("{key} must be inside {APP_BASE}: {p}"));
    }
    Ok(())
}

/// Follow symlinks when the file exists and check it still lands inside the
/// mount. A missing script is allowed: the mount may be filled later.
fn resolve_inside_base(key: &str, p: &str) -> Result<String, String> {
    match std::fs::canonicalize(p) {
        Ok(real) => {
            let base = std::fs::canonicalize(APP_BASE).unwrap_or_else(|_| PathBuf::from(APP_BASE));
            if !real.starts_with(&base) {
                return Err(format!("{key} resolves outside {APP_BASE}: {}", real.display()));
            }
            Ok(p.to_string())
        }
        Err(_) => Ok(p.to_string()),
    }
}

/// Threads follow the cgroup CPU quota, rounded up, at least 2.
fn default_workers() -> usize {
    let cpus = cgroup_cpus().unwrap_or_else(|| {
        std::thread::available_parallelism().map(|n| n.get() as f64).unwrap_or(2.0)
    });
    (cpus.ceil() as usize).max(2)
}

pub fn cgroup_cpus() -> Option<f64> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let mut it = s.split_whitespace();
        let quota = it.next()?;
        let period: f64 = it.next()?.parse().ok()?;
        if quota == "max" || period <= 0.0 {
            return None;
        }
        return Some(quota.parse::<f64>().ok()? / period);
    }
    let quota: f64 = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").ok()?.trim().parse().ok()?;
    let period: f64 = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us").ok()?.trim().parse().ok()?;
    if quota <= 0.0 || period <= 0.0 {
        return None;
    }
    Some(quota / period)
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 1024),
        b'm' | b'M' => (&s[..s.len() - 1], 1024 * 1024),
        b'g' | b'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: u64 = num.trim().parse().ok()?;
    n.checked_mul(mul).filter(|&v| v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("32m"), Some(32 << 20));
        assert_eq!(parse_size("512K"), Some(512 << 10));
        assert_eq!(parse_size("1g"), Some(1 << 30));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("x"), None);
        assert_eq!(parse_size("0"), None);
    }

    #[test]
    fn modes() {
        assert_eq!(RunMode::parse(None), Ok(RunMode::Auto));
        assert_eq!(RunMode::parse(Some("")), Ok(RunMode::Auto));
        assert_eq!(RunMode::parse(Some("  ")), Ok(RunMode::Auto));
        assert_eq!(RunMode::parse(Some(" Worker ")), Ok(RunMode::Worker));
        assert_eq!(RunMode::parse(Some("INDEX")), Ok(RunMode::Index));
        assert_eq!(RunMode::parse(Some("file\n")), Ok(RunMode::File));
        assert_eq!(
            RunMode::parse(Some("fpm")),
            Err("PHPHUB_MODE must be worker, index, or file".to_string())
        );
    }

    #[test]
    fn app_paths() {
        assert!(check_app_path("X", "/var/www/html").is_ok());
        assert!(check_app_path("X", "/var/www/html/frontend/public").is_ok());
        assert!(check_app_path("X", "/var/www/html/../etc").is_err());
        assert!(check_app_path("X", "/var/www/htmlx").is_err());
        assert!(check_app_path("X", "/etc").is_err());
        assert!(check_app_path("X", "var/www/html").is_err());
        assert!(check_app_path("X", "/var/www/html/a b").is_err());
        assert!(check_app_path("X", "/var/www/html/a$b").is_err());
    }
}
