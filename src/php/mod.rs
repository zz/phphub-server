//! PHP threads: each owns a TSRM context and runs the worker script.

pub mod ffi;
pub mod job;

use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender, select};
use tokio::sync::mpsc;

use crate::config::{Config, RunMode};
use crate::logln;
use crate::metrics::{METRICS, ThreadView};
use job::{Job, OWNER_CANCELLED, OWNER_NONE, Reply};

/// Responses up to this size are sent in one piece with Content-Length.
const STREAM_THRESHOLD: usize = 1 << 20;
const STREAM_CHUNK: usize = 64 << 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Mode {
    /// Not known yet: run per request until the script calls phphub_handle_request().
    Unknown = 0,
    Worker = 1,
    Classic = 2,
}

enum Ctrl {
    Shutdown,
    ModeChanged,
}

pub struct ThreadShared {
    idx: usize,
    ctrl_tx: Sender<Ctrl>,
    ctrl_rx: Receiver<Ctrl>,
    busy: AtomicBool,
    ready: AtomicBool,
    worker: AtomicBool,
    alive: AtomicBool,
    cur_seq: AtomicU64,
    kicked_seq: AtomicU64,
    /// The C context, guarded so a kick never races thread teardown.
    ctx: Mutex<usize>,
}

pub struct Pool {
    cfg: &'static Config,
    script_c: CString,
    jobs_tx: Sender<Job>,
    jobs_rx: Receiver<Job>,
    threads: Vec<Arc<ThreadShared>>,
    mode: AtomicU8,
    shutting_down: AtomicBool,
    seq: AtomicU64,
}

impl Pool {
    pub fn new(cfg: &'static Config) -> Pool {
        let (jobs_tx, jobs_rx) = crossbeam_channel::unbounded();
        let threads = (0..cfg.workers)
            .map(|idx| {
                let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
                Arc::new(ThreadShared {
                    idx,
                    ctrl_tx,
                    ctrl_rx,
                    busy: AtomicBool::new(false),
                    ready: AtomicBool::new(false),
                    worker: AtomicBool::new(false),
                    alive: AtomicBool::new(false),
                    cur_seq: AtomicU64::new(0),
                    kicked_seq: AtomicU64::new(0),
                    ctx: Mutex::new(0),
                })
            })
            .collect();

        // A script that names phphub_handle_request() is booted before any
        // traffic. Anything else waits for its first request.
        let initial = match cfg.mode {
            RunMode::Auto => match std::fs::read(&cfg.worker_script) {
                Ok(src) if contains(&src, b"phphub_handle_request") => Mode::Worker,
                _ => Mode::Unknown,
            },
            RunMode::Worker => Mode::Worker,
            RunMode::Index | RunMode::File => Mode::Classic,
        };

        Pool {
            cfg,
            script_c: CString::new(cfg.worker_script.clone()).expect("script path has no NUL"),
            jobs_tx,
            jobs_rx,
            threads,
            mode: AtomicU8::new(initial as u8),
            shutting_down: AtomicBool::new(false),
            seq: AtomicU64::new(1),
        }
    }

    /// Start PHP (on the calling thread) and its worker threads.
    pub fn start(&'static self) -> Result<Vec<std::thread::JoinHandle<()>>, String> {
        let ini = CString::new(self.cfg.ini_entries()).unwrap();
        let defaults = CString::new(self.cfg.ini_defaults()).unwrap();
        let rc = unsafe {
            ffi::phphub_php_startup(
                self.threads.len() as c_int,
                ini.as_ptr(),
                defaults.as_ptr(),
                self.cfg.php_timeout.as_secs() as c_int,
            )
        };
        if rc != 0 {
            return Err("PHP failed to start".into());
        }
        let entry = &self.cfg.worker_script;
        match (self.cfg.mode, self.mode()) {
            (RunMode::Worker, _) => logln!("PHPHUB_MODE=worker: {entry} boots once per thread"),
            (RunMode::Index, _) => logln!("PHPHUB_MODE=index: {entry} runs once per request"),
            (RunMode::File, _) => {
                logln!("PHPHUB_MODE=file: requested .php files run when they exist, otherwise {entry}")
            }
            (RunMode::Auto, Mode::Worker) => logln!("worker mode: {entry} boots once per thread"),
            (RunMode::Auto, _) => logln!("{entry} has no worker loop yet; the first request decides"),
        }
        let mut handles = Vec::new();
        for t in &self.threads {
            let t = t.clone();
            let h = std::thread::Builder::new()
                .name(format!("php-{}", t.idx))
                .stack_size(16 << 20)
                .spawn(move || thread_main(self, t))
                .map_err(|e| format!("cannot start PHP thread: {e}"))?;
            handles.push(h);
        }
        Ok(handles)
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Relaxed)
    }

    pub fn submit(&self, job: Job) {
        METRICS.queue_depth.fetch_add(1, Relaxed);
        let _ = self.jobs_tx.send(job);
    }

    pub fn mode(&self) -> Mode {
        match self.mode.load(Acquire) {
            1 => Mode::Worker,
            2 => Mode::Classic,
            _ => Mode::Unknown,
        }
    }

    /// Only PHPHUB_MODE unset lets the process mode move.
    fn set_mode(&self, m: Mode) -> Mode {
        if self.cfg.mode != RunMode::Auto {
            return self.mode();
        }
        let prev = self.mode.swap(m as u8, AcqRel);
        if m == Mode::Worker && prev != Mode::Worker as u8 {
            for t in &self.threads {
                let _ = t.ctrl_tx.send(Ctrl::ModeChanged);
            }
        }
        match prev {
            1 => Mode::Worker,
            2 => Mode::Classic,
            _ => Mode::Unknown,
        }
    }

    /// The connection stopped waiting for `seq`. Drop it if still queued,
    /// otherwise abort it on its thread. Returns the thread index if running.
    pub fn abandon(&self, shared: &job::JobShared, seq: u64) -> Option<usize> {
        match shared.owner.compare_exchange(OWNER_NONE, OWNER_CANCELLED, AcqRel, Acquire) {
            Ok(_) => None,
            Err(owner) => {
                let t = self.threads.get(owner)?;
                t.kicked_seq.store(seq, Release);
                if t.cur_seq.load(Acquire) == seq {
                    let g = t.ctx.lock().unwrap();
                    if *g != 0 {
                        unsafe { ffi::phphub_kick(*g as *mut c_void, seq) };
                    }
                }
                Some(owner)
            }
        }
    }

    pub fn shutdown(&self) {
        self.shutting_down.store(true, Release);
        for t in &self.threads {
            let _ = t.ctrl_tx.send(Ctrl::Shutdown);
        }
    }

    pub fn alive_threads(&self) -> usize {
        self.threads.iter().filter(|t| t.alive.load(Acquire)).count()
    }

    pub fn healthy(&self) -> bool {
        self.threads.iter().any(|t| t.ready.load(Acquire))
    }

    pub fn views(&self) -> Vec<ThreadView> {
        self.threads
            .iter()
            .map(|t| ThreadView {
                busy: t.busy.load(Relaxed),
                ready: t.ready.load(Relaxed),
                worker: t.worker.load(Relaxed),
            })
            .collect()
    }

    pub fn script(&self) -> &str {
        &self.cfg.worker_script
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A request a PHP thread is working on, and the response being built.
struct Active {
    job: Job,
    status: u16,
    head: Option<Vec<u8>>,
    php_len: Option<u64>,
    body: Vec<u8>,
    stream: Option<mpsc::Sender<Bytes>>,
    dead: bool,
    body_off: usize,
    started: Instant,
}

impl Active {
    fn new(job: Job) -> Active {
        Active {
            job,
            status: 200,
            head: None,
            php_len: None,
            body: Vec::new(),
            stream: None,
            dead: false,
            body_off: 0,
            started: Instant::now(),
        }
    }

    fn reset_output(&mut self) {
        self.status = 200;
        self.head = None;
        self.php_len = None;
        self.body.clear();
    }

    fn write(&mut self, data: &[u8], may_stream: bool) {
        if self.dead {
            return;
        }
        self.body.extend_from_slice(data);
        if self.stream.is_some() {
            if self.body.len() >= STREAM_CHUNK {
                self.push_chunk();
            }
        } else if may_stream && self.body.len() >= STREAM_THRESHOLD {
            self.start_stream();
        }
    }

    fn start_stream(&mut self) {
        let (tx, rx) = mpsc::channel(8);
        let reply = Reply::Stream {
            status: self.status,
            head: self.head.take().unwrap_or_default(),
            php_len: self.php_len,
            rx,
        };
        let sent = match self.job.tx.take() {
            Some(otx) => otx.send(reply).is_ok(),
            None => false,
        };
        if sent {
            self.stream = Some(tx);
            self.push_chunk();
        } else {
            self.dead = true;
            self.body = Vec::new();
        }
    }

    fn push_chunk(&mut self) {
        if self.body.is_empty() {
            return;
        }
        let chunk = Bytes::from(std::mem::take(&mut self.body));
        if let Some(tx) = &self.stream {
            if tx.blocking_send(chunk).is_err() {
                self.dead = true;
                self.stream = None;
            }
        }
    }

    fn send_headers(&mut self, status: u16, lines: &[&[u8]]) {
        self.status = status;
        let mut head = Vec::with_capacity(lines.iter().map(|l| l.len() + 2).sum());
        for line in lines {
            let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
            let name = trim(&line[..colon]);
            let value = trim(&line[colon + 1..]);
            if name.is_empty() || line.iter().any(|&b| b == b'\r' || b == b'\n') {
                continue;
            }
            let drop = [
                &b"server"[..],
                b"x-powered-by",
                b"transfer-encoding",
                b"connection",
                b"keep-alive",
                b"date",
            ];
            if drop.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                continue;
            }
            if name.eq_ignore_ascii_case(b"content-length") {
                self.php_len = std::str::from_utf8(value).ok().and_then(|s| s.parse().ok());
                continue;
            }
            head.extend_from_slice(name);
            head.extend_from_slice(b": ");
            head.extend_from_slice(value);
            head.extend_from_slice(b"\r\n");
        }
        self.head = Some(head);
    }

    /// Hand the response to the connection. Returns the status sent and the
    /// job, whose strings PHP may still point at until the next request.
    fn finish(mut self, crashed: bool) -> (u16, Job) {
        if self.head.is_none() && crashed {
            self.status = 500;
        }
        if self.stream.is_some() {
            self.push_chunk();
            self.stream = None;
        } else if let Some(tx) = self.job.tx.take() {
            let _ = tx.send(Reply::Full {
                status: self.status,
                head: self.head.take().unwrap_or_default(),
                php_len: self.php_len,
                body: std::mem::take(&mut self.body),
            });
        }
        (self.status, self.job)
    }
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !b.is_ascii_whitespace()).map(|i| i + 1).unwrap_or(start);
    &s[start..end.max(start)]
}

#[derive(Clone, Copy, PartialEq)]
enum NextNull {
    None,
    Shutdown,
    Restart,
}

/// Owned by one PHP thread; C gets a pointer to it.
struct ThreadState {
    pool: &'static Pool,
    shared: Arc<ThreadShared>,
    current: Option<Active>,
    /// The request a run started with, held while the bootstrap is discarded.
    bootstrap: Option<Active>,
    run_started: Instant,
    run_requests: u64,
    run_worker: bool,
    /// This run serves one request only (index/file, or a worker fallback).
    run_once: bool,
    /// PHPHUB_MODE=worker: the script returned without entering the loop,
    /// so this thread now runs it per request.
    index_fallback: bool,
    /// PHPHUB_MODE=worker: the last boot died before the loop; boot the next
    /// run inside a request instead of spinning.
    boot_with_request: bool,
    next_null: NextNull,
    /// "GET /path took 1.2s" for the request a crash happened in.
    crash_note: Option<(String, u64)>,
    /// The last finished request. SG(request_info) still points into it
    /// until PHP detaches it at the next request or the end of the run.
    retired: Option<Job>,
}

enum Wait {
    Job(Job),
    Shutdown,
    ModeChanged,
}

impl ThreadState {
    fn wait(&mut self, in_worker: bool) -> Wait {
        loop {
            select! {
                recv(self.pool.jobs_rx) -> j => {
                    let Ok(j) = j else { return Wait::Shutdown };
                    METRICS.queue_depth.fetch_sub(1, Relaxed);
                    if !j.claim(self.shared.idx) {
                        continue; // the connection already timed out
                    }
                    return Wait::Job(j);
                }
                recv(self.shared.ctrl_rx) -> c => match c {
                    Ok(Ctrl::Shutdown) | Err(_) => return Wait::Shutdown,
                    Ok(Ctrl::ModeChanged) => {
                        if !in_worker && self.pool.mode() == Mode::Worker {
                            return Wait::ModeChanged;
                        }
                    }
                },
            }
        }
    }

    fn activate(&mut self, job: Job) -> *const ffi::HubRequest {
        self.shared.cur_seq.store(job.seq, Release);
        self.shared.busy.store(true, Relaxed);
        self.run_requests += 1;
        self.retired = None;
        let a = self.current.insert(Active::new(job));
        &*a.job.creq
    }

    /// Streaming is only allowed once the run's mode is settled; output of
    /// a first-request bootstrap may still be thrown away.
    fn may_stream(&self) -> bool {
        self.run_worker || self.run_once || self.pool.mode() == Mode::Classic
    }

    fn finish_current(&mut self, crashed: bool) {
        let Some(a) = self.current.take() else { return };
        let seq = a.job.seq;
        let took = a.started.elapsed();
        let total = a.job.received.elapsed();
        let method = a.job.method().to_string();
        let script = a.job.script(self.pool.script()).into_owned();
        let path = if self.pool.cfg.slowlog.is_some() || crashed { Some(a.job.path()) } else { None };
        let (status, job) = a.finish(crashed);
        self.retired = Some(job);

        self.shared.busy.store(false, Relaxed);
        self.shared.cur_seq.store(0, Release);

        let stats = METRICS.stats(self.run_worker);
        stats.requests.fetch_add(1, Relaxed);
        stats.time_us.fetch_add(took.as_micros() as u64, Relaxed);

        if let (Some(limit), Some(path)) = (self.pool.cfg.slowlog, &path) {
            if total >= limit {
                logln!(
                    "slow request method={method} path={path} script={script} time={:.3}s status={status}",
                    total.as_secs_f64()
                );
            }
        }
        if crashed {
            self.crash_note = Some((
                format!("method={method} path={} time={:.3}s", path.unwrap_or_default(), total.as_secs_f64()),
                seq,
            ));
        }
    }

    fn end_run(&mut self, flags: c_int, had_job: bool) {
        let fatal = flags & ffi::RUN_FATAL != 0;
        let handled = flags & ffi::RUN_HANDLED != 0;

        // Requests C did not finish (should not happen) get an error.
        if let Some(a) = self.bootstrap.take() {
            self.current = Some(a);
            self.finish_current(true);
        }
        if self.current.is_some() {
            self.finish_current(true);
        }

        let script = self.pool.script();
        let idx = self.shared.idx;
        let uptime = self.run_started.elapsed().as_secs_f64();
        let note = self.crash_note.take();
        let timed_out = note
            .as_ref()
            .is_some_and(|(_, seq)| *seq == self.shared.kicked_seq.load(Acquire));
        // Timeouts were already logged when they happened.
        let note_str = note.map(|(s, _)| format!(" {s}")).unwrap_or_default();

        if handled {
            self.boot_with_request = false;
            match self.next_null {
                NextNull::Shutdown => {}
                NextNull::Restart => {
                    METRICS.worker.restarts.fetch_add(1, Relaxed);
                    logln!(
                        "thread {idx} restarted after {} requests (MAX_REQUESTS) script={script} uptime={uptime:.1}s",
                        self.run_requests
                    );
                }
                NextNull::None => {
                    METRICS.worker.crashes.fetch_add(1, Relaxed);
                    if !timed_out {
                        let why = if fatal { "fatal error" } else { "script exited" };
                        logln!("thread {idx} crashed ({why}), restarting: script={script}{note_str} uptime={uptime:.1}s");
                    }
                }
            }
        } else if self.run_once {
            // index/file: PHP already logged a fatal error itself.
            if fatal {
                METRICS.classic.crashes.fetch_add(1, Relaxed);
            }
        } else if self.pool.cfg.mode == RunMode::Worker {
            if fatal {
                // Do not spin on a broken boot: the next run starts inside a
                // request, which gets the error page.
                METRICS.worker.crashes.fetch_add(1, Relaxed);
                if !had_job {
                    logln!(
                        "thread {idx} crashed before phphub_handle_request(): script={script} time={uptime:.3}s; next run waits for a request"
                    );
                }
                self.boot_with_request = true;
            } else {
                logln!(
                    "error: {script} returned without calling phphub_handle_request() (PHPHUB_MODE=worker); thread {idx} now runs it once per request"
                );
                self.index_fallback = true;
                self.boot_with_request = false;
            }
        } else if had_job {
            if self.pool.mode() == Mode::Unknown {
                if self.pool.set_mode(Mode::Classic) != Mode::Classic {
                    logln!("{script} does not call phphub_handle_request(); running it once per request (classic)");
                }
            }
            // PHP already logged the fatal error itself.
            if fatal {
                METRICS.classic.crashes.fetch_add(1, Relaxed);
            }
        } else if fatal {
            // The worker died while booting. Do not spin: wait for a request.
            METRICS.worker.crashes.fetch_add(1, Relaxed);
            logln!(
                "thread {idx} crashed before phphub_handle_request(): script={script} time={uptime:.3}s; next run waits for a request"
            );
            self.pool.set_mode(Mode::Unknown);
        } else {
            logln!("{script} returned without calling phphub_handle_request(); running it once per request (classic)");
            self.pool.set_mode(Mode::Classic);
        }

        self.next_null = NextNull::None;
        self.run_worker = false;
        self.shared.ready.store(false, Release);
    }
}

fn thread_main(pool: &'static Pool, shared: Arc<ThreadShared>) {
    let mut st = Box::new(ThreadState {
        pool,
        shared: shared.clone(),
        current: None,
        bootstrap: None,
        run_started: Instant::now(),
        run_requests: 0,
        run_worker: false,
        run_once: false,
        index_fallback: false,
        boot_with_request: false,
        next_null: NextNull::None,
        crash_note: None,
        retired: None,
    });
    let st_ptr: *mut ThreadState = &mut *st;
    let ctx = unsafe { ffi::phphub_thread_init(st_ptr as *mut c_void, pool.script_c.as_ptr()) };
    *shared.ctx.lock().unwrap() = ctx as usize;
    shared.alive.store(true, Release);

    loop {
        if pool.shutting_down.load(Acquire) {
            break;
        }
        let st = unsafe { &mut *st_ptr };
        let (boot_worker, once) = match pool.cfg.mode {
            RunMode::Auto => (pool.mode() == Mode::Worker, false),
            RunMode::Worker => (!st.index_fallback && !st.boot_with_request, st.index_fallback),
            RunMode::Index | RunMode::File => (false, true),
        };
        st.run_once = once;
        let job = if boot_worker {
            shared.worker.store(true, Relaxed);
            None
        } else {
            shared.worker.store(pool.cfg.mode == RunMode::Worker && !st.index_fallback, Relaxed);
            shared.ready.store(true, Release);
            match st.wait(false) {
                Wait::Job(j) => Some(j),
                Wait::Shutdown => break,
                Wait::ModeChanged => continue,
            }
        };

        st.run_started = Instant::now();
        st.run_requests = 0;
        let had_job = job.is_some();
        let creq = match job {
            Some(j) => st.activate(j),
            None => std::ptr::null(),
        };
        let flags = unsafe { ffi::phphub_run(ctx, creq, once as c_int) };
        let st = unsafe { &mut *st_ptr };
        st.end_run(flags, had_job);
        st.retired = None;
    }

    shared.ready.store(false, Release);
    shared.alive.store(false, Release);
    *shared.ctx.lock().unwrap() = 0;
    unsafe { ffi::phphub_thread_free(ctx) };
    drop(st);
}

// ------------------------------------------------------------ C callbacks

unsafe fn state<'a>(rs: *mut c_void) -> &'a mut ThreadState {
    unsafe { &mut *(rs as *mut ThreadState) }
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_next(rs: *mut c_void) -> *const ffi::HubRequest {
    let st = unsafe { state(rs) };
    if !st.run_worker {
        st.run_worker = true;
        st.shared.worker.store(true, Relaxed);
        if st.pool.set_mode(Mode::Worker) != Mode::Worker {
            logln!("worker mode: {} on {} threads", st.pool.script(), st.pool.threads.len());
        }
    }
    st.shared.ready.store(true, Release);
    // PHP detached the previous request before asking for the next one.
    st.retired = None;

    if let Some(mut a) = st.bootstrap.take() {
        a.body_off = 0;
        a.started = Instant::now();
        st.shared.busy.store(true, Relaxed);
        let a = st.current.insert(a);
        return &*a.job.creq;
    }
    if st.pool.cfg.max_requests > 0 && st.run_requests >= st.pool.cfg.max_requests {
        st.next_null = NextNull::Restart;
        return std::ptr::null();
    }
    if st.pool.shutting_down.load(Acquire) {
        st.next_null = NextNull::Shutdown;
        return std::ptr::null();
    }
    match st.wait(true) {
        Wait::Job(j) => st.activate(j),
        _ => {
            st.next_null = NextNull::Shutdown;
            std::ptr::null()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_detach(rs: *mut c_void) {
    let st = unsafe { state(rs) };
    if let Some(mut a) = st.current.take() {
        a.reset_output();
        st.shared.busy.store(false, Relaxed);
        st.bootstrap = Some(a);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_read_body(rs: *mut c_void, buf: *mut c_char, len: usize) -> usize {
    let st = unsafe { state(rs) };
    let Some(a) = st.current.as_mut().or(st.bootstrap.as_mut()) else { return 0 };
    let rest = &a.job.body[a.body_off.min(a.job.body.len())..];
    let n = rest.len().min(len);
    unsafe { std::ptr::copy_nonoverlapping(rest.as_ptr(), buf as *mut u8, n) };
    a.body_off += n;
    n
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_write(rs: *mut c_void, buf: *const c_char, len: usize) {
    let st = unsafe { state(rs) };
    let may_stream = st.may_stream();
    if let Some(a) = st.current.as_mut() {
        let data = unsafe { std::slice::from_raw_parts(buf as *const u8, len) };
        a.write(data, may_stream);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_send_headers(rs: *mut c_void, status: c_int, lines: *const ffi::HubStr, count: usize) {
    let st = unsafe { state(rs) };
    if let Some(a) = st.current.as_mut() {
        let lines: Vec<&[u8]> = unsafe { std::slice::from_raw_parts(lines, count) }
            .iter()
            .map(|s| unsafe { std::slice::from_raw_parts(s.ptr as *const u8, s.len) })
            .collect();
        let status = if (100..600).contains(&status) { status as u16 } else { 500 };
        a.send_headers(status, &lines);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_flush(rs: *mut c_void) {
    let st = unsafe { state(rs) };
    if !st.may_stream() {
        return;
    }
    if let Some(a) = st.current.as_mut() {
        if a.dead {
            return;
        }
        if a.stream.is_none() {
            a.start_stream();
        } else {
            a.push_chunk();
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_finish(rs: *mut c_void, crashed: c_int) {
    let st = unsafe { state(rs) };
    st.finish_current(crashed != 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_log(msg: *const c_char, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(msg as *const u8, len) };
    crate::log::line(&String::from_utf8_lossy(bytes));
}

#[unsafe(no_mangle)]
pub extern "C" fn phphub_rs_opcache_restart(reason: c_int) {
    if let Some(c) = METRICS.opcache_restarts.get(reason as usize) {
        c.fetch_add(1, Relaxed);
    }
}

pub fn php_shutdown() {
    unsafe { ffi::phphub_php_shutdown() };
}
