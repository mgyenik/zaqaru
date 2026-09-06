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
cargo build --release --quiet -p zaqaru-brotli --target wasm32-unknown-unknown --manifest-path "$repo/Cargo.toml"
cp "$repo/target/wasm32-unknown-unknown/release/brotli.wasm" "$repo/web/brotli.wasm"
"$repo/target/release/zaqaru" bake "$out/hello-django.tar" -o "$out/hello-django.wasm"
rm -f "$out/hello-django.tar"
# brotli at quality 10: 29 MB against gzip's 39, two minutes to compress
# against four for the last 0.7 MB quality 11 finds; the page inflates it
# with web/brotli.wasm in 0.7 s.
node "$repo/web/preboot.mjs" "$out/hello-django.wasm" "$out/hello-django.snapshot" --publish 80 --brotli 10
# And that what was written answers: Django's page, through nginx.
node "$repo/web/check-demo.mjs" "$out/hello-django.wasm" "$out/hello-django.snapshot" 80 'GET / HTTP/1.0\r\n\r\n' '<h1>Hello, world!</h1>'
echo "demo in $out: hello-django.wasm, hello-django.snapshot"
echo "serve the repository and open web/?module=demo/hello-django.wasm&snapshot=demo/hello-django.snapshot&live=80"
