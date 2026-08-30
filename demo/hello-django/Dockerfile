# Debian-based for glibc, which is the tested libc.
FROM python:3.12-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends nginx \
    && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir gunicorn django

WORKDIR /app
COPY hello /app/hello
COPY nginx.conf /etc/nginx/nginx.conf
COPY init.sh /app/init.sh

EXPOSE 80
CMD ["/app/init.sh"]
