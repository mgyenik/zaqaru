#!/bin/sh
# An OCI image runs one command and this container is a tree, so the
# entrypoint is what builds it: gunicorn behind, nginx in front, and nginx
# in the foreground so that the container's life is its life.
set -e
# `--timeout 300` because gunicorn's watchdog kills a worker that takes
# longer than 30 seconds to answer, and an interpreted CPython is one to two
# orders of magnitude off native — the first request pulls Django's whole
# import graph through the interpreter. It is a statement about the engine's
# speed, not about the worker being stuck.
gunicorn --workers 1 --timeout 300 --bind 127.0.0.1:8000 hello.wsgi &
exec nginx -g 'daemon off;'
