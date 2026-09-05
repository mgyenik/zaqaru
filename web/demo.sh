#!/bin/sh
# Makes the demo: the nginx + gunicorn + Django image baked into a module,
# and that module booted under Node and written to a snapshot, so that the
# page starts from a listening server. Wants docker, and about a minute.
#
#   web/demo.sh [out-dir]      default web/demo
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$repo/web/demo}
mkdir -p "$out"
docker build -q -t hello-django "$repo/demo/hello-django" >/dev/null
docker save hello-django:latest -o "$out/hello-django.tar"
cargo build --release --quiet -p zaqaru --manifest-path "$repo/Cargo.toml"
"$repo/target/release/zaqaru" bake "$out/hello-django.tar" -o "$out/hello-django.wasm"
rm -f "$out/hello-django.tar"
node "$repo/web/preboot.mjs" "$out/hello-django.wasm" "$out/hello-django.snapshot" --publish 80
echo "demo in $out: hello-django.wasm, hello-django.snapshot"
echo "serve the repository and open web/?module=demo/hello-django.wasm&snapshot=demo/hello-django.snapshot&live=80"
