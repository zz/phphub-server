# PHPHub

PHPHub 是用 Rust 写的 PHP 应用服务器，性能对标 FrankenPHP 的 worker 模式。PHP 嵌在同一个进程里，应用启动一次，之后每个请求只跑业务本身。它替换容器里的 FrankenPHP 或 nginx，不替换 Octoproxy。

仓库 `/Users/fred/Workspace/phphub-server` 目前只有这份规格。ZTS 的 libphp 和 Phalcon 仍由 `docker-build-phalcon` 编译。

## 对标什么

对标的是 FrankenPHP 的这条路径，不是 nginx + php-fpm：

- 没有 FastCGI，没有额外的 PHP 进程跳转。
- 一个线程上的 worker 脚本只启动一次。Composer、opcache、Phalcon 的应用对象都留着。
- 静态文件由 HTTP 服务器直接发，不进 PHP。
- 并发靠同一个进程里的多条线程，线程数跟着 CPU。

实验室里的入口到应用走的是 HTTP/1.1。PHPHub 把这条连接的开销做小：keepalive、请求头零拷贝进 PHP、响应直接写回。不在应用容器里做 HTTP/2 或 HTTP/3，那一跳到不了 Octoproxy 后面。

第一版要拿出和同机 FrankenPHP worker 能并排的数：同样的 Phalcon 应用、同样的 CPU 和内存上限，吞吐和延迟落在同一档。做不到就继续改这条路径，而不是退回 FPM。

## 现在的 FrankenPHP 差在哪

`8.5-phalcon-5.23-frankenphp` 基于 `dunglas/frankenphp:1.13.3-php8.5-alpine`。PHP 是 ZTS（`no-debug-zts-20250925`），Phalcon 5.23 在这套 PHP 里另编。NTS 的 FPM 扩展不能拿过来用。

它已经是嵌入加 worker 的模型，所以性能方向是对的。不继续用它，是因为 HTTP 服务器是 Caddy，和 BaseHub 拧着：

- 健康检查打 `127.0.0.1:2019/metrics`，Caddyfile 又是 `admin off`，容器一直 unhealthy。
- `encode zstd br gzip` 让应用容器自己压缩。客户端带了 `Accept-Encoding` 就出 `Content-Encoding: zstd`。
- 自动证书、管理端口、Go 运行时都在这个进程里。入口已经是 Octoproxy，这些用不上，还占内存。

nginx + FPM 那套能跑现在的 Phalcon 网站，但是每个请求都要重新进入前端控制器，FastCGI 也还在。那不是这条产品的目标。

## 决定

一个 Rust 进程同时是 HTTP 服务器和 PHP 运行时。链接 ZTS 的 libphp，用嵌入式 SAPI。每个工作线程跑一份 worker 脚本：启动一次，然后循环接受请求。

```text
客户端 → Octoproxy → phphub:80
                       ├─ Rust：解析 HTTP、静态文件、超时、指标
                       └─ N 条 ZTS 线程，每条一份热的 PHP worker
```

工作线程数是 `WORKERS`。没写时按 cgroup 的 CPU 配额向上取整，至少 2。没有 8 这种上限，避免把大机器压成小池子。容器只有 0.2 个 CPU 时就是 2，和实验室里 FrankenPHP 的 `max_threads=2` 同一档。

选择嵌入，是因为这才和 FrankenPHP 同一条性能路径。FPM 再快也多一次进程间通信，应用也无法在请求之间留着已经建好的 Phalcon 对象。

## Worker

文档根下的入口默认是 `SERVER_ROOT/index.php`，可用 `WORKER_SCRIPT` 改成仓库里的另一个文件。脚本必须位于 `/var/www/html` 之内。

脚本自己循环。运行时提供两个函数：

| 函数 | 作用 |
|---|---|
| `phphub_handle_request()` | 堵住，直到下一个请求到来。返回 `false` 表示服务器正在退出。调用返回后，`$_GET`、`$_POST`、`$_COOKIE`、`$_FILES`、`$_SERVER` 已经是这一次请求的值。 |
| `phphub_finish_request()` | 把这次响应发完，并清掉这次请求的输出缓冲。不销毁 worker 里已经建好的应用对象。 |

Phalcon 应用长这样：

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

应用对象、数据库连接和 opcache 留在循环外面。每次请求必须重新建立只属于这一次的状态：路由匹配、dispatcher、视图、这一次的会话。上一个请求的事务要在 `phphub_finish_request()` 之前结束或回滚。这是 worker 模式的约定，FrankenPHP 也一样。PHPHub 不会在两次请求之间偷偷重建 `$app`。

`$_FILES` 的临时文件在 `phphub_finish_request()` 时删掉。上传大小受 `BODY_LIMIT` 限制，默认 32m。

部署新代码不用重启容器：`docker kill -s USR2 <容器>`。监听端口不关，每条线程做完手上的请求后重启自己的 PHP（空闲的立即重启），同一时刻最多四分之一的线程在重启，其余线程照常接请求，新请求在队列里等，不会被拒。重载期间再发信号会合并成一次。依赖 `opcache.validate_timestamps=1`；同步完文件后要等过 `opcache.revalidate_freq` 秒再发信号。`index`、`file` 模式下只记一条日志。

worker 循环里，一条线程处理满 `MAX_REQUESTS`（默认 1000）之后，只重启这一条线程上的 PHP，其它线程继续接请求。线程 `i` 的上限是 `MAX_REQUESTS + i`。`0` 表示不重启。这是为了拦住慢慢涨的内存，不是退回每个请求启动一次。`index` 和 `file` 会读到这个环境变量，但每个请求单独跑完，不按它回收。`PHP_TIMEOUT` 三种模式都生效。

附加的 `php.ini` 放在 `/usr/local/etc/php/conf.d`，发布镜像没有主 `php.ini`。PHPHub 读完这个目录之后，固定写下 `expose_php=0`、`display_errors`、`zlib.output_compression=0`，并用 `BODY_LIMIT`（默认 32m）盖过 `post_max_size` 和 `upload_max_filesize`。这份附加文件可以改 `memory_limit`、`date.timezone`、`session.save_handler`、`session.save_path`，以及默认的 `max_execution_time` 和 opcache 三项。`php -i` 不经过这次覆盖，仍可能显示上传 2M、post 8M。BaseHub 把应用的附加配置只读挂到 `zz-basehub.ini`。

## 和未改过的前端控制器

现有 `index.php` 很多是跑完一次就退出的。这种文件不用先改就能跑：线程发现脚本没有调用 `phphub_handle_request()`，就在已经热的解释器里按次包含它，请求结束走 PHP 的请求回收，解释器和 opcache 不拆。

这比 FPM 少一次进程跳转，也比冷启动快，但 Phalcon 应用每次都会重建。它是迁到上面的循环之前的跑法，不拿来做对标。对标只用 worker 循环。

## HTTP

- 听 `0.0.0.0:80`。只有 HTTP/1.1，带 keepalive。
- 文档根是 `SERVER_ROOT`，默认 `/var/www/html`。必须是绝对路径，只能含字母、数字、`/`、`_`、`.`、`-`，不能含 `..`，并且落在 `/var/www/html` 里面。不满足就拒绝启动。
- 整个仓库由 BaseHub 挂在 `/var/www/html`。文档根是 `frontend/public` 时，worker 仍然能 `include` 到 `../../vendor`。
- 存在的非 PHP 文件由 Rust 用 sendfile 发送，不占用工作线程。
- 点开头的路径返回 403 空正文。`.env` 和 `.git` 不能被取走，也不能被当成 PHP 执行。
- 等一个请求的时间是 `PHP_TIMEOUT`，默认 60 秒。超时关掉这次请求并回收那条线程。
- 不压缩。压缩在 Octoproxy。这里再压会和入口的 `compress` 叠在一起。
- 不发 `Server`，不发 `X-Powered-By`。`expose_php=Off`。
- 不听 443，不读证书。
- `X-Forwarded-For`、`X-Forwarded-Proto`、`X-Real-Ip` 原样放进 `$_SERVER`。PHPHub 不靠它们做访问控制。

PHP 的报错不写进响应。`display_errors=Off`，`log_errors=On`。`DISPLAY_ERRORS=1` 才在页面上显示，默认关。

## 线程和崩溃

每条工作线程有自己的 TSRM 上下文，不共享请求数据。Phalcon 扩展必须是同一套 ZTS 构建，和现在的 FrankenPHP 镜像一样。

扩展如果段错误，整个进程会退出。这一点和 FrankenPHP 相同。进程退出由 Docker 重启。线程里的致命错误、超时和内存上限只回收那一条线程。

opcache 打开。`validate_timestamps=1`，`revalidate_freq=2`，这样 BaseHub 同步文件之后不用重建容器。确认要更少的 stat 时再关，关了就要等线程重启才看得到新代码。

## 镜像

运行镜像里要有 ZTS 的 libphp、和 `images/frankenphp` 同一套扩展（Phalcon 5.23、redis、memcached、yaml、apcu），以及静态链接的 `phphub`。不要把 Caddy 带进来，也不要 `FROM dunglas/frankenphp` 之后只是换掉 Caddyfile。

不在 2 GiB 的 basehub 虚拟机上编 Phalcon。

## BaseHub

一个 PHP 项目仍然是一个应用。web 和 admin 是两个容器，`SERVER_ROOT` 不同，仓库挂载是同一份。

| 项 | 约定 |
|---|---|
| 路由 | Octoproxy。PHPHub 不读路由文件。 |
| 端口 | `80` |
| 指标 | `METRICS_PORT`，默认 `9100`。`/metrics` 和 `/health` 由 Rust 回答，不进 PHP。进程的指标端口填同一个数。 |
| 健康检查 | 容器内 `http://127.0.0.1:$METRICS_PORT/health`。至少一条工作线程能接请求才是 200，否则 503。不再使用 2019。 |
| 日志 | 访问日志不打。错误日志默认开，慢日志默认关。都写到标准错误，不写文件。 |
| 模式 | `PHPHUB_MODE`：`worker`、`index`、`file`。空着保持现在的自动判断。规格在 [modes.md](modes.md)。 |

停容器时先不接受新连接，等在途请求到 `PHP_TIMEOUT`，再让循环里的 `phphub_handle_request()` 返回 `false`。

BaseHub 按进程写入 `PHPHUB_MODE`。当前镜像会读它。`PHP_TIMEOUT`、`MAX_REQUESTS` 和 `BODY_LIMIT` 写在同一份进程环境里。

## 指标

FrankenPHP 的指标挂在 Caddy 管理端口上。要在 Caddyfile 里打开 `metrics`，默认地址是 `127.0.0.1:2019/metrics`。管理端口关掉时，这个地址不存在。实验室里的健康检查因此一直失败。

PHPHub 不打开那个端口。同一类数字放在 `METRICS_PORT` 的 `/metrics`，默认 `9100`。文本是 Prometheus 格式。抓取不进 PHP。里面没有环境变量、脚本路径以外的本地路径、密钥。

名字用 `phphub_` 前缀。含义和 FrankenPHP 的 `frankenphp_` 指标对齐，方便照它的面板看。`worker` 标签是 worker 脚本路径。按次跑的旧脚本用 `classic`。

| 指标 | 类型 | 含义 |
|---|---|---|
| `phphub_total_threads` | gauge | PHP 线程总数。 |
| `phphub_busy_threads` | gauge | 正在处理请求的 PHP 线程数。停在 `phphub_handle_request()` 里等下一个请求的线程算空闲。 |
| `phphub_queue_depth` | gauge | 还没有分到线程的请求数。 |
| `phphub_total_workers{worker}` | gauge | 这个 worker 的线程数。 |
| `phphub_busy_workers{worker}` | gauge | 这个 worker 里正在处理请求的线程数。 |
| `phphub_ready_workers{worker}` | gauge | 已经进入过 `phphub_handle_request()` 的线程数。脚本还在启动时不算。 |
| `phphub_worker_request_count{worker}` | counter | 这个 worker 处理完的请求数。 |
| `phphub_worker_request_time_seconds{worker}` | counter | 这个 worker 花在请求上的秒数。 |
| `phphub_request_duration_seconds{worker}` | histogram | 请求从到达到结束的耗时（含排队），桶 5ms–10s，用来算 p50/p95/p99：`histogram_quantile(0.95, sum by (le) (rate(phphub_request_duration_seconds_bucket[5m])))`。 |
| `phphub_worker_queue_depth{worker}` | gauge | 排在这个 worker 上的请求数。 |
| `phphub_worker_restarts{worker}` | counter | 主动重启线程的次数。打满 `MAX_REQUESTS` 和 `SIGUSR2` 重载都算在这里。 |
| `phphub_worker_crashes{worker}` | counter | 线程意外退出的次数。超时和致命错误算在这里。 |
| `phphub_opcache_restarts{reason}` | counter | opcache 共享内存重启次数。`reason` 是 `oom`、`hash` 或 `manual`。PHP 8.5 有这个数。读不到就不出这条。 |

另外三条是入口那边没有的，给这个进程自己用：

| 指标 | 类型 | 含义 |
|---|---|---|
| `phphub_http_requests{status}` | counter | 按状态码计的 HTTP 响应。静态文件也算。 |
| `phphub_static_requests` | counter | 没进 PHP 的静态文件请求数。 |
| `phphub_rejected_dotfiles` | counter | 点开头路径被拒绝的次数。 |

`/health` 和 `/metrics` 在同一个端口。`/health` 只回答 `ok` 或 `unavailable`，不带这些数字。至少一条线程已经 ready，才是 200。

## 错误日志和慢日志

这两样都写到标准错误，一行一条，由容器的 syslog 收走。不另开日志文件，也不打访问日志。

错误日志默认开，对应 `log_errors=On`。PHP 的警告、致命错误，以及 `error_log()`，都进这一条。页面上仍然不显示，除非 `DISPLAY_ERRORS=1`。成功的请求不写这条。线程超时、崩溃和主动重启也各写一行，带上脚本和耗时。

慢日志默认关，`SLOWLOG=0`。设成秒数之后，一次请求的耗时达到这个值，才在结束时写一行：方法、路径、脚本、耗时、状态码。不在请求进行中打断它，也不做 php-fpm 那种当时的栈转储。挂死的请求由 `PHP_TIMEOUT` 收掉，算在错误日志里，不靠慢日志。

FrankenPHP 没有 php-fpm 的 `slowlog`。它有的是错误日志：`log_errors` 和 `error_log()` 进 Caddy 的日志，另外可以用 `frankenphp_log()` 打带级别的结构化日志。Caddy 自己的访问日志默认不开。官方性能说明里写了，日志要做分配和 I/O，开得细会拉低吞吐，所以只保留必要的级别。排队等待有 `max_wait_time`，默认关闭，那不是慢日志。

对快路径的影响：

| | 默认 | 成功请求 |
|---|---|---|
| 错误日志 | 开 | 不写。只有真的产生 PHP 错误才写，成本在那一次错误上。 |
| 慢日志 | 关 | 不做。打开之后每个请求只多一次结束时的时间比较；超过阈值才写一行。 |
| 访问日志 | 关 | 不做。入口已经在打。 |

PHPHub 第一版不提供 `frankenphp_log()` 那种带上下文字段的结构化接口。现有的 `error_log()` 够用。

## 先不做

- 用 php-fpm 或 FastCGI 当默认路径。
- 为了省事把每个请求都当成冷的前端控制器。那只作为没改过的脚本的跑法。
- Caddy、管理端口、自动证书、Mercure、HTTP/3。
- 在应用容器里压缩。
- 改 Octoproxy 或 BaseHub 控制面。
- 把 NTS 的 FPM 扩展拷进这个进程。

## 第一版怎么算做完

用同一个 Phalcon 应用，写成上面的 worker 循环。

- 和 FrankenPHP worker 在同样的 CPU、内存上限下并排压测，吞吐和延迟同一档。数字写在这次压测的记录里。
- 循环外面创建的 Phalcon 应用对象在 1000 次请求之后仍是同一个。请求范围内的数据不会留到下一次。
- 静态文件不进 PHP。`/.env` 是 403。
- 响应没有 `Server` 和 `X-Powered-By`。带 `Accept-Encoding: zstd` 时 PHPHub 不加 `Content-Encoding`。
- 一条线程超时或打满 `MAX_REQUESTS` 时，其它线程继续 200。
- `/health` 和 `/metrics` 在 `METRICS_PORT` 上，不访问 2019。`/metrics` 里能看到上表的线程数、排队数和 worker 请求数。容器没有 Caddy，没有 9000，没有 443。
- 一次 PHP 警告会出现在标准错误。`SLOWLOG=0` 时慢请求没有额外的行。设成 `1` 之后，超过 1 秒的请求才有一行慢日志。
- 没改成循环的旧 `index.php` 仍能按次跑通，并且不拿这组数字去和对标结果混在一起。
