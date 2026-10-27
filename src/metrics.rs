//! Counters and the Prometheus text for `/metrics`.

use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

pub struct LabelStats {
    pub requests: AtomicU64,
    pub time_us: AtomicU64,
    pub queue_us: AtomicU64,
    pub restarts: AtomicU64,
    pub crashes: AtomicU64,
}

impl LabelStats {
    const fn new() -> Self {
        LabelStats {
            requests: AtomicU64::new(0),
            time_us: AtomicU64::new(0),
            queue_us: AtomicU64::new(0),
            restarts: AtomicU64::new(0),
            crashes: AtomicU64::new(0),
        }
    }
}

pub struct Metrics {
    http_status: [AtomicU64; 600],
    pub static_requests: AtomicU64,
    pub rejected_dotfiles: AtomicU64,
    pub queue_depth: AtomicI64,
    /// oom, hash, manual
    pub opcache_restarts: [AtomicU64; 3],
    pub worker: LabelStats,
    pub classic: LabelStats,
}

pub static METRICS: Metrics = Metrics {
    http_status: [const { AtomicU64::new(0) }; 600],
    static_requests: AtomicU64::new(0),
    rejected_dotfiles: AtomicU64::new(0),
    queue_depth: AtomicI64::new(0),
    opcache_restarts: [const { AtomicU64::new(0) }; 3],
    worker: LabelStats::new(),
    classic: LabelStats::new(),
};

impl Metrics {
    pub fn http(&self, status: u16) {
        if let Some(c) = self.http_status.get(status as usize) {
            c.fetch_add(1, Relaxed);
        }
    }

    pub fn stats(&self, worker: bool) -> &LabelStats {
        if worker { &self.worker } else { &self.classic }
    }
}

/// What a scrape needs to know about one PHP thread.
pub struct ThreadView {
    pub busy: bool,
    pub ready: bool,
    /// Running the worker loop (as opposed to classic, per-request runs).
    pub worker: bool,
}

pub fn render(script: &str, threads: &[ThreadView], queue_label_worker: bool) -> String {
    let m = &METRICS;
    let mut out = String::with_capacity(4096);
    let total = threads.len();
    let busy = threads.iter().filter(|t| t.busy).count();
    let queue = m.queue_depth.load(Relaxed).max(0);

    gauge(&mut out, "phphub_total_threads", "PHP threads.", &[("", total as f64)]);
    gauge(&mut out, "phphub_busy_threads", "PHP threads handling a request.", &[("", busy as f64)]);
    gauge(&mut out, "phphub_queue_depth", "Requests waiting for a PHP thread.", &[("", queue as f64)]);

    let label_w = format!("{{worker=\"{}\"}}", escape(script));
    let label_c = "{worker=\"classic\"}".to_string();
    let count = |worker: bool, f: &dyn Fn(&ThreadView) -> bool| {
        threads.iter().filter(|t| t.worker == worker && f(t)).count() as f64
    };
    // A label shows up once it has threads or history.
    let has = |worker: bool| {
        let s = m.stats(worker);
        threads.iter().any(|t| t.worker == worker)
            || s.requests.load(Relaxed) > 0
            || s.restarts.load(Relaxed) > 0
            || s.crashes.load(Relaxed) > 0
            || queue_label_worker == worker
    };
    let mut labels: Vec<(&str, bool)> = Vec::new();
    if has(true) {
        labels.push((&label_w, true));
    }
    if has(false) {
        labels.push((&label_c, false));
    }

    let per = |f: &dyn Fn(bool) -> f64| -> Vec<(&str, f64)> { labels.iter().map(|(l, w)| (*l, f(*w))).collect() };

    gauge(&mut out, "phphub_total_workers", "PHP threads per worker script.", &per(&|w| count(w, &|_| true)));
    gauge(&mut out, "phphub_busy_workers", "Threads of this worker handling a request.", &per(&|w| count(w, &|t| t.busy)));
    gauge(&mut out, "phphub_ready_workers", "Threads of this worker ready for requests.", &per(&|w| count(w, &|t| t.ready)));
    counter(&mut out, "phphub_worker_request_count", "Requests this worker finished.",
        &per(&|w| m.stats(w).requests.load(Relaxed) as f64));
    counter(&mut out, "phphub_worker_request_time_seconds", "Seconds this worker spent on requests.",
        &per(&|w| m.stats(w).time_us.load(Relaxed) as f64 / 1e6));
    counter(&mut out, "phphub_worker_queue_time_seconds", "Seconds requests for this worker waited for a free PHP thread.",
        &per(&|w| m.stats(w).queue_us.load(Relaxed) as f64 / 1e6));
    gauge(&mut out, "phphub_worker_queue_depth", "Requests queued for this worker.",
        &per(&|w| if w == queue_label_worker { queue as f64 } else { 0.0 }));
    counter(&mut out, "phphub_worker_restarts", "Planned thread restarts, such as MAX_REQUESTS.",
        &per(&|w| m.stats(w).restarts.load(Relaxed) as f64));
    counter(&mut out, "phphub_worker_crashes", "Unexpected thread exits, including timeouts and fatal errors.",
        &per(&|w| m.stats(w).crashes.load(Relaxed) as f64));

    let oc = &m.opcache_restarts;
    counter(&mut out, "phphub_opcache_restarts", "Opcache shared memory restarts.", &[
        ("{reason=\"oom\"}", oc[0].load(Relaxed) as f64),
        ("{reason=\"hash\"}", oc[1].load(Relaxed) as f64),
        ("{reason=\"manual\"}", oc[2].load(Relaxed) as f64),
    ]);

    let statuses: Vec<(String, f64)> = m
        .http_status
        .iter()
        .enumerate()
        .filter_map(|(code, c)| {
            let v = c.load(Relaxed);
            (v > 0).then(|| (format!("{{status=\"{code}\"}}"), v as f64))
        })
        .collect();
    let statuses: Vec<(&str, f64)> = statuses.iter().map(|(l, v)| (l.as_str(), *v)).collect();
    counter(&mut out, "phphub_http_requests", "HTTP responses by status code.", &statuses);
    counter(&mut out, "phphub_static_requests", "Static file requests served without PHP.",
        &[("", m.static_requests.load(Relaxed) as f64)]);
    counter(&mut out, "phphub_rejected_dotfiles", "Requests for dot paths answered with 403.",
        &[("", m.rejected_dotfiles.load(Relaxed) as f64)]);
    out
}

fn gauge(out: &mut String, name: &str, help: &str, rows: &[(&str, f64)]) {
    family(out, name, help, "gauge", rows)
}

fn counter(out: &mut String, name: &str, help: &str, rows: &[(&str, f64)]) {
    family(out, name, help, "counter", rows)
}

fn family(out: &mut String, name: &str, help: &str, kind: &str, rows: &[(&str, f64)]) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    for (labels, v) in rows {
        let _ = writeln!(out, "{name}{labels} {v}");
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}
