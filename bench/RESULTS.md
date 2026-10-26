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
