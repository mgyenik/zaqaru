#!/bin/bash
# The interpreted half of the trace comparison: the same scenario `trace.sh`
# traced natively — boot, two `curl` requests, a `SIGTERM` shutdown — run
# under the interpreter with its own trace on.
#
# Then:
#   python3 demo/hello-django/diff.py \
#       $ZAQARU_DEMO_OUT/traces/native.txt $ZAQARU_DEMO_OUT/traces/interpreted.txt
#
# Waits for gunicorn's control socket before stopping, because five of the
# native trace's calls live in that thread and a comparison that stops
# before the other run got there is comparing durations, not kernels.
set -u
trap 'kill -9 $(jobs -p) 2>/dev/null' EXIT INT TERM
REPO=${ZAQARU_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
PORT=8104
OUT=${ZAQARU_DEMO_OUT:-/tmp/zaqaru-demo}/traces/interpreted.txt
rm -f "$OUT"
"$REPO/target/release/zaqaru" emulate --trace -p "$PORT:80" \
    ${ZAQARU_DEMO_OUT:-/tmp/zaqaru-demo}/hello-django.tar > "$OUT" 2>&1 &
pid=$!
for _ in $(seq 40); do
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 "http://localhost:$PORT/" || true)
    [ "$code" = "200" ] && { echo "first request 200"; break; }
    sleep 5
done
curl -s -o /dev/null --max-time 30 "http://localhost:$PORT/" && echo "second request ok"
# Long enough for gunicorn to reach its control socket, which is where five
# of the native trace's calls live — a comparison that stops before the
# other run got there is comparing durations, not kernels.
for _ in $(seq 40); do
    grep -q 'Control socket' "$OUT" && { echo "control socket up"; break; }
    sleep 3
done
kill -INT "$pid"
for _ in $(seq 20); do
    kill -0 "$pid" 2>/dev/null || { echo "shut down cleanly"; break; }
    sleep 2
done
kill -9 "$pid" 2>/dev/null
wc -l "$OUT"
