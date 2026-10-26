#!/bin/sh
# Acceptance checks from docs/phphub.md ("第一版怎么算做完"), except the
# benchmark (see bench/run.sh). Runs against an image, default phphub:dev.
#
#   docker build -t phphub:dev . && ./tests/smoke.sh
set -u
cd "$(dirname "$0")"
IMAGE="${1:-phphub:dev}"
NET=phphub-smoke
fails=0

pass() { printf 'PASS  %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }
check() { if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (got '$2', want '$3')"; fi; }

cleanup() {
    docker rm -f s-worker s-slow s-classic s-client s-mworker s-mworker-classic s-index s-file s-fpm >/dev/null 2>&1
    docker volume rm -f phphub-smoke-app >/dev/null 2>&1
    docker network rm $NET >/dev/null 2>&1
}
trap cleanup EXIT
cleanup

docker network create $NET >/dev/null
docker volume create phphub-smoke-app >/dev/null
# .git cannot live in this repo, so the fixture is made here.
docker run --rm -v "$PWD/app":/src:ro -v phphub-smoke-app:/dst alpine:3.24 \
    sh -c 'cp -a /src/. /dst/ && mkdir -p /dst/public/.git && echo "[core]" > /dst/public/.git/config &&
        ln -s /var/www/html/outside.php /dst/public/link.php'

run() {
    name=$1; shift
    docker run -d --name "$name" --network $NET -v phphub-smoke-app:/var/www/html \
        -e SERVER_ROOT=/var/www/html/public "$@" "$IMAGE" >/dev/null
}
run s-worker -e WORKERS=2 -e PHP_TIMEOUT=3 -e MAX_REQUESTS=50
run s-slow -e WORKERS=2 -e SLOWLOG=1
run s-classic -e WORKERS=2 -e WORKER_SCRIPT=classic.php
run s-mworker -e WORKERS=2 -e PHPHUB_MODE=worker
run s-mworker-classic -e WORKERS=2 -e PHPHUB_MODE=' Worker ' -e WORKER_SCRIPT=classic.php
run s-index -e WORKERS=2 -e PHPHUB_MODE=index
run s-file -e WORKERS=2 -e PHPHUB_MODE=file -e WORKER_SCRIPT=classic.php
run s-fpm -e PHPHUB_MODE=fpm
docker run -d --name s-client --network $NET alpine:3.24 sleep infinity >/dev/null
docker exec s-client apk add --no-cache curl >/dev/null
c() { docker exec s-client curl -s "$@"; }
code() { docker exec s-client curl -s -o /dev/null -w '%{http_code}' "$@"; }

for _ in $(seq 1 50); do
    ok=1
    for h in s-worker s-classic s-mworker s-index s-file; do
        [ "$(code http://$h:9100/health)" = 200 ] || ok=0
    done
    [ $ok = 1 ] && break
    sleep 0.2
done

echo "== worker loop"
check "/health is 200 on METRICS_PORT" "$(code http://s-worker:9100/health)" 200
check "/health body" "$(c http://s-worker:9100/health)" ok
first=$(c http://s-worker/ | sed 's/.*"booted_at":\([0-9.]*\).*/\1/')
ids=$(for i in $(seq 1 40); do c "http://s-worker/?i=$i"; echo; done | sed 's/.*"booted_at":\([0-9.]*\).*/\1/' | sort -u | wc -l | tr -d ' ')
check "app object is reused (2 threads -> 2 boots)" "$ids" 2
c -H 'Cookie: a=1' 'http://s-worker/?x=1' -d 'p=1' >/dev/null
check "no leaked GET/POST/COOKIE" "$(c http://s-worker/ | grep -c '"get":\[\],"post":\[\],"cookie":\[\]')" 1
check "header() does not leak" "$(c -si http://s-worker/header >/dev/null; c -si http://s-worker/ | grep -ci 'x-set-once')" 0
check "X-Forwarded-For passed through" "$(c -H 'X-Forwarded-For: 9.9.9.9' http://s-worker/ | grep -c '"xff":"9.9.9.9"')" 1
check "bootstrap output never sent" "$(for i in 1 2 3 4; do c http://s-worker/; done | grep -c bootstrap)" 0

echo "== static and dotfiles"
check "static file 200" "$(code http://s-worker/style.css)" 200
check "static body" "$(c http://s-worker/style.css)" "body { color: red }"
check "/.env is 403" "$(code http://s-worker/.env)" 403
check "/.env body is empty" "$(c http://s-worker/.env | wc -c | tr -d ' ')" 0
check "/.git/config is 403" "$(code http://s-worker/.git/config)" 403
check "encoded dot is 403" "$(code http://s-worker/%2eenv)" 403
check ".php source is not served" "$(c http://s-worker/other.php | grep -c 'should never run')" 0
static_before=$(c http://s-worker:9100/metrics | awk '/^phphub_static_requests/ {print $2}')
php_before=$(c http://s-worker:9100/metrics | awk '/^phphub_worker_request_count/ {print $2}')
c http://s-worker/style.css >/dev/null
check "static files skip PHP" "$(c http://s-worker:9100/metrics | awk '/^phphub_worker_request_count/ {print $2}')" "$php_before"
check "static counter moves" "$(c http://s-worker:9100/metrics | awk '/^phphub_static_requests/ {print $2}')" "$((static_before + 1))"

echo "== response headers"
h=$(c -si -H 'Accept-Encoding: zstd, br, gzip' http://s-worker/)
check "no Server header" "$(echo "$h" | grep -ci '^server:')" 0
check "no X-Powered-By" "$(echo "$h" | grep -ci '^x-powered-by:')" 0
check "no Content-Encoding with zstd asked" "$(echo "$h" | grep -ci '^content-encoding:')" 0
check "static: no Content-Encoding" "$(c -si -H 'Accept-Encoding: zstd' http://s-worker/style.css | grep -ci '^content-encoding:')" 0

echo "== timeouts and restarts"
docker exec -d s-client sh -c 'curl -s -o /dev/null -w "%{http_code}" http://s-worker/spin > /tmp/spin'
sleep 0.5
others=$(for i in 1 2 3 4 5; do code http://s-worker/; echo; done | sort -u | tr -d '\n')
check "other thread answers while one spins" "$others" 200
sleep 3.5
check "spinning request gets 504 at PHP_TIMEOUT" "$(docker exec s-client cat /tmp/spin)" 504
check "fatal error is 500" "$(code http://s-worker/fatal)" 500
codes=$(for i in $(seq 1 120); do code http://s-worker/; echo; done | sort -u | tr -d '\n')
check "MAX_REQUESTS restarts keep answering 200" "$codes" 200
m=$(c http://s-worker:9100/metrics)
check "restarts counted" "$(echo "$m" | awk '/^phphub_worker_restarts/ {print ($2 > 0)}')" 1
check "crashes counted" "$(echo "$m" | awk '/^phphub_worker_crashes/ {print ($2 >= 2)}')" 1

echo "== metrics"
for name in phphub_total_threads phphub_busy_threads phphub_queue_depth phphub_total_workers \
    phphub_busy_workers phphub_ready_workers phphub_worker_request_count phphub_worker_request_time_seconds \
    phphub_worker_queue_depth phphub_opcache_restarts phphub_http_requests phphub_static_requests phphub_rejected_dotfiles; do
    if [ "$(echo "$m" | grep -c "^$name")" -gt 0 ]; then pass "metric $name"; else fail "metric $name missing"; fi
done
check "threads gauge" "$(echo "$m" | awk '/^phphub_total_threads/ {print $2}')" 2
check "no 2019 listener" "$(docker exec s-worker sh -c 'netstat -lnt | grep -c ":2019 "')" 0
check "no 443 listener" "$(docker exec s-worker sh -c 'netstat -lnt | grep -c ":443 "')" 0
check "no 9000 listener" "$(docker exec s-worker sh -c 'netstat -lnt | grep -c ":9000 "')" 0
check "no caddy binary" "$(docker exec s-worker sh -c 'command -v caddy frankenphp | wc -l')" 0

echo "== logs"
c http://s-worker/warn >/dev/null
sleep 0.2
check "PHP warning on stderr" "$(docker logs s-worker 2>&1 >/dev/null | grep -c 'test warning from worker')" 1
c 'http://s-worker/sleep?ms=1200' >/dev/null
check "SLOWLOG=0: no slow line" "$(docker logs s-worker 2>&1 | grep -c 'slow request')" 0
c 'http://s-slow/sleep?ms=300' >/dev/null
c 'http://s-slow/sleep?ms=1100' >/dev/null
sleep 0.2
check "SLOWLOG=1: one slow line, for the 1.1s request" "$(docker logs s-slow 2>&1 | grep -c 'slow request')" 1

echo "== classic front controller"
check "classic script runs per request" "$(c 'http://s-classic/?a=1' | grep -c '"classic":true,"counter":1')" 1
check "classic again starts fresh" "$(c 'http://s-classic/?a=2' | grep -c '"counter":1')" 1
check "classic metrics label" "$(c http://s-classic:9100/metrics | grep -c 'worker="classic"')" 8

echo "== PHPHUB_MODE unset"
check "auto: entry is SCRIPT_FILENAME for /info3.php" "$(c http://s-worker/info3.php | grep -c '"script_filename":"\\/var\\/www\\/html\\/public\\/index.php"')" 1
check "auto: classic entry for /info3.php" "$(c http://s-classic/info3.php | grep -c '"classic":true')" 1

echo "== PHPHUB_MODE=worker"
boots=$(for i in $(seq 1 30); do c http://s-mworker/; echo; done | sed 's/.*"booted_at":\([0-9.]*\).*/\1/' | sort -u | wc -l | tr -d ' ')
check "worker: app object kept (2 threads -> 2 boots)" "$boots" 2
check "worker: /info3.php runs the entry" "$(c http://s-mworker/info3.php | grep -c '"info3"')" 0
check "worker: SCRIPT_NAME is the entry" "$(c http://s-mworker/info3.php | grep -c '"script_name":"\\/index.php"')" 1
check "worker without loop: still answers" "$(code 'http://s-mworker-classic/?a=1')" 200
check "worker without loop: runs per request" "$(c 'http://s-mworker-classic/?a=2' | grep -c '"counter":1')" 1
sleep 0.2
check "worker without loop: error logged" "$(docker logs s-mworker-classic 2>&1 | grep -c 'returned without calling phphub_handle_request() (PHPHUB_MODE=worker)')" 2
check "worker without loop: process keeps running" "$(docker inspect s-mworker-classic --format '{{.State.Running}}')" true

echo "== PHPHUB_MODE=index"
r1=$(c 'http://s-index/?a=1'); r2=$(c 'http://s-index/?a=2')
check "index: loop script served once per run" "$(echo "$r1$r2" | grep -o '"served":1' | wc -l | tr -d ' ')" 2
b1=$(echo "$r1" | sed 's/.*"booted_at":\([0-9.]*\).*/\1/'); b2=$(echo "$r2" | sed 's/.*"booted_at":\([0-9.]*\).*/\1/')
check "index: entry re-runs per request" "$([ "$b1" != "$b2" ] && echo yes)" yes
check "index: /info3.php body comes from the entry" "$(c http://s-index/info3.php | grep -c '"info3"')" 0
check "index: SCRIPT_FILENAME is the entry" "$(c http://s-index/info3.php | grep -c '"script_filename":"\\/var\\/www\\/html\\/public\\/index.php"')" 1
check "index: no _url in GET" "$(c 'http://s-index/x/y?b=1' | grep -c '"get":{"b":"1"}')" 1
check "index: metrics label is classic" "$(c http://s-index:9100/metrics | grep -c 'worker="/var/www/html/public/index.php"')" 0
check "index: never upgraded to worker" "$(docker logs s-index 2>&1 | grep -c 'worker mode')" 0

echo "== PHPHUB_MODE=file"
f=$(c 'http://s-file/info3.php?q=1')
check "file: /info3.php runs info3.php" "$(echo "$f" | grep -c '"info3":true')" 1
check "file: SCRIPT_FILENAME is info3.php" "$(echo "$f" | grep -c '"script_filename":"\\/var\\/www\\/html\\/public\\/info3.php"')" 1
check "file: SCRIPT_NAME" "$(echo "$f" | grep -c '"script_name":"\\/info3.php"')" 1
check "file: REQUEST_URI is the client URI" "$(echo "$f" | grep -c '"request_uri":"\\/info3.php?q=1"')" 1
check "file: QUERY_STRING untouched" "$(echo "$f" | grep -c '"query_string":"q=1","get":{"q":"1"}')" 1
check "file: PATH_INFO empty" "$(echo "$f" | grep -c '"path_info":""')" 1
p=$(c 'http://s-file/info3.php/extra/x')
check "file: PATH_INFO split" "$(echo "$p" | grep -c '"script_name":"\\/info3.php","php_self":"\\/info3.php\\/extra\\/x","document_uri":"\\/info3.php","path_info":"\\/extra\\/x"')" 1
check "file: / runs the entry" "$(c http://s-file/ | grep -c '"classic":true')" 1
check "file: pretty URL runs the entry" "$(c http://s-file/album/123 | grep -c '"classic":true')" 1
check "file: missing .php runs the entry" "$(c http://s-file/nope.php | grep -c '"classic":true')" 1
check "file: missing .php is not a 404" "$(code http://s-file/nope.php)" 200
check "file: dir without index.html runs the entry" "$(c http://s-file/sub2/ | grep -c '"classic":true')" 1
check "file: style.css is static" "$(c http://s-file/style.css)" "body { color: red }"
check "file: /.env is 403" "$(code http://s-file/.env)" 403
check "file: /.env body empty" "$(c http://s-file/.env | wc -c | tr -d ' ')" 0
check "file: symlink outside root does not run" "$(c http://s-file/link.php | grep -c 'outside the document root')" 0
check "file: symlink outside root falls to entry" "$(c http://s-file/link.php | grep -c '"classic":true')" 1
check "file: fatal in the picked file is 500" "$(code http://s-file/fatal.php)" 500
check "file: entry still 200 after a fatal" "$(code http://s-file/)" 200
check "file: .php source never sent" "$(c http://s-file/info3.php | grep -c '<?php')" 0
check "file: metrics label is classic" "$(c http://s-file:9100/metrics | grep -c 'info3')" 0

echo "== PHPHUB_MODE=fpm"
sleep 0.5
check "fpm: process exited" "$(docker inspect s-fpm --format '{{.State.Running}}')" false
check "fpm: non-zero exit" "$(docker inspect s-fpm --format '{{.State.ExitCode}}' | grep -vc '^0$')" 1
check "fpm: error line" "$(docker logs s-fpm 2>&1 | grep -c 'PHPHUB_MODE must be worker, index, or file')" 1
check "fpm: did not listen" "$(docker logs s-fpm 2>&1 | grep -c 'listening')" 0

echo "== graceful stop"
docker exec -d s-client sh -c 'curl -s -o /dev/null -w "%{http_code}" "http://s-slow/sleep?ms=1500" > /tmp/drain'
sleep 0.3
docker stop -t 20 s-slow >/dev/null
check "in-flight request finishes on stop" "$(docker exec s-client cat /tmp/drain)" 200
check "clean exit" "$(docker inspect s-slow --format '{{.State.ExitCode}}')" 0

echo
if [ "$fails" -eq 0 ]; then echo "all checks passed"; else echo "$fails checks failed"; fi
exit "$fails"
