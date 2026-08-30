#!/bin/sh
# An OCI image runs one command and this container is a tree, so the
# entrypoint is what builds it: gunicorn behind, nginx in front, and nginx
# in the foreground so that the container's life is its life.
set -e
gunicorn --workers 1 --bind 127.0.0.1:8000 hello.wsgi &
exec nginx -g 'daemon off;'
