#!/bin/sh
# Stability soak: mixed traffic per mode, then a run with fatal errors mixed
# in. Reports memory before/after, non-2xx counts and whether the process
# stayed up. Needs phphub:dev.
#
#   ./tests/soak.sh            # 60 s per mode
#   SOAK=300s ./tests/soak.sh
set -u
cd "$(dirname "$0")/.."
IMAGE="${IMAGE:-phphub:dev}"
SOAK="${SOAK:-60s}"
NET=phphub-soak
fails=0

cleanup() {
    docker rm -f k-srv k-client >/dev/null 2>&1
    docker volume rm -f phphub-soak-bench phphub-soak-tests >/dev/null 2>&1
    docker network rm $NET >/dev/null 2>&1
}
trap cleanup EXIT
cleanup
docker network create $NET >/dev/null
docker volume create phphub-soak-bench >/dev/null
docker volume create phphub-soak-tests >/dev/null
docker run --rm -v "$PWD/bench/app":/src:ro -v phphub-soak-bench:/dst alpine:3.24 cp -a /src/. /dst/
docker run --rm -v "$PWD/tests/app":/src:ro -v phphub-soak-tests:/dst alpine:3.24 cp -a /src/. /dst/
docker run -d --name k-client --network $NET alpine:3.24 sleep infinity >/dev/null
docker exec k-client apk add --no-cache wrk curl >/dev/null
# Mixed traffic: one wrk per path, all at once (Alpine's wrk ignores -s
# scripts, so a path-cycling Lua script is not an option).
mix() {
    dur=$1; shift
    docker exec k-client sh -c '
        dur=$1; shift; n=0
        for p in "$@"; do
            n=$((n + 1))
            wrk -t1 -c2 -d"$dur" "http://k-srv$p" > /tmp/w$n.txt 2>&1 &
        done
        wait
        cat /tmp/w*.txt | awk "/requests in/ {r += \$1} /Non-2xx/ {e += \$NF} /Socket errors/ {split(\$0, a, \"timeout \"); t += a[2]} END {print r + 0, e + 0, t + 0}"
        rm -f /tmp/w*.txt' sh "$dur" "$@"
}

rss() { docker exec k-srv sh -c 'grep VmRSS /proc/1/status' | awk '{print $2}'; }

soak() {
    label=$1 volume=$2 paths=$3; shift 3
    docker rm -f k-srv >/dev/null 2>&1
    docker run -d --name k-srv --network $NET --cpus=1 --memory=512m \
        -v "$volume":/var/www/html:ro -e SERVER_ROOT=/var/www/html/public -e WORKERS=2 "$@" "$IMAGE" >/dev/null
    for _ in $(seq 1 50); do
        [ "$(docker exec k-client curl -s -o /dev/null -w '%{http_code}' http://k-srv:9100/health)" = 200 ] && break
        sleep 0.2
    done
    # shellcheck disable=SC2086
    mix 5s $paths >/dev/null
    before=$(rss)
    # shellcheck disable=SC2086
    set -- $(mix "$SOAK" $paths)
    after=$(rss)
    reqs=$1 non2xx=$2 wrk_timeouts=$3
    running=$(docker inspect k-srv --format '{{.State.Running}} restarts={{.RestartCount}}')
    health=$(docker exec k-client curl -s -o /dev/null -w '%{http_code}' http://k-srv:9100/health)
    crashes=$(docker exec k-client curl -s http://k-srv:9100/metrics | awk '/^phphub_worker_crashes/ {s += $2} END {print s + 0}')
    restarts=$(docker exec k-client curl -s http://k-srv:9100/metrics | awk '/^phphub_worker_restarts/ {s += $2} END {print s + 0}')
    printf '%-24s %9s req  non-2xx %-7s rss %6s -> %6s KiB  health %s  running %s  crashes %s  restarts %s  wrk-timeouts %s\n' \
        "$label" "$reqs" "$non2xx" "$before" "$after" "$health" "$running" "$crashes" "$restarts" "$wrk_timeouts"
    echo "$running" | grep -q '^true' || fails=$((fails + 1))
    [ "$health" = 200 ] || fails=$((fails + 1))
    if echo "$label" | grep -q fatal; then
        # Every /fatal is a 500 and one thread restart; nothing else may fail.
        [ "$non2xx" -gt 0 ] && [ "$crashes" -gt 0 ] || fails=$((fails + 1))
    else
        [ "$non2xx" = 0 ] || fails=$((fails + 1))
    fi
    # More than 25% RSS growth during the soak counts as a leak.
    [ "$after" -le $((before + before / 4)) ] || fails=$((fails + 1))
}

PHALCON="/ /item/7?q=abc /hello /app.css /plain.php /item/8"
soak "worker" phphub-soak-bench "$PHALCON" -e PHPHUB_MODE=worker -e WORKER_SCRIPT=phphub.php
soak "worker MAX_REQUESTS=50" phphub-soak-bench "$PHALCON" -e PHPHUB_MODE=worker -e WORKER_SCRIPT=phphub.php -e MAX_REQUESTS=50
soak "index" phphub-soak-bench "$PHALCON" -e PHPHUB_MODE=index -e WORKER_SCRIPT=index.php
soak "file" phphub-soak-bench "$PHALCON" -e PHPHUB_MODE=file -e WORKER_SCRIPT=index.php
soak "auto" phphub-soak-bench "$PHALCON" -e WORKER_SCRIPT=phphub.php
# One request in ten is a fatal error: threads restart all the time.
FATAL="/ /?a=1 /header /status /x /y /z /a /b /fatal"
soak "worker + 10% fatal" phphub-soak-tests "$FATAL" -e PHPHUB_MODE=worker
# index.php prints before its loop, so in file mode its /fatal is a 200 with
# headers already sent; fatal.php has no output first and must be a 500.
soak "file + 10% fatal" phphub-soak-tests "/ /?a=1 /header /status /x /y /z /info3.php /a /fatal.php" -e PHPHUB_MODE=file

echo
if [ "$fails" -eq 0 ]; then echo "soak passed"; else echo "$fails soak checks failed"; fi
exit "$fails"
