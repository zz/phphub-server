#!/bin/sh
# Side-by-side load test: PHPHub vs FrankenPHP worker mode, same Phalcon app,
# same CPU and memory limits. Needs phphub:dev built from this repo.
#
#   ./bench/run.sh            # 1 CPU, 2 PHP threads
#   CPUS=0.2 ./bench/run.sh   # lab setting
#   MODE=file ./bench/run.sh  # PHPHUB_MODE=file vs FrankenPHP php_server without worker
#   MODE=index ./bench/run.sh # PHPHUB_MODE=index vs the same FrankenPHP setup
set -eu
cd "$(dirname "$0")"
CPUS="${CPUS:-1}"
MEM="${MEM:-512m}"
THREADS="${THREADS:-2}"
CONNS="${CONNS:-16}"
DURATION="${DURATION:-15s}"
ROUNDS="${ROUNDS:-2}"
MODE="${MODE:-worker}"
case "$MODE" in
worker) PHPHUB_ENV="-e WORKER_SCRIPT=phphub.php"; CADDYFILE=Caddyfile ;;
index|file) PHPHUB_ENV="-e PHPHUB_MODE=$MODE -e WORKER_SCRIPT=index.php"; CADDYFILE=Caddyfile.classic ;;
*) echo "MODE must be worker, index or file" >&2; exit 2 ;;
esac
# Fixed-rate run for latency at equal load (requests/s across all connections).
RATE="${RATE:-4000}"
# /plain.php is a one-line PHP file: file mode runs it, the others run the entry.
PATHS="${PATHS:-/ /item/7?q=abc /hello /app.css /plain.php}"
FRANKEN_IMAGE="${FRANKEN_IMAGE:-zhuzhu/php:8.5-phalcon-5.23-frankenphp}"
PHPHUB_IMAGE="${PHPHUB_IMAGE:-phphub:dev}"
# Copy the app into a volume inside the Docker VM. A bind mount from the
# macOS host makes every stat/open slow and would skew both servers.
docker volume rm -f phbench-app >/dev/null 2>&1 || true
docker volume create phbench-app >/dev/null
docker run --rm -v "$PWD/app":/src:ro -v phbench-app:/dst alpine:3.24 cp -a /src/. /dst/
APP=phbench-app

docker network create phbench >/dev/null 2>&1 || true
docker rm -f b-phphub b-franken b-client >/dev/null 2>&1 || true
docker run -d --name b-phphub --network phbench --cpus="$CPUS" --memory="$MEM" \
    -v "$APP":/var/www/html:ro -e SERVER_ROOT=/var/www/html/public \
    $PHPHUB_ENV -e WORKERS="$THREADS" -e MAX_REQUESTS=0 "$PHPHUB_IMAGE" >/dev/null
docker run -d --name b-franken --network phbench --cpus="$CPUS" --memory="$MEM" \
    -v "$APP":/var/www/html:ro  \
    -e THREADS="$THREADS" "$FRANKEN_IMAGE" frankenphp run --config "/var/www/html/$CADDYFILE" >/dev/null
docker run -d --name b-client --network phbench alpine:3.24 sleep infinity >/dev/null
docker exec b-client apk add --no-cache wrk hey curl >/dev/null
sleep 3

echo "mode=$MODE cpus=$CPUS mem=$MEM php_threads=$THREADS connections=$CONNS duration=$DURATION"
for path in $PATHS; do
    for host in b-phphub b-franken; do
        docker exec b-client wrk -t1 -c"$CONNS" -d3s "http://$host$path" >/dev/null # warm up
    done
    for round in $(seq 1 "$ROUNDS"); do
        for host in b-phphub b-franken; do
            out=$(docker exec b-client wrk -t1 -c"$CONNS" -d"$DURATION" --latency "http://$host$path")
            rps=$(echo "$out" | awk '/Requests\/sec/ {print $2}')
            p50=$(echo "$out" | awk '$1=="50%" {print $2}')
            p99=$(echo "$out" | awk '$1=="99%" {print $2}')
            errs=$(echo "$out" | awk '/Non-2xx|Socket errors/ {printf "%s ", $0}')
            printf '%-14s %-10s round %s  %10s req/s  p50 %8s  p99 %8s  %s\n' "$path" "${host#b-}" "$round" "$rps" "$p50" "$p99" "$errs"
        done
    done
done
echo "--- latency at a fixed $RATE req/s (hey, $CONNS connections)"
per=$((RATE / CONNS))
for path in $PATHS; do
    for host in b-phphub b-franken; do
        out=$(docker exec b-client hey -z "$DURATION" -c "$CONNS" -q "$per" "http://$host$path")
        rps=$(echo "$out" | awk '/Requests\/sec/ {print $2}')
        p50=$(echo "$out" | awk '/ 50% in/ {print $3*1000 "ms"}')
        p99=$(echo "$out" | awk '/ 99% in/ {print $3*1000 "ms"}')
        codes=$(echo "$out" | awk '/Status code distribution/ {on=1; next} on && /\[[0-9]+\]/ {printf "%s%s ", $1, $2} on && /^$/ {on=0}')
        printf '%-14s %-10s %10s req/s  p50 %8s  p99 %8s  %s\n' "$path" "${host#b-}" "$rps" "$p50" "$p99" "$codes"
    done
done
for host in b-phphub b-franken; do
    printf '%-10s cpu throttled: %s\n' "${host#b-}" "$(docker exec "$host" sh -c 'grep throttled_usec /sys/fs/cgroup/cpu.stat' | awk '{printf "%.1fs", $2/1e6}')"
done
for host in b-phphub b-franken; do
    printf '%-10s memory: %s\n' "${host#b-}" "$(docker stats --no-stream --format '{{.MemUsage}}' "$host")"
done
docker rm -f b-phphub b-franken b-client >/dev/null
