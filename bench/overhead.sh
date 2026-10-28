#!/bin/sh
# What PHPHub and Octoproxy cost per request, apart from the PHP application:
# throughput, latency and CPU per request on near-empty PHP routes, direct
# and through Octoproxy (with timeouts.upstream set, as BaseHub does).
#
#   ./bench/overhead.sh                # 1 CPU for PHPHub, 1 for Octoproxy, 2 PHP threads
#   PHPHUB_IMAGE=zhuzhu/php:8.5-phalcon-5.23-phphub ./bench/overhead.sh
set -eu
cd "$(dirname "$0")"
CPUS="${CPUS:-1}"
THREADS="${THREADS:-2}"
CONNS="${CONNS:-16}"
DURATION="${DURATION:-10s}"
PHPHUB_IMAGE="${PHPHUB_IMAGE:-phphub:dev}"
OCTO_IMAGE="${OCTO_IMAGE:-zhuzhu/octoproxy:latest}"
PATHS="${PATHS:-/hello / /big}"

docker volume rm -f phbench-app >/dev/null 2>&1 || true
docker volume create phbench-app >/dev/null
docker run --rm -v "$PWD/app":/src:ro -v phbench-app:/dst alpine:3.24 cp -a /src/. /dst/
docker volume rm -f phbench-octo >/dev/null 2>&1 || true
docker volume create phbench-octo >/dev/null
docker run --rm -v phbench-octo:/d alpine:3.24 sh -c 'cat > /d/app.yml <<Y
http:
  routers:
    b:
      rule: "Host(\`b.local\`)"
      entryPoints: [web]
      service: b
  services:
    b:
      loadBalancer:
        servers:
          - url: "http://b-phphub:80"
Y
printf "timeouts:\n  connect: 3s\n  headers: 10s\n  upstream: 125s\n  idle: 90s\n" > /d/timeouts.yml'

docker network create phbench >/dev/null 2>&1 || true
docker rm -f b-phphub b-octo b-client >/dev/null 2>&1 || true
docker run -d --name b-phphub --network phbench --cpus="$CPUS" --memory=512m \
    -v phbench-app:/var/www/html:ro -e SERVER_ROOT=/var/www/html/public \
    -e WORKER_SCRIPT=phphub.php -e WORKERS="$THREADS" -e MAX_REQUESTS=0 "$PHPHUB_IMAGE" >/dev/null
docker run -d --name b-octo --network phbench --cpus="$CPUS" --memory=256m -v phbench-octo:/dyn:ro "$OCTO_IMAGE" \
    --providers.file.directory=/dyn --providers.file.watch=false \
    --entrypoints.web.address=:80 --entrypoints.metrics.address=:9100 >/dev/null
OCTO_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' b-octo)
docker run -d --name b-client --network phbench --add-host "b.local:$OCTO_IP" alpine:3.24 sleep infinity >/dev/null
docker exec b-client apk add --no-cache wrk curl >/dev/null
docker exec b-client sh -c 'cat > /tmp/post.lua <<L
wrk.method = "POST"
wrk.body = string.rep("x", 20000)
wrk.headers["Content-Type"] = "application/x-www-form-urlencoded"
L'
sleep 3
for i in $(seq 1 30); do docker exec b-client curl -sf -o /dev/null http://b.local/hello && break; sleep 0.5; done
echo "check: direct $(docker exec b-client curl -s -o /dev/null -w '%{http_code}' http://b-phphub/hello)  octo $(docker exec b-client curl -s -o /dev/null -w '%{http_code}' http://b.local/hello)" 

# CPU time (us) of a container from the Docker stats API: a counter, and it
# needs no shell in the image (the Octoproxy image has none).
DOCKER_SOCK=$(docker context inspect -f '{{.Endpoints.docker.Host}}' "$(docker context show)" | sed 's#^unix://##')
cpu_us() {
    curl -s --unix-socket "$DOCKER_SOCK" "http://localhost/containers/$1/stats?stream=false" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["cpu_stats"]["cpu_usage"]["total_usage"]//1000)'
}
H='-H User-Agent:Mozilla/5.0(Macintosh;IntelMacOSX10_15_7)AppleWebKit/537.36(KHTML,likeGecko)Chrome/130.0Safari/537.36 -H Accept:text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8 -H Accept-Language:de-DE,de;q=0.9,en;q=0.8 -H Accept-Encoding:identity -H CF-Connecting-IP:203.0.113.9 -H CF-IPCountry:DE -H X-Forwarded-Proto:https'
COOKIE="-H Cookie:frontend=abcdefghijklmnopqrstuvwxyz0123456789abcdefghij;theme=dark;width=1440;dens=2;aview=grid;_ga=GA1.2.123456789.1700000000;_gid=GA1.2.987654321.1700000000"

run() { # name target url [wrk extra args]
    name=$1; target=$2; url=$3; shift 3
    docker exec b-client wrk -t1 -c"$CONNS" -d2s $H $COOKIE "$@" "$url" >/dev/null
    p0=$(cpu_us b-phphub); o0=$(cpu_us b-octo)
    out=$(docker exec b-client wrk -t1 -c"$CONNS" -d"$DURATION" --latency $H $COOKIE "$@" "$url")
    p1=$(cpu_us b-phphub); o1=$(cpu_us b-octo)
    echo "$out" | awk -v name="$name" -v target="$target" -v pc=$((p1-p0)) -v oc=$((o1-o0)) '
        /requests in/ {n=$1} /Requests\/sec/ {rps=$2} $1=="50%" {p50=$2} $1=="99%" {p99=$2}
        /Non-2xx|Socket errors/ {errs=errs $0 "; "}
        END { printf "%-18s %-7s %9s req/s  p50 %8s  p99 %8s  CPU/req: phphub %5.1fus  octoproxy %5.1fus  %s\n", name, target, rps, p50, p99, pc/n, oc/n, errs }'
}
echo "phphub=$PHPHUB_IMAGE octo=$OCTO_IMAGE cpus=$CPUS/each php_threads=$THREADS connections=$CONNS duration=$DURATION (8 request headers + 170B cookie on every request)"
for path in $PATHS; do
    run "GET $path" direct "http://b-phphub$path"
    run "GET $path" octo   "http://b.local$path"
done
run "POST 20KB /hello" direct "http://b-phphub/hello" -s /tmp/post.lua
run "POST 20KB /hello" octo   "http://b.local/hello" -s /tmp/post.lua
docker exec b-phphub wget -qO- 127.0.0.1:9100/metrics | grep -E "^phphub_worker_(crashes|restarts)"
docker rm -f b-phphub b-octo b-client >/dev/null
