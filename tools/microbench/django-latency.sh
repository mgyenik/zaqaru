#!/bin/bash
# The same OCI image two ways: `docker run` and `zaqaru-run`, same client.
#
# What is deliberately *not* controlled: the native container gets the whole
# machine and the module is single-threaded by construction, because that is
# the comparison worth having. The question is not "how do these compare per
# core", it is "what does this cost me".
set -euo pipefail

REPO=${ZAQARU_REPO:-/home/m/git/zaqaru}
OUT=${ZAQARU_DEMO_OUT:-/tmp/zaqaru-demo}
NATIVE_PORT=${NATIVE_PORT:-8091}
WASM_PORT=${WASM_PORT:-8090}
mkdir -p "$OUT"

container=""
docker_id=""
cleanup() {
    [ -n "$container" ] && kill "$container" 2>/dev/null || true
    [ -n "$docker_id" ] && docker rm -f "$docker_id" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

echo "== building the image (cached) =="
docker build -q -t hello-django "$REPO/demo/hello-django" >/dev/null
[ -f "$OUT/hello-django.tar" ] && [ "$OUT/hello-django.tar" -nt "$REPO/demo/hello-django/Dockerfile" ] \
    || docker save hello-django:latest -o "$OUT/hello-django.tar"

echo "== baking the module =="
cargo run --manifest-path "$REPO/Cargo.toml" --release --quiet --example bake-vm -- \
    "$OUT/hello-django.tar" "$OUT/hello-django.wasm"
cargo build --manifest-path "$REPO/Cargo.toml" --release --quiet -p runner
ls -la "$OUT/hello-django.wasm" | awk '{printf "module: %.1f MB\n", $5/1048576}'

echo
echo "== native: docker run =="
docker_id=$(docker run --rm -d -p "$NATIVE_PORT:80" hello-django)
uv run --script "$REPO/tools/microbench/latency.py" native "$NATIVE_PORT" 120
docker rm -f "$docker_id" >/dev/null; docker_id=""

echo
echo "== wasm: zaqaru-run =="
"$REPO/target/release/zaqaru-run" "$OUT/hello-django.wasm" -p "$WASM_PORT:80" \
    >"$OUT/wasm.log" 2>&1 &
container=$!
# The module's pid, so readiness is read off its CPU use rather than asked
# for with requests that would queue behind the boot -- see `await_idle`.
uv run --script "$REPO/tools/microbench/latency.py" wasm "$WASM_PORT" 900 "$container"
echo
echo "-- what the module reported --"
grep -E 'compiled|instructions in' "$OUT/wasm.log" || true
kill "$container" 2>/dev/null || true
wait "$container" 2>/dev/null || true
container=""
grep -E 'instructions in' "$OUT/wasm.log" || true
