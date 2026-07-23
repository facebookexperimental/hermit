#!/bin/sh

set -eu

root=/tmp/hermit-wave3-curl
port=18938
mkdir -p "$root"
printf 'curl-wave3-ok\n' >"$root/payload.txt"

printf 'phase=curl-server-start\n'
/usr/bin/python3 -m http.server "$port" \
  --bind 127.0.0.1 \
  --directory "$root" \
  >"$root/server.log" 2>&1 &
server=$!

cleanup() {
  kill "$server" 2>/dev/null || true
  wait "$server" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

printf 'phase=curl-request\n'
attempt=0
while [ "$attempt" -lt 40 ]; do
  if response=$(/usr/bin/curl \
    --silent \
    --show-error \
    --fail \
    --noproxy '*' \
    "http://127.0.0.1:$port/payload.txt" 2>/dev/null); then
    printf '%s\n' "$response"
    exit 0
  fi
  attempt=$((attempt + 1))
  sleep 0.05
done

printf 'curl server did not become ready\n' >&2
exit 1
