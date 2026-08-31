#!/bin/bash
# N5's last acceptance: a served HTTP session, recorded and replayed
# bit-identically with no network at all.
#
#   usage: replay.sh [host-port]
#
# Nothing inside the container is nondeterministic. The schedule is a
# function of retired instructions, the guest's instructions are a function
# of its own bytes, and every input from outside — the clock, the entropy
# seed, the network, a shutdown request — arrives as a store *read*. So the
# sequence of read answers is the whole of a run's nondeterminism, and a
# tape of it is the run.
set -euo pipefail

REPO=${ZAQARU_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
OUT=${ZAQARU_DEMO_OUT:-/tmp/zaqaru-demo}
PORT=${1:-8080}
RUN="$REPO/target/release/zaqaru-run"
mkdir -p "$OUT"

[ -f "$OUT/hello-django.wasm" ] || { echo "run django.sh first" >&2; exit 1; }
rm -f "$OUT/session.bin" "$OUT/served.txt" "$OUT/replayed.txt"

"$RUN" "$OUT/hello-django.wasm" -p "$PORT:80" --record "$OUT/session.bin" > "$OUT/served.txt" 2>&1 &
container=$!
# Reaps whatever is still running however this exits. Kept armed to the end
# rather than disarmed once the container is stopped: a script that turns its
# own cleanup off part-way through is one leaked server away from a core
# burning until somebody notices, and nobody notices quickly.
trap 'kill -9 $(jobs -p) 2>/dev/null' EXIT INT TERM

for _ in $(seq 40); do
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 "http://localhost:$PORT/" || true)
    [ "$code" = "200" ] && { echo "served a request"; break; }
    sleep 5
done
sleep 3
kill -INT "$container"
for _ in $(seq 20); do kill -0 "$container" 2>/dev/null || break; sleep 2; done
kill -9 "$container" 2>/dev/null || true

# No `-p`. Nothing is listening, nothing is mounted at `/iso/net`, and the
# session is served entirely from the tape.
echo "replaying, with no network"
"$RUN" "$OUT/hello-django.wasm" --replay "$OUT/session.bin" > "$OUT/replayed.txt" 2>&1

# The runner's own lines are the runner's, not the container's.
strip() { grep -vE 'listening on host port|recorded [0-9]+ host answers' "$1"; }
if diff <(strip "$OUT/served.txt") <(strip "$OUT/replayed.txt") > /dev/null; then
    echo
    echo "the replayed session is identical, byte for byte:"
    grep -E 'GET /' "$OUT/replayed.txt" | tail -1
    exit 0
fi
echo "the replay diverged:" >&2
diff <(strip "$OUT/served.txt") <(strip "$OUT/replayed.txt") | head -20 >&2
exit 1
