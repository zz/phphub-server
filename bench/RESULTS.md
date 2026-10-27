# Benchmark record: PHPHub vs FrankenPHP worker mode

Date: 2026-10-26. Command: `bench/run.sh` (and `CPUS=0.2 RATE=600 ROUNDS=1`).

## Setup

- Same Phalcon 5.23 app (`bench/app/bootstrap.php`), built once per thread.
  PHPHub runs it with the `phphub_handle_request()` loop (`public/phphub.php`),
  FrankenPHP with `frankenphp_handle_request()` (`public/franken.php`). Both
  set one `X-Served` header per request.
- PHPHub: `phphub:dev` from this repo, `WORKERS=2`, `MAX_REQUESTS=0`.
- FrankenPHP: `zhuzhu/php:8.5-phalcon-5.23-frankenphp` (FrankenPHP 1.13.3,
  PHP 8.5.12 ZTS), 2 worker threads, no `encode`, `bench/app/Caddyfile`.
- Each server container: `--cpus=1 --memory=512m` (second run `--cpus=0.2`).
- Client: `wrk -t1 -c16` (closed loop, 12 s, after a 3 s warm-up) and
  `hey -c16 -q` at a fixed rate, in a separate container on the same Docker
  network.
- Host: Docker in a 2-CPU, 3 GiB Lima VM on Apple Silicon (arm64).
- The app is copied into a Docker volume. A bind mount from macOS makes every
  `stat`/`open` slow; FrankenPHP checks files on every request, so a bind
  mount made it look 3x slower than it is.

Routes: `/` renders a 20-row HTML list through a controller; `/item/7?q=abc`
returns JSON; `/hello` returns `hello`; `/app.css` is a 6.6 KB static file.

## 1 CPU, 512 MiB

Closed loop (wrk, 16 connections), mean of 2 rounds:

| Route | PHPHub req/s | FrankenPHP req/s | PHPHub p50 | FrankenPHP p50 | PHPHub p99 | FrankenPHP p99 |
|---|---:|---:|---:|---:|---:|---:|
| `/` | 28,047 | 8,974 | 0.38 ms | 1.76 ms | 39.5 ms | 4.2–9.1 ms |
| `/item/7` | 32,753 | 9,774 | 0.33 ms | 1.62 ms | 38.7 ms | 3.9 ms |
| `/hello` | 47,525 | 11,022 | 0.22 ms | 1.43 ms | 35.2 ms | 3.6 ms |
| `/app.css` | 129,711 | 27,056 | 0.12 ms | 0.60 ms | 0.46 ms | 2.0 ms |

Fixed 4,000 req/s (hey), all responses 200:

| Route | PHPHub p50 | FrankenPHP p50 | PHPHub p99 | FrankenPHP p99 |
|---|---:|---:|---:|---:|
| `/` | 0.6 ms | 1.4 ms | 1.6 ms | 4.1 ms |
| `/item/7` | 0.6 ms | 1.2 ms | 1.4 ms | 3.2 ms |
| `/hello` | 0.5 ms | 1.1 ms | 1.2 ms | 3.2 ms |
| `/app.css` | 0.2 ms | 0.5 ms | 1.1 ms | 1.9 ms |

Memory after the run: PHPHub 24 MiB, FrankenPHP 43–47 MiB.

## 0.2 CPU, 512 MiB (lab setting)

| Route | PHPHub req/s | FrankenPHP req/s | PHPHub p50 | FrankenPHP p50 | PHPHub p99 | FrankenPHP p99 |
|---|---:|---:|---:|---:|---:|---:|
| `/` | 2,843 | 1,622 | 1.42 ms | 4.07 ms | 90 ms | 81 ms |
| `/item/7` | 3,238 | 1,807 | 1.33 ms | 3.73 ms | 89 ms | 80 ms |
| `/hello` | 4,626 | 2,169 | 0.85 ms | 3.21 ms | 88 ms | 78 ms |
| `/app.css` | 19,136 | 3,781 | 0.24 ms | 1.63 ms | 79 ms | 82 ms |

Fixed 600 req/s (hey), all responses 200:

| Route | PHPHub p50 | FrankenPHP p50 | PHPHub p99 | FrankenPHP p99 |
|---|---:|---:|---:|---:|
| `/` | 1.1 ms | 2.5 ms | 4.0 ms | 5.6 ms |
| `/item/7` | 1.1 ms | 2.3 ms | 3.4 ms | 5.3 ms |
| `/hello` | 0.9 ms | 2.1 ms | 3.4 ms | 5.2 ms |
| `/app.css` | 0.5 ms | 1.1 ms | 2.0 ms | 4.1 ms |

## Reading the numbers

- Throughput: PHPHub is 3–4.5x FrankenPHP on PHP routes at 1 CPU and about
  2x at 0.2 CPU; static files 5x.
- Latency at the same load: PHPHub's p50 and p99 are lower on every route.
- The high closed-loop p99 for PHPHub at 1 CPU is CFS quota throttling, not
  queueing in PHPHub: when it saturates its 1-CPU quota with 3 busy threads on
  a 2-CPU VM, the kernel pauses the container for the rest of each 100 ms
  period (`cpu.stat` showed 4.1 s throttled in an 8 s run). With `--cpus=2`
  the same test gives 73,900 req/s on `/hello` with p99 0.73 ms. FrankenPHP
  never reached its quota at 1 CPU (0 s throttled), so it never hit this.
  At 0.2 CPU both are throttled and both p99s land around 80–90 ms.

## PHPHUB_MODE (2026-10-26, after the modes change)

Same setup, 1 CPU, 512 MiB, 2 PHP threads, one 10 s round. `/plain.php` is a
one-line PHP file. Rows marked `wrk timeout 16` in the raw output are the
16 requests still open when wrk stops; they showed up on both servers and
never as read or write errors.

### worker vs FrankenPHP worker

No change from before the modes work.

| Route | PHPHub req/s | FrankenPHP req/s | at 4,000 req/s PHPHub p50 / p99 | FrankenPHP p50 / p99 |
|---|---:|---:|---:|---:|
| `/` | 27,844 | 8,770 | 0.5 / 1.1 ms | 1.3 / 3.3 ms |
| `/item/7` | 32,697 | 9,686 | 0.4 / 1.0 ms | 1.1 / 3.1 ms |
| `/hello` | 46,226 | 10,986 | 0.3 / 0.9 ms | 1.0 / 2.7 ms |
| `/app.css` | 127,143 | 26,071 | 0.2 / 0.5 ms | 0.5 / 1.5 ms |
| `/plain.php` | 41,643 | 11,117 | 0.4 / 0.8 ms | 1.0 / 2.5 ms |

### file and index vs FrankenPHP `php_server` without a worker

The entry is an unmodified front controller (`public/index.php`) that builds
the Phalcon app on every request. FrankenPHP has no index mode; its
`php_server` behaves like file mode in both tables (it runs `/plain.php`).

| Route | file req/s | index req/s | FrankenPHP req/s | at 1,000 req/s file p50 / p99 | index p50 / p99 | FrankenPHP p50 / p99 |
|---|---:|---:|---:|---:|---:|---:|
| `/` | 10,242 | 10,069 | 5,902 | 1.3 / 3.0 ms | 1.3 / 3.1 ms | 2.6 / 5.2 ms |
| `/item/7` | 9,945 | 10,184 | 6,304 | 1.3 / 3.4 ms | 1.6 / 4.0 ms | 2.6 / 5.2 ms |
| `/hello` | 12,330 | 12,352 | 6,871 | 1.5 / 3.8 ms | 1.2 / 2.5 ms | 2.4 / 5.4 ms |
| `/app.css` | 126,808 | 120,339 | 25,915 | 0.5 / 2.3 ms | 0.2 / 0.8 ms | 0.9 / 3.6 ms |
| `/plain.php` | 49,707 | 12,293 (entry) | 11,458 | 0.8 / 3.2 ms | 1.1 / 2.4 ms | 1.8 / 4.1 ms |

Rebuilding the app per request drops throughput to about a third of the
worker loop (10k vs 28k req/s on `/`), which is why benchmarks use the loop.
Closed-loop p99 for PHPHub is again the 1-CPU quota throttling described
above; FrankenPHP never reaches its quota.

## mimalloc (2026-10-27)

`phphub` linked against Alpine's `libmimalloc.so.2` versus musl's malloc,
same image otherwise. Phalcon bench app, `--cpus=2`, 4 PHP threads, `wrk -t2
-c32`, 12 s after a 3 s warm-up, two rounds each.

| Mode, route | musl req/s | mimalloc req/s | musl p50 | mimalloc p50 |
|---|---:|---:|---:|---:|
| index `/` | 18,560–18,860 | 21,540–21,640 | 1.61–1.66 ms | 1.41–1.42 ms |
| index `/item/7` | 18,070–18,870 | 21,240–21,750 | 1.65–1.71 ms | 1.40–1.42 ms |
| worker `/` | 47,590–48,610 | 52,580–52,670 | 0.60–0.63 ms | 0.54 ms |
| worker `/item/7` | 55,380–56,910 | 57,480–59,710 | 0.51–0.53 ms | 0.47–0.50 ms |

Index mode, which builds the app per request and allocates the most, gains
13–18%; worker mode 2–10%. Resident memory after the run is about 10 MiB
higher (36 vs 27 MiB).
