//! Counters and the Prometheus text for `/metrics`.

use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

/// Upper bounds of `phphub_request_duration_seconds`, in seconds.
pub const DURATION_BUCKETS: [f64; 13] = [0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.15, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// A Prometheus histogram: one count per bucket (not cumulative; render adds
/// them up), the last slot for anything above the top bound.
pub struct Histogram {
    buckets: [AtomicU64; DURATION_BUCKETS.len() + 1],
    sum_us: AtomicU64,
}

impl Histogram {
    const fn new() -> Self {
        Histogram { buckets: [const { AtomicU64::new(0) }; DURATION_BUCKETS.len() + 1], sum_us: AtomicU64::new(0) }
    }

    pub fn observe(&self, d: std::time::Duration) {
        let secs = d.as_secs_f64();
        let i = DURATION_BUCKETS.iter().position(|b| secs <= *b).unwrap_or(DURATION_BUCKETS.len());
        self.buckets[i].fetch_add(1, Relaxed);
        self.sum_us.fetch_add(d.as_micros() as u64, Relaxed);
    }

    /// Cumulative counts per bound, then the total.
    fn cumulative(&self) -> (Vec<u64>, u64) {
        let mut run = 0;
        let mut out = Vec::with_capacity(DURATION_BUCKETS.len());
        for (i, b) in self.buckets.iter().enumerate() {
            run += b.load(Relaxed);
            if i < DURATION_BUCKETS.len() {
                out.push(run);
            }
        }
        (out, run)
    }
}

pub struct LabelStats {
    pub requests: AtomicU64,
    pub time_us: AtomicU64,
    pub queue_us: AtomicU64,
    pub restarts: AtomicU64,
    pub crashes: AtomicU64,
    /// Arrival to finish, queue included: what the client waited for.
    pub duration: Histogram,
}

impl LabelStats {
    const fn new() -> Self {
        LabelStats {
            requests: AtomicU64::new(0),
            time_us: AtomicU64::new(0),
            queue_us: AtomicU64::new(0),
            restarts: AtomicU64::new(0),
            crashes: AtomicU64::new(0),
            duration: Histogram::new(),
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

    let _ = writeln!(out, "# HELP phphub_request_duration_seconds PHP requests from arrival to finish, queue included.");
    let _ = writeln!(out, "# TYPE phphub_request_duration_seconds histogram");
    for (label, w) in &labels {
        let h = &m.stats(*w).duration;
        let (cum, total) = h.cumulative();
        // {worker="x"} -> {worker="x",le="0.1"}
        let base = label.trim_end_matches('}');
        for (bound, n) in DURATION_BUCKETS.iter().zip(cum) {
            let _ = writeln!(out, "phphub_request_duration_seconds_bucket{base},le=\"{bound}\"}} {n}");
        }
        let _ = writeln!(out, "phphub_request_duration_seconds_bucket{base},le=\"+Inf\"}} {total}");
        let _ = writeln!(out, "phphub_request_duration_seconds_sum{label} {}", h.sum_us.load(Relaxed) as f64 / 1e6);
        let _ = writeln!(out, "phphub_request_duration_seconds_count{label} {total}");
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn histogram_is_cumulative_with_inf() {
        let h = Histogram::new();
        h.observe(Duration::from_millis(3));
        h.observe(Duration::from_millis(80));
        h.observe(Duration::from_millis(100));
        h.observe(Duration::from_secs(30));
        let (cum, total) = h.cumulative();
        assert_eq!(total, 4);
        assert_eq!(cum[0], 1); // <= 5ms
        assert_eq!(cum[5], 3); // <= 100ms, the bound itself counts
        assert_eq!(*cum.last().unwrap(), 3); // <= 10s; 30s only in +Inf
        assert_eq!(h.sum_us.load(Relaxed), 30_183_000);
    }

    #[test]
    fn render_writes_histogram_lines() {
        METRICS.worker.duration.observe(Duration::from_millis(20));
        METRICS.worker.requests.fetch_add(1, Relaxed);
        let text = render("/app/index.php", &[ThreadView { busy: false, ready: true, worker: true }], true);
        assert!(text.contains("# TYPE phphub_request_duration_seconds histogram"));
        assert!(text.contains("phphub_request_duration_seconds_bucket{worker=\"/app/index.php\",le=\"0.025\"} "));
        assert!(text.contains("phphub_request_duration_seconds_bucket{worker=\"/app/index.php\",le=\"+Inf\"} "));
        assert!(text.contains("phphub_request_duration_seconds_count{worker=\"/app/index.php\"} "));
    }
}
