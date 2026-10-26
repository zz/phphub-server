# 运行模式

PHPHub 按进程启动时的 `PHPHUB_MODE` 固定三种跑法。BaseHub 已经会把这个变量写进容器。这份文档是 PHPHub 还要做的事。

现在的二进制不读 `PHPHUB_MODE`。脚本正文里有 `phphub_handle_request` 就进 worker，否则每个请求重新包含入口脚本，`SCRIPT_FILENAME` 始终是入口。`/info3.php` 因此不会执行 `info3.php`。

## 环境变量

启动时读一次，去掉首尾空白，按小写比较。运行中不热切换。改模式要重启进程。

| `PHPHUB_MODE` | 行为 |
|---|---|
| 未设置，或空字符串 | 保持现在的自动判断 |
| `worker` | 强制 worker。入口脚本启动一次，之后循环接请求 |
| `index` | 每个请求只跑入口脚本一次 |
| `file` | 网址上的 PHP 文件存在就跑它，否则跑入口 |
| 其它值 | 拒绝启动。错误日志一行：`PHPHUB_MODE must be worker, index, or file`。进程以非 0 退出，不要听端口 |

BaseHub 的进程字段 `php_mode` 空着时不写这个变量。三个值原样写入该进程，并覆盖应用环境里的同名变量。前台和后台是两个进程，可以各选各的。Cron 跑 `php console/cli.php` 时不要设，那个进程不是 PHPHub。

入口脚本仍是 `WORKER_SCRIPT`，默认 `SERVER_ROOT/index.php`，规则不变：绝对路径或相对文档根，必须落在 `/var/www/html` 里。

## worker

和现在的 worker 相同。每条线程先执行入口脚本。脚本循环调用 `phphub_handle_request()`，用 `phphub_finish_request()` 结束这一次响应。应用对象留在循环外面。

`SCRIPT_NAME`、`SCRIPT_FILENAME`、`PHP_SELF`、`DOCUMENT_URI` 固定是入口脚本。`REQUEST_URI` 是客户端原始 URI。旁边的 `info3.php` 不会被当成这次的脚本。

脚本一次都没调用 `phphub_handle_request()` 就返回时：记一条错误，这条线程改按 `index` 接后面的请求。不要因此改成 `file`，也不要让整个进程退出。

## index

每个动态请求在已经热的线程上重新包含入口脚本一次，然后做 PHP 的请求回收。解释器和 opcache 留着。Phalcon 应用、数据库连接、视图这些对象不留到下一次。

`SCRIPT_FILENAME` 固定是入口。`REQUEST_URI` 仍是客户端原始 URI。不要写入 `$_GET['_url']`。Phalcon 的 `$application->handle($_SERVER['REQUEST_URI'])` 用的是原始 URI。

不因为脚本里写了循环就升成 worker。`phphub_handle_request()` 在这个模式里只对当前请求返回一次 `true`，下一次调用返回 `false`，这样 `while` 不会把线程占住。脚本返回后，线程等下一个 HTTP 请求，再重新包含入口。

静态文件、点文件 403、PHP 源码不外发，和现在一样。`/info3.php` 跑的仍是入口，不是 `info3.php`。

## file

这是要对齐 FrankenPHP 非 worker（`php_server`，没有 worker 指令）和 nginx 的模式：网址点名的 PHP 文件存在就执行它，否则落到入口。

查找在现有检查之后做。点开头的路径已经是 403。含 `..` 的路径已经拒绝。百分号解码已经完成。

1. 文档根下已有的非 PHP 文件：sendfile，不进 PHP。
2. 目录里的 `index.html`：和现在一样，该补斜杠就补斜杠。
3. 文档根下已有的普通文件，扩展名是 `php`、`phtml`、`phar`、`inc`（大小写不敏感）：这次的脚本就是它。
4. 其余情况，包括不存在的路径、目录、以及没有这些扩展名的路径：脚本是入口，和 `index` 一样。

`/album/123` 这种漂亮网址没有对应文件，所以跑 `index.php`。`/info3.php` 在文件存在时跑 `info3.php`。`/info3.php` 不存在时跑入口，不要由 PHPHub 直接回 404。`/subdir/` 不自动改去执行 `subdir/index.php`；要执行那个文件，请求路径得是 `/subdir/index.php`。

真实路径必须仍在 `SERVER_ROOT` 里。先解析符号链接再判断。链接跳出文档根，就当成文件不存在，落到入口，不要读根外面的文件。

这次选中文件之后：

| 变量 | 值 |
|---|---|
| `SCRIPT_FILENAME` | 选中文件的绝对路径 |
| `SCRIPT_NAME`、`PHP_SELF`、`DOCUMENT_URI` | 站内路径，例如 `/info3.php` |
| `REQUEST_URI` | 客户端原始 URI，不要改成脚本名 |
| `QUERY_STRING` | 原始查询串。不要额外加上 `_url` |
| `PATH_INFO` | 默认空。路径是 `/info3.php/extra` 并且 `info3.php` 存在时，`SCRIPT_NAME` 为 `/info3.php`，`PATH_INFO` 为 `/extra` |

和 `index` 一样，不升成 worker。`phphub_handle_request()` 只服务当前这一次，下一次返回 `false`。请求结束做请求回收，不把这个文件里的对象留到下一个网址。

不要把 PHP 源码当静态文件发出。这条和现在的 `Lookup::Php` 一致，`file` 模式改变的是执行哪个文件，不是能不能把源码下载走。

## 代码要动哪里

- `src/config.rs`：读 `PHPHUB_MODE`。空值保持现在的自动判断。非法值在 `from_env` 里失败。
- `src/php/mod.rs`：环境变量已经指定模式时，不要再扫脚本正文决定 worker 还是 classic。`index` 和 `file` 禁止把进程升成 worker。
- `src/php/job.rs`：`SCRIPT_FILENAME` 等四个变量按这次选中的脚本填，不要再写死 `cfg.worker_script`。`file` 模式每个请求都可能不同。
- `src/statics.rs`：`file` 模式要能把「文档根内的这个 PHP 文件」和「落到入口」分开交给线程。静态文件和点文件的结果不变。

指标标签保持现在的基数。`worker` 模式的 `worker` 标签仍是入口脚本路径。`index` 和 `file` 都用 `classic`，不要按每个 PHP 文件各建一条序列。

三种模式都不做 TLS、压缩、`Server`、`X-Powered-By`。访问日志仍然不打。

## 和现有 Phalcon 站点

`frontend/public/index.php` 和 `backend/public/index.php` 都是跑完一次就退出，并把 `$_SERVER["REQUEST_URI"]` 交给 `$application->handle()`。

| 模式 | 应用代码 |
|---|---|
| `index` | 两个入口都不用改。`info3.php` 不执行。 |
| `file` | 两个入口都不用改。`public/info3.php` 会执行。 |
| `worker` | 两个入口都要把 `new Application($di)` 留在循环外，`handle` 和 `send` 放进 `phphub_handle_request()`。每次请求拆掉 `session`、`flashSession`、`cookies`、`view`、`request` 再重建。数据库、Redis、Sphinx、Memcache、Volt 编译器留在循环外。控制器不用改。 |

这是应用自己的改法。PHPHub 这次只负责按模式选脚本、设 `$_SERVER`，并在非 worker 模式里把循环调用收成一次。

## 验收

- 不设 `PHPHUB_MODE`：现有 worker 脚本仍只启动一次；没有循环的入口仍按次重跑，`SCRIPT_FILENAME` 仍是入口。
- `PHPHUB_MODE=worker`：带循环的 `index.php` 只启动一次，循环外的对象跨请求还在。
- `PHPHUB_MODE=index`：同一个入口每个请求重新跑。`/info3.php` 的正文仍来自入口。`SCRIPT_FILENAME` 是入口。
- `PHPHUB_MODE=file`：`/info3.php` 执行 `info3.php`，`SCRIPT_FILENAME` 是它。`/` 和没有对应文件的路径跑入口。`style.css` 仍是静态响应。`/.env` 仍是 403 空正文。指向文档根外的符号链接不执行。
- `PHPHUB_MODE=fpm`：进程不起端口，错误日志有上面那一行。
