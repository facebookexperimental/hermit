#!/bin/sh

set -eu

root=/tmp/hermit-wave3-nginx
port=18939
mkdir -p \
  "$root/client" \
  "$root/proxy" \
  "$root/fastcgi" \
  "$root/uwsgi" \
  "$root/scgi"

cat >"$root/nginx.conf" <<EOF
user root;
daemon on;
master_process on;
pid $root/nginx.pid;
error_log $root/error.log notice;
events { worker_connections 64; }
http {
  access_log off;
  client_body_temp_path $root/client;
  proxy_temp_path $root/proxy;
  fastcgi_temp_path $root/fastcgi;
  uwsgi_temp_path $root/uwsgi;
  scgi_temp_path $root/scgi;
  server {
    listen 127.0.0.1:$port;
    location / { return 200 "nginx-wave3-ok\\n"; }
  }
}
EOF

cleanup() {
  nginx -e "$root/error.log" -p "$root" -c nginx.conf -s quit \
    >/dev/null 2>&1 || true
}
trap cleanup EXIT HUP INT TERM

printf 'phase=nginx-start\n'
nginx -e "$root/error.log" -p "$root" -c nginx.conf
printf 'phase=nginx-request\n'

attempt=0
while [ "$attempt" -lt 40 ]; do
  if response=$(curl \
    --silent \
    --show-error \
    --fail \
    --noproxy '*' \
    "http://127.0.0.1:$port/" 2>/dev/null); then
    printf '%s\n' "$response"
    break
  fi
  attempt=$((attempt + 1))
  sleep 0.05
done
[ "$attempt" -lt 40 ] || {
  printf 'nginx server did not become ready\n' >&2
  exit 1
}

printf 'phase=nginx-stop\n'
nginx -e "$root/error.log" -p "$root" -c nginx.conf -s quit
trap - EXIT HUP INT TERM
