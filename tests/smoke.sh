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
    docker rm -f s-worker s-slow s-classic s-client s-mworker s-mworker-classic s-index s-file s-fpm \
        s-realip s-realip-cf s-realip-other s-recycle s-reload s-reload-index s-timeout s-badcidr s-badhdr s-badtimeout s-init s-tini s-exit >/dev/null 2>&1
    docker volume rm -f phphub-smoke-app >/dev/null 2>&1
    docker network rm $NET >/dev/null 2>&1
}
trap cleanup EXIT
cleanup

docker network create $NET >/dev/null
SUBNET=$(docker network inspect $NET -f '{{(index .IPAM.Config 0).Subnet}}')
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
run s-slow -e WORKERS=2 -e SLOWLOG=1 -e PHP_TIMEOUT=3
run s-classic -e WORKERS=2 -e WORKER_SCRIPT=classic.php
run s-mworker -e WORKERS=2 -e PHPHUB_MODE=worker
run s-mworker-classic -e WORKERS=2 -e PHPHUB_MODE=' Worker ' -e WORKER_SCRIPT=classic.php
run s-index -e WORKERS=2 -e PHPHUB_MODE=index
run s-file -e WORKERS=2 -e PHPHUB_MODE=file -e WORKER_SCRIPT=classic.php
run s-fpm -e PHPHUB_MODE=fpm
run s-realip -e WORKERS=1 -e TRUSTED_PROXIES="192.0.2.0/24, $SUBNET"
run s-realip-cf -e WORKERS=1 -e TRUSTED_PROXIES="$SUBNET" -e REAL_IP_HEADER=cf-connecting-ip
run s-realip-other -e WORKERS=1 -e TRUSTED_PROXIES=192.0.2.0/24
run s-recycle -e WORKERS=3 -e MAX_REQUESTS=5
run s-reload -e WORKERS=4 -e PHPHUB_MODE=worker -e MAX_REQUESTS=0
run s-reload-index -e WORKERS=2 -e PHPHUB_MODE=index
run s-timeout -e WORKERS=1 -e PHP_TIMEOUT=2
run s-badcidr -e TRUSTED_PROXIES='10.0.0.0/8,10.0.0.0/33'
run s-badhdr -e TRUSTED_PROXIES=10.0.0.0/8 -e REAL_IP_HEADER=X-Forwarded-For
run s-badtimeout -e PHP_TIMEOUT=0.5
run s-init -e WORKERS=4
run s-tini -e WORKERS=1 --init
run s-exit -e WORKERS=1 -e PHPHUB_MODE=index -e WORKER_SCRIPT=classic.php -e SLOWLOG=1
docker run -d --name s-client --network $NET alpine:3.24 sleep infinity >/dev/null
docker exec s-client apk add --no-cache curl >/dev/null
c() { docker exec s-client curl -s "$@"; }
code() { docker exec s-client curl -s -o /dev/null -w '%{http_code}' "$@"; }

for _ in $(seq 1 50); do
    ok=1
    for h in s-worker s-classic s-mworker s-index s-file s-realip s-realip-cf s-realip-other s-recycle s-reload s-reload-index s-timeout s-init s-tini s-exit; do
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

echo "== visitor address"
CLIENT_IP=$(docker inspect s-client -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}")
ra() { c "$@" | sed 's/.*"remote_addr":"\([^"]*\)".*/\1/'; }
check "untrusted: X-Real-Ip ignored" "$(ra -H 'X-Real-Ip: 203.0.113.9' http://s-worker/)" "$CLIENT_IP"
check "untrusted: X-Real-Ip still in \$_SERVER" "$(c -H 'X-Real-Ip: 203.0.113.9' http://s-worker/ | grep -c '"xri":"203.0.113.9"')" 1
check "other proxy list: X-Real-Ip ignored" "$(ra -H 'X-Real-Ip: 203.0.113.9' http://s-realip-other/)" "$CLIENT_IP"
check "trusted: X-Real-Ip is REMOTE_ADDR" "$(ra -H 'X-Real-Ip: 203.0.113.9' http://s-realip/)" 203.0.113.9
check "trusted: header kept in \$_SERVER" "$(c -H 'X-Real-Ip: 203.0.113.9' http://s-realip/ | grep -c '"xri":"203.0.113.9"')" 1
check "trusted: IPv6 value" "$(ra -H 'X-Real-Ip: 2001:db8::7' http://s-realip/)" "2001:db8::7"
check "trusted: header name is case-insensitive" "$(ra -H 'x-real-ip: 203.0.113.10' http://s-realip/)" 203.0.113.10
check "trusted: next request without header is the peer" "$(ra http://s-realip/)" "$CLIENT_IP"
check "trusted: list is not adopted" "$(ra -H 'X-Real-Ip: 203.0.113.9, 198.51.100.1' http://s-realip/)" "$CLIENT_IP"
check "trusted: port is not adopted" "$(ra -H 'X-Real-Ip: 203.0.113.9:80' http://s-realip/)" "$CLIENT_IP"
check "trusted: junk is not adopted" "$(ra -H 'X-Real-Ip: unknown' http://s-realip/)" "$CLIENT_IP"
check "trusted: empty is not adopted" "$(ra -H 'X-Real-Ip;' http://s-realip/)" "$CLIENT_IP"
check "trusted: repeated header is not adopted" "$(ra -H 'X-Real-Ip: 203.0.113.9' -H 'X-Real-Ip: 203.0.113.9' http://s-realip/)" "$CLIENT_IP"
check "trusted: X-Forwarded-For is not read" "$(ra -H 'X-Forwarded-For: 203.0.113.9' http://s-realip/)" "$CLIENT_IP"
check "trusted: bad values still 200" "$(code -H 'X-Real-Ip: 1.2.3.4:5' http://s-realip/)" 200
sleep 0.2
check "bad header value logged once" "$(docker logs s-realip 2>&1 | grep -c 'not a single IP')" 1
check "startup line names the header" "$(docker logs s-realip 2>&1 | grep -c 'REMOTE_ADDR comes from X-Real-Ip for TRUSTED_PROXIES (2 networks)')" 1
check "CF-Connecting-IP adopted" "$(ra -H 'CF-Connecting-IP: 198.51.100.4' -H 'X-Real-Ip: 203.0.113.9' http://s-realip-cf/)" 198.51.100.4
check "CF: X-Real-Ip ignored" "$(ra -H 'X-Real-Ip: 203.0.113.9' http://s-realip-cf/)" "$CLIENT_IP"

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

echo "== CGI Status header"
U=http://s-worker/cgi-status
check "Status: 302 Found sets the status" "$(code "$U?s=302%20Found&loc=/x")" 302
check "Status: Location still sent" "$(c -si "$U?s=302%20Found&loc=/x" | grep -ci '^location: /x')" 1
check "Status: header not sent to the client" "$(c -si "$U?s=302%20Found&loc=/x" | grep -ci '^status:')" 0
check "Status: 404 with a body" "$(code "$U?s=404") $(c "$U?s=404")" "404 cgi"
check "status: any case" "$(code "$U?name=status&s=201")" 201
check "Status beats http_response_code()" "$(code "$U?code=500&s=200%20OK")" 200
check "Status beats Location's 302" "$(code "$U?s=301&loc=/y")" 301
check "Status while streaming" "$(c -s -o /dev/null -w '%{http_code}' "$U?s=503&flush=1") $(c "$U?s=503&flush=1" | head -1)" "503 part1"
check "invalid Status is 500" "$(code "$U?s=abc") $(code "$U?s=600")" "500 500"
check "invalid Status logged once" "$(docker logs s-worker 2>&1 | grep -c 'PHP sent an invalid Status header: \"abc\"; answered 500 (logged once)') $(docker logs s-worker 2>&1 | grep -c 'invalid Status header')" "1 1"

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
check "timeout is one log line" "$(docker logs s-worker 2>&1 | grep -c -e 'request timed out' -e 'Maximum execution time')" 1
check "timeout line has method, path, script, time" "$(docker logs s-worker 2>&1 | grep -c 'request timed out (PHP_TIMEOUT=3s) in thread [01]: method=GET path=/spin script=/var/www/html/public/index.php time=3.0')" 1
for i in $(seq 1 60); do code http://s-recycle/ >/dev/null; done
sleep 0.2
lines=$(docker logs s-recycle 2>&1 | grep 'restarted after')
check "recycle: some restarts" "$(echo "$lines" | grep -c . | awk '{print ($1 >= 3)}')" 1
check "recycle: thread i restarts after MAX_REQUESTS+i" "$(echo "$lines" | sed -n 's/.*thread \([0-9]*\) restarted after \([0-9]*\) requests.*/\1 \2/p' | awk '$2 != 5 + $1' | wc -l | tr -d ' ')" 0
check "recycle: line names the script" "$(echo "$lines" | grep -vc 'script=/var/www/html/public/index.php')" 0
check "recycle: counted in phphub_worker_restarts" "$(c http://s-recycle:9100/metrics | awk '/^phphub_worker_restarts/ {print $2}')" "$(echo "$lines" | grep -c .)"

echo "== reload (SIGUSR2)"
boots() { for i in $(seq 1 40); do c "http://s-reload/?i=$i"; echo; done | sed 's/.*"booted_at":\([0-9.]*\).*/\1/' | sort -u; }
before=$(boots)
docker exec s-reload sh -c 'echo v2 > /tmp/phphub-version'
# Load across the reload: 4 clients x 40 requests, every 4th 200ms long so threads are busy.
docker exec s-client sh -c 'for k in 1 2 3 4; do (for i in $(seq 1 40); do
    if [ $((i % 4)) = 0 ]; then u="http://s-reload/sleep?ms=200"; else u="http://s-reload/?k=$k&i=$i"; fi
    curl -s -o /dev/null -w "%{http_code}\n" "$u"; done > /tmp/reload-$k.txt) & done; wait' &
load=$!
sleep 0.5
docker kill -s USR2 s-reload >/dev/null
wait $load
check "reload: every request during the reload is 200" "$(docker exec s-client sh -c 'cat /tmp/reload-*.txt' | grep -vc '^200$')" 0
check "reload: 160 requests answered" "$(docker exec s-client sh -c 'cat /tmp/reload-*.txt' | grep -c '^200$')" 160
for _ in $(seq 1 50); do docker logs s-reload 2>&1 | grep -q 'reload 1 done' && break; sleep 0.1; done
after=$(boots)
check "reload: no thread kept its old boot" "$(printf '%s\n%s\n' "$before" "$after" | sort | uniq -d | wc -l | tr -d ' ')" 0
check "reload: new runs see the new code" "$(for i in $(seq 1 20); do c http://s-reload/; echo; done | grep -vc '"version":"v2"')" 0
rl=$(docker logs s-reload 2>&1)
check "reload: announced with the cap" "$(echo "$rl" | grep -c 'reload 1: restarting 4 PHP threads, at most 1 at a time')" 1
check "reload: each thread reloaded once" "$(echo "$rl" | grep -c 'reloaded after .* (SIGUSR2)')" 4
check "reload: done line" "$(echo "$rl" | grep -c 'reload 1 done in')" 1
check "reload: counted in phphub_worker_restarts" "$(c http://s-reload:9100/metrics | awk '/^phphub_worker_restarts/ {print $2}')" 4
check "reload: not a crash" "$(c http://s-reload:9100/metrics | awk '/^phphub_worker_crashes/ {print $2}')" 0
docker kill -s USR2 s-reload-index >/dev/null
sleep 0.2
check "reload: index mode logs and keeps serving" "$(docker logs s-reload-index 2>&1 | grep -c 'reload: not in worker mode'):$(code http://s-reload-index/)" "1:200"

echo "== timeouts in queue, in the body, while streaming, during upload"
check "busy-after-finish answers" "$(c 'http://s-timeout/after?ms=3500')" answered
check "queued request gets 504" "$(code 'http://s-timeout/?queued=1')" 504
sleep 0.2
check "queued timeout logged in queue" "$(docker logs s-timeout 2>&1 | grep -c 'timed out (PHP_TIMEOUT=2s) in queue: method=GET path=/')" 1
sleep 1.5
check "thread answers after the queue timeout" "$(code http://s-timeout/)" 200
check "queued request never ran" "$(c http://s-timeout:9100/metrics | awk '/^phphub_worker_request_count/ {print $2}')" 2
check "slow body gets 504" "$(docker exec s-client sh -c "(printf 'POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\nabc'; sleep 3) | nc s-timeout 80 | head -1 | tr -d '\r'")" "HTTP/1.1 504 Gateway Timeout"
check "slow body timeout logged" "$(docker logs s-timeout 2>&1 | grep -c 'timed out (PHP_TIMEOUT=2s) reading the body: method=POST path=/')" 1
out=$(docker exec s-client sh -c 'curl -s -w "|%{http_code}" http://s-timeout/stream-spin; echo "|$?"')
check "streaming timeout keeps the 200 and cuts the body" "$out" "part1
|200|18"
docker exec s-client sh -c 'head -c 200000 /dev/urandom > /tmp/up.bin'
check "upload that spins gets 504" "$(code -F f=@/tmp/up.bin http://s-timeout/upload-spin)" 504
sleep 0.3
tmp=$(docker exec s-timeout cat /tmp/last-upload)
check "upload temp file was there" "$(echo "$tmp" | grep -c '^/tmp/php')" 1
check "upload temp file removed after timeout" "$(docker exec s-timeout sh -c "[ -e $tmp ] && echo left || echo gone")" gone
check "timeouts are not process exits" "$(docker inspect s-timeout --format '{{.State.Running}} {{.RestartCount}}')" "true 0"

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
check "SLOWLOG: timeout is 504" "$(code http://s-slow/spin)" 504
sleep 0.3
check "SLOWLOG: timeouts write no slow line" "$(docker logs s-slow 2>&1 | grep -c 'slow request')" 1

echo "== classic front controller"
check "classic script runs per request" "$(c 'http://s-classic/?a=1' | grep -c '"classic":true,"counter":1')" 1
check "classic again starts fresh" "$(c 'http://s-classic/?a=2' | grep -c '"counter":1')" 1
check "classic metrics label" "$(c http://s-classic:9100/metrics | grep -c 'worker="classic"')" 9

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

echo "== exit() and slow requests in index mode"
crashes() { c http://s-exit:9100/metrics | awk '/^phphub_worker_crashes/ {print $2}'; }
check "exit: response sent" "$(code 'http://s-exit/?exit=0') $(c 'http://s-exit/?exit=0')" "200 bye"
c 'http://s-exit/?exit=3' >/dev/null
check "exit() is not a crash" "$(crashes)" 0
check "uncaught exception is 500" "$(code 'http://s-exit/?throw=1')" 500
check "uncaught exception is a crash" "$(crashes)" 1
c 'http://s-exit/?sleep=1100&exit=0' >/dev/null
check "SLOWLOG covers requests that call exit()" "$(docker logs s-exit 2>&1 | grep -c 'slow request method=GET path=/ .* queue=0.0[0-9]*s php=1.[0-9]*s status=200')" 1
c 'http://s-exit/?sleep=1100' >/dev/null & sleep 0.2; c 'http://s-exit/?sleep=300' >/dev/null; wait
check "SLOWLOG shows time spent waiting for a thread" "$(docker logs s-exit 2>&1 | grep -c 'queue=0.[89][0-9]*s php=0.3[0-9]*s')" 1
check "queue time metric" "$(c http://s-exit:9100/metrics | awk '/^phphub_worker_queue_time_seconds/ {print ($2 >= 0.8)}')" 1

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

echo "== settings that refuse to start"
check "bad CIDR: exited" "$(docker inspect s-badcidr --format '{{.State.Running}} {{.State.ExitCode}}')" "false 1"
check "bad CIDR: names the entry" "$(docker logs s-badcidr 2>&1 | grep -c 'refusing to start: TRUSTED_PROXIES has an invalid CIDR: 10.0.0.0/33')" 1
check "bad REAL_IP_HEADER: exited" "$(docker inspect s-badhdr --format '{{.State.Running}} {{.State.ExitCode}}')" "false 1"
check "bad REAL_IP_HEADER: error line" "$(docker logs s-badhdr 2>&1 | grep -c 'REAL_IP_HEADER must be X-Real-Ip or CF-Connecting-IP: X-Forwarded-For')" 1
check "PHP_TIMEOUT=0.5: exited" "$(docker inspect s-badtimeout --format '{{.State.Running}} {{.State.ExitCode}}')" "false 1"
check "PHP_TIMEOUT=0.5: error line" "$(docker logs s-badtimeout 2>&1 | grep -c 'PHP_TIMEOUT must be a whole number of seconds, at least 1')" 1

echo "== PID 1 reaps exited processes"
zombies() { docker exec "$1" sh -c 'cat /proc/[0-9]*/stat 2>/dev/null' | grep -c ') Z '; }
check "PID 1 is the reaper" "$(docker logs s-init 2>&1 | grep -c 'running as PID 1: forwarding signals')" 1
check "server runs as a child of PID 1" "$(docker exec s-init sh -c 'ps -o pid,ppid,comm' | awk '$2==1 && $3=="phphub"' | wc -l | tr -d ' ')" 1
for i in $(seq 1 20); do c http://s-init/orphan >/dev/null; done
sleep 1
check "orphans are reaped" "$(zombies s-init)" 0
check "PHP keeps its children's exit codes" "$(c http://s-init/exitcodes)" "7 3 5"
mix=$(for i in $(seq 1 20); do c http://s-init/orphan >/dev/null & done
    for i in $(seq 1 16); do c http://s-init/exitcodes & done; wait)
check "exit codes with concurrent requests and orphans" "$(echo "$mix" | sort -u)" "7 3 5"
sleep 1
check "no zombies after the mix" "$(zombies s-init)" 0
check "--init: tini is PID 1, no second reaper" "$(docker logs s-tini 2>&1 | grep -c 'running as PID 1')" 0
check "--init: exit codes" "$(c http://s-tini/exitcodes)" "7 3 5"
check "command exit code passes through PID 1" "$(docker run --rm "$IMAGE" sh -c 'exit 3' >/dev/null 2>&1; echo $?)" 3
check "command killed by a signal is 128+n" "$(docker run --rm "$IMAGE" sh -c 'kill -9 $$' >/dev/null 2>&1; echo $?)" 137
check "commands print no PID 1 line" "$(docker run --rm "$IMAGE" true 2>&1 | wc -c | tr -d ' ')" 0
check "orphans of a command are reaped" "$(docker run --rm "$IMAGE" sh -c 'sh -c "sleep 0.2 &"; sleep 1; cat /proc/[0-9]*/stat' 2>/dev/null | grep -c ') Z ')" 0
docker kill -s INT s-init >/dev/null
sleep 1.5
check "SIGINT: graceful stop through PID 1" "$(docker logs s-init 2>&1 | grep -c 'phphub: stopped')" 1
check "SIGINT: clean exit" "$(docker inspect s-init --format '{{.State.Running}} {{.State.ExitCode}}')" "false 0"

echo "== other commands in the image"
check "php CLI runs through the entrypoint" "$(docker run --rm "$IMAGE" php -r 'echo PHP_ZTS ? "zts" : "nts";')" zts
check "crond runs through the entrypoint" "$(docker run --rm "$IMAGE" sh -c 'timeout 1 crond -f -l 2; echo $?')" 143
check "unknown command is 127" "$(docker run --rm "$IMAGE" no-such-cmd >/dev/null 2>&1; echo $?)" 127
check "phphub runs on mimalloc" "$(docker run --rm -e MIMALLOC_VERBOSE=1 --entrypoint phphub "$IMAGE" health 2>&1 | grep -c '^mimalloc: v')" 1
check "php CLI keeps musl malloc" "$(docker run --rm -e MIMALLOC_VERBOSE=1 --entrypoint php "$IMAGE" -r 'echo 1;' 2>&1 | grep -c '^mimalloc')" 0
check "image has the app extensions" "$(docker run --rm "$IMAGE" php -m | grep -ciE '^(phalcon|redis|memcached|mysqli)$')" 4

echo "== graceful stop"
docker exec -d s-client sh -c 'curl -s -o /dev/null -w "%{http_code}" "http://s-slow/sleep?ms=1500" > /tmp/drain'
sleep 0.3
docker stop -t 20 s-slow >/dev/null
check "in-flight request finishes on stop" "$(docker exec s-client cat /tmp/drain)" 200
check "clean exit" "$(docker inspect s-slow --format '{{.State.ExitCode}}')" 0
check "SIGTERM: drained and stopped through PID 1" "$(docker logs s-slow 2>&1 | grep -c 'phphub: stopped')" 1

echo
if [ "$fails" -eq 0 ]; then echo "all checks passed"; else echo "$fails checks failed"; fi
exit "$fails"
