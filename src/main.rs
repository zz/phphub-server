//! PHPHub: HTTP/1.1 server and embedded ZTS PHP in one process.

mod config;
mod http;
mod init;
mod log;
mod metrics;
mod php;
mod realip;
mod statics;

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering::*};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use config::Config;
use http::Server;
use php::Pool;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Without `docker run --init`, PHPHub is PID 1 and must reap orphans.
    // Only the server says so; other commands keep their output clean.
    init::become_init_if_pid1(args.len() > 1);
    if args.get(1).map(String::as_str) == Some("health") {
        std::process::exit(health_check());
    }
    if args.len() > 1 {
        // ENTRYPOINT passthrough: `docker run <image> crond -f` or
        // `... php bin/console` runs that command instead of the server.
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&args[1]).args(&args[2..]).exec();
        eprintln!("phphub: cannot run {}: {err}", args[1]);
        std::process::exit(if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 });
    }

    let cfg: &'static Config = match Config::from_env() {
        Ok(c) => Box::leak(Box::new(c)),
        Err(e) => {
            logln!("refusing to start: {e}");
            std::process::exit(1);
        }
    };
    if !std::path::Path::new(&cfg.worker_script).is_file() {
        logln!("warning: {} does not exist yet", cfg.worker_script);
    }

    let pool: &'static Pool = Box::leak(Box::new(Pool::new(cfg)));
    // PHP starts on this thread and is shut down on it again at the end.
    let handles = match pool.start() {
        Ok(h) => h,
        Err(e) => {
            logln!("{e}");
            std::process::exit(1);
        }
    };

    let io_threads = config::cgroup_cpus()
        .map(|c| c.ceil() as usize)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
        .clamp(1, 64);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(io_threads)
        .thread_name("phphub-io")
        .enable_all()
        .build()
        .expect("tokio runtime");

    let clean = rt.block_on(serve(cfg, pool));
    drop(rt);

    // Let the worker loops see phphub_handle_request() return false.
    pool.shutdown();
    let deadline = Instant::now() + Duration::from_secs(5);
    while pool.alive_threads() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if pool.alive_threads() > 0 || !clean {
        logln!("{} PHP threads still running; exiting without PHP shutdown", pool.alive_threads());
        std::process::exit(0);
    }
    for h in handles {
        let _ = h.join();
    }
    php::php_shutdown();
    logln!("stopped");
}

/// Serve until SIGTERM/SIGINT, then drain. Returns false if requests were
/// still running when PHP_TIMEOUT ran out.
async fn serve(cfg: &'static Config, pool: &'static Pool) -> bool {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let srv: &'static Server = Box::leak(Box::new(Server {
        cfg,
        pool,
        shutdown: shutdown_rx.clone(),
        inflight: AtomicUsize::new(0),
    }));

    let addr: SocketAddr = ([0, 0, 0, 0], cfg.listen_port).into();
    let listener = match bind(addr) {
        Ok(l) => l,
        Err(e) => {
            logln!("cannot listen on {addr}: {e}");
            std::process::exit(1);
        }
    };
    let maddr: SocketAddr = ([0, 0, 0, 0], cfg.metrics_port).into();
    let mlistener = match bind(maddr) {
        Ok(l) => l,
        Err(e) => {
            logln!("cannot listen on {maddr}: {e}");
            std::process::exit(1);
        }
    };
    logln!(
        "listening on :{} (metrics :{}), mode={}, {} PHP threads, root={}, script={}",
        cfg.listen_port,
        cfg.metrics_port,
        cfg.mode.name(),
        cfg.workers,
        cfg.server_root,
        cfg.worker_script
    );

    if !cfg.real_ip.disabled() {
        logln!("REMOTE_ADDR comes from {} for TRUSTED_PROXIES ({})", cfg.real_ip.header(), cfg.real_ip.describe());
    }

    tokio::spawn(metrics_server(mlistener, pool));

    let mut sd = shutdown_rx.clone();
    let accept = tokio::spawn(async move {
        loop {
            tokio::select! {
                r = listener.accept() => match r {
                    Ok((stream, remote)) => {
                        let local = stream.local_addr().unwrap_or(addr);
                        tokio::spawn(http::serve_conn(stream, remote, local, srv));
                    }
                    Err(e) => {
                        // EMFILE and friends: back off instead of spinning.
                        logln!("accept: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                },
                _ = sd.changed() => break,
            }
        }
        // Dropping the listener stops new connections.
    });

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).expect("SIGINT");
    tokio::select! {
        _ = term.recv() => {},
        _ = int.recv() => {},
    }
    logln!("shutting down: no new connections, waiting up to {}s for {} requests", cfg.php_timeout.as_secs(), srv.inflight.load(Acquire));
    let _ = shutdown_tx.send(true);
    let _ = accept.await;

    let deadline = Instant::now() + cfg.php_timeout;
    while srv.inflight.load(Acquire) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    srv.inflight.load(Acquire) == 0
}

fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let sock = tokio::net::TcpSocket::new_v4()?;
    sock.set_reuseaddr(true)?;
    sock.bind(addr)?;
    sock.listen(4096)
}

// ------------------------------------------------------ metrics and health

async fn metrics_server(listener: TcpListener, pool: &'static Pool) {
    loop {
        if let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(metrics_conn(stream, pool));
        }
    }
}

async fn metrics_conn(mut stream: TcpStream, pool: &'static Pool) {
    let mut buf = BytesMut::with_capacity(4096);
    loop {
        let (path, keepalive, n) = loop {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);
            match req.parse(&buf) {
                Ok(httparse::Status::Complete(n)) => {
                    let path = req.path.unwrap_or("/").split('?').next().unwrap_or("/").to_string();
                    let close = req.headers.iter().any(|h| {
                        h.name.eq_ignore_ascii_case("connection") && h.value.eq_ignore_ascii_case(b"close")
                    });
                    break (path, req.version == Some(1) && !close, n);
                }
                Ok(httparse::Status::Partial) if buf.len() < 16 << 10 => {}
                _ => return,
            }
            match tokio::time::timeout(Duration::from_secs(30), stream.read_buf(&mut buf)).await {
                Ok(Ok(r)) if r > 0 => {}
                _ => return,
            }
        };
        let _ = buf.split_to(n);

        let (status, ctype, body) = match path.as_str() {
            "/health" => {
                if pool.healthy() {
                    (200, "text/plain; charset=utf-8", "ok\n".to_string())
                } else {
                    (503, "text/plain; charset=utf-8", "unavailable\n".to_string())
                }
            }
            "/metrics" => (
                200,
                "text/plain; version=0.0.4; charset=utf-8",
                metrics::render(pool.script(), &pool.views(), pool.mode() != php::Mode::Classic),
            ),
            _ => (404, "text/plain; charset=utf-8", "not found\n".to_string()),
        };
        let mut out = http::status_line(status, 1);
        http::push_date(&mut out);
        out.extend_from_slice(format!("Content-Type: {ctype}\r\nContent-Length: {}\r\n", body.len()).as_bytes());
        http::connection_header(&mut out, 1, keepalive);
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(body.as_bytes());
        if stream.write_all(&out).await.is_err() || !keepalive {
            return;
        }
    }
}

/// `phphub health`: for the container health check.
fn health_check() -> i32 {
    let port = std::env::var("METRICS_PORT").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "9100".into());
    let addr = format!("127.0.0.1:{port}");
    let Ok(addr) = addr.parse::<SocketAddr>() else { return 1 };
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3)) else { return 1 };
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    if s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").is_err() {
        return 1;
    }
    let mut resp = Vec::new();
    let _ = s.read_to_end(&mut resp);
    if resp.starts_with(b"HTTP/1.1 200") { 0 } else { 1 }
}
