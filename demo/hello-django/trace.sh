#!/bin/bash
# Gate N0 of `docs/network-plan.md`: what nginx + gunicorn + django actually
# call, traced from the real stack rather than guessed.
#
#   usage: trace.sh
#
# Writes the raw trace to $ZAQARU_DEMO_OUT/n0/native.txt (megabytes, so not
# the repo) and regenerates demo/hello-django/baseline/n0-surface.txt, which
# is the worklist and the baseline N5 diffs the interpreted run against.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=${ZAQARU_REPO:-$(cd "$HERE/../.." && pwd)}
OUT=${ZAQARU_DEMO_OUT:-/tmp/zaqaru-demo}/n0
PORT=${ZAQARU_TRACE_PORT:-18080}
mkdir -p "$OUT"
chmod 777 "$OUT"
rm -f "$OUT/native.txt"

docker build -q -t hello-django "$HERE" >/dev/null
docker build -q -t hello-django:trace -f "$HERE/Dockerfile.trace" "$HERE" >/dev/null
docker rm -f zaqaru-n0 >/dev/null 2>&1 || true

# `SYS_PTRACE` and an unconfined seccomp profile because the container traces
# itself; neither is needed by the demo image, which is why the tracing image
# is a separate one.
docker run -d --name zaqaru-n0 \
    --cap-add=SYS_PTRACE --security-opt seccomp=unconfined \
    -v "$OUT:/trace" -p "$PORT:80" hello-django:trace >/dev/null

for _ in $(seq 30); do
    body=$(curl -s --max-time 2 "http://localhost:$PORT/" || true)
    [ -n "$body" ] && break
    sleep 1
done
[ -n "${body:-}" ] || { docker logs zaqaru-n0; docker rm -f zaqaru-n0; exit 1; }
echo "first request:  $body"
# A second, so the warm path is in the baseline beside the cold one.
curl -s --max-time 2 -o /dev/null "http://localhost:$PORT/"
echo "second request: ok"

# The shutdown, sent from *inside*. `docker stop` signals pid 1, which is
# strace — the tracer would die and take the shutdown path with it, which is
# exactly the half of the trace N0 asks for.
docker exec zaqaru-n0 sh -c '
  kill -TERM "$(cat /run/nginx.pid)" 2>/dev/null || true
  for p in /proc/[0-9]*; do
    [ -r "$p/cmdline" ] || continue
    tr "\0" " " < "$p/cmdline" 2>/dev/null | grep -q "[g]unicorn" && kill -TERM "${p#/proc/}" 2>/dev/null || true
  done' >/dev/null 2>&1 || true
sleep 4
docker rm -f zaqaru-n0 >/dev/null 2>&1 || true

python3 "$HERE/surface.py" "$OUT/native.txt" "$REPO"
