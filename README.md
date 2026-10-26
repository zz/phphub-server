# PHPHub

PHPHub is a PHP application server written in Rust. One process is both the
HTTP/1.1 server and the PHP runtime: it links the ZTS `libphp` and runs a
worker script on each PHP thread. The app boots once per thread; after that
each request only runs the app itself. It replaces FrankenPHP or nginx in the
app container, not Octoproxy. The full spec is in [docs/phphub.md](docs/phphub.md).

```text
client → Octoproxy → phphub:80
                       ├─ Rust: HTTP/1.1, static files (sendfile), timeouts, metrics
                       └─ N ZTS PHP threads, each with a warm worker
```

## Worker script

```php
<?php
require dirname(__DIR__) . '/vendor/autoload.php';

$app = new \Phalcon\Mvc\Application($di);

while (phphub_handle_request()) {
    $app->useImplicitView(false);
    echo $app->handle($_SERVER['REQUEST_URI'])->getContent();
    phphub_finish_request();
}
```

| Function | |
|---|---|
| `phphub_handle_request()` | Blocks until the next request. `false` means the server is stopping or the thread is due for a restart. Afterwards `$_GET`, `$_POST`, `$_COOKIE`, `$_FILES` and `$_SERVER` hold this request. |
| `phphub_finish_request()` | Sends this response and ends the request's output buffers and uploads. The app object stays. |

Everything built outside the loop survives between requests. Per-request
state (routing, dispatcher, view, session, transactions) has to be set up
again inside the loop. PHPHub never rebuilds `$app` behind your back.

A front controller that never calls `phphub_handle_request()` still works: it
runs once per request on an already warm interpreter (label `classic` in the
metrics). Use the loop for real traffic and benchmarks.

## Modes

`PHPHUB_MODE` is read once at startup (trimmed, case-insensitive). Spec:
[docs/modes.md](docs/modes.md).

| `PHPHUB_MODE` | Which script runs |
|---|---|
| unset or empty | Auto: a script whose source names `phphub_handle_request` boots as a worker; otherwise the first request decides, and a script without the loop runs once per request. |
| `worker` | The entry boots once per thread and loops. If it returns without calling `phphub_handle_request()`, an error is logged and that thread runs it once per request from then on. |
| `index` | The entry runs once per request. A loop in it gets `true` once, then `false`. Never becomes a worker. |
| `file` | Like nginx or FrankenPHP's `php_server`: an existing `.php`/`.phtml`/`.phar`/`.inc` file under `SERVER_ROOT` named by the URL runs (with `PATH_INFO` for `/a.php/extra`); anything else runs the entry. Symlinks that leave `SERVER_ROOT` count as missing. |
| anything else | The process logs `PHPHUB_MODE must be worker, index, or file` and exits non-zero before listening. |

In every mode static files, the dotfile 403 and "PHP source is never sent"
work the same. `REQUEST_URI` and `QUERY_STRING` are always the client's; no
`_url` is added. `SCRIPT_FILENAME`, `SCRIPT_NAME`, `PHP_SELF` and
`DOCUMENT_URI` name the entry, except in file mode when a file was picked.
Metrics use the entry path as the `worker` label in worker mode and `classic`
for index and file.

## Settings

| Variable | Default | |
|---|---|---|
| `PHPHUB_MODE` | unset (auto) | `worker`, `index` or `file`; see Modes. |
| `SERVER_ROOT` | `/var/www/html` | Document root. Absolute, `[A-Za-z0-9/_.-]`, no `..`, inside `/var/www/html`, or the server refuses to start. |
| `WORKER_SCRIPT` | `SERVER_ROOT/index.php` | Relative to `SERVER_ROOT` or absolute; must be inside `/var/www/html`. |
| `WORKERS` | CPU quota rounded up, at least 2 | PHP threads. |
| `MAX_REQUESTS` | `1000` | Restart one thread's PHP after this many requests. `0` never. |
| `PHP_TIMEOUT` | `60` | Seconds a request may take from arrival. After that the client gets 504 and the PHP thread is aborted and restarted. Also the drain time on stop. |
| `BODY_LIMIT` | `32m` | Request body limit (also `post_max_size` and `upload_max_filesize`). |
| `METRICS_PORT` | `9100` | `/metrics` and `/health`. |
| `SLOWLOG` | `0` | Seconds; log requests at least this slow. `0` is off. |
| `DISPLAY_ERRORS` | `0` | `1` shows PHP errors in pages. |
| `PHPHUB_PORT` | `80` | HTTP port, for tests. |

Fixed behaviour:

- HTTP/1.1 with keepalive, no TLS, no HTTP/2 or 3, no compression.
- No `Server` or `X-Powered-By` header (`expose_php=Off`).
- Any path segment starting with `.` gets 403 with an empty body. `.env` and
  `.git` are never served or run.
- Existing files that are not PHP are sent by Rust with `sendfile` and never
  reach a PHP thread. Everything else goes to PHP. PHP source is never sent
  as a file. Only the entry script runs, except in `PHPHUB_MODE=file`.
- `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Real-Ip` reach `$_SERVER`
  unchanged; PHPHub does not trust them for anything.
- `display_errors=Off`, `log_errors=On`. Opcache on with
  `validate_timestamps=1` and `revalidate_freq=2` (conf.d may change these).
- Logs go to standard error, one line each: PHP errors, thread timeouts,
  crashes and restarts, and slow requests when `SLOWLOG` is set. No access log.
- `SIGTERM`: stop accepting, wait up to `PHP_TIMEOUT` for running requests,
  then let `phphub_handle_request()` return `false` and exit.

## Metrics

`http://127.0.0.1:$METRICS_PORT/metrics`, Prometheus text, answered by Rust.
The `worker` label is the worker script path, or `classic`.

`phphub_total_threads`, `phphub_busy_threads`, `phphub_queue_depth`,
`phphub_total_workers`, `phphub_busy_workers`, `phphub_ready_workers`,
`phphub_worker_request_count`, `phphub_worker_request_time_seconds`,
`phphub_worker_queue_depth`, `phphub_worker_restarts`, `phphub_worker_crashes`,
`phphub_opcache_restarts{reason}`, `phphub_http_requests{status}`,
`phphub_static_requests`, `phphub_rejected_dotfiles`.

`/health` answers `ok` (200) once at least one PHP thread is ready, otherwise
`unavailable` (503). The image's `HEALTHCHECK` runs `phphub health`.

## Build

```sh
docker build -t phphub:dev .
./tests/smoke.sh phphub:dev
./bench/run.sh                 # side by side with FrankenPHP worker mode
```

The build stage takes `libphp.so` (ZTS), headers and the compiled extensions
(Phalcon 5.23, redis, memcached, yaml, apcu, …) from
`zhuzhu/php:8.5-phalcon-5.23-frankenphp`, which `docker-build-phalcon`
publishes. Nothing is compiled against PHP here except PHPHub itself. The
runtime image is plain Alpine with `libphp.so`, the extensions, their shared
libraries and `/usr/local/bin/phphub`; no Caddy, no FrankenPHP binary. Rust
code is linked statically into `phphub`; `libphp.so` and musl stay shared so
that `dlopen()`ed extensions resolve PHP's symbols.

## Layout

| Path | |
|---|---|
| `csrc/phphub.c` | SAPI module, worker request cycle, `phphub_*` PHP functions, thread start/abort. |
| `src/php/` | PHP thread pool, request handoff, response building. |
| `src/http.rs` | HTTP/1.1 parsing, body framing, routing, responses. |
| `src/statics.rs` | Static files: lookup, conditional and range requests, sendfile. |
| `src/metrics.rs` | Counters and `/metrics`. |
| `tests/smoke.sh` | Acceptance checks against an image. |
| `bench/` | Phalcon app and the side-by-side load test. |
