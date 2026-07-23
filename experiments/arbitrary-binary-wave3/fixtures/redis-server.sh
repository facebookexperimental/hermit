#!/bin/sh

set -eu

root=/tmp/hermit-wave3-redis
port=18940
mkdir "$root"

cleanup() {
  redis-cli -h 127.0.0.1 -p "$port" shutdown nosave \
    >/dev/null 2>&1 || true
}
trap cleanup EXIT HUP INT TERM

printf 'phase=redis-start\n'
redis-server \
  --daemonize yes \
  --bind 127.0.0.1 \
  --port "$port" \
  --save '' \
  --appendonly no \
  --pidfile "$root/redis.pid" \
  --logfile "$root/redis.log" \
  --dir "$root"
printf 'phase=redis-ping\n'

attempt=0
until redis-cli -h 127.0.0.1 -p "$port" ping >/dev/null 2>&1; do
  attempt=$((attempt + 1))
  [ "$attempt" -lt 40 ] || {
    printf 'redis server did not become ready\n' >&2
    exit 1
  }
  sleep 0.05
done

redis-cli -h 127.0.0.1 -p "$port" set wave3 ok >/dev/null
value=$(redis-cli -h 127.0.0.1 -p "$port" get wave3)
printf 'phase=redis-bgsave\n'
redis-cli -h 127.0.0.1 -p "$port" bgsave >/dev/null

attempt=0
while [ ! -f "$root/dump.rdb" ]; do
  attempt=$((attempt + 1))
  [ "$attempt" -lt 80 ] || {
    printf 'redis background save did not finish\n' >&2
    exit 1
  }
  sleep 0.05
done

printf 'phase=redis-stop\n'
redis-cli -h 127.0.0.1 -p "$port" shutdown nosave >/dev/null
trap - EXIT HUP INT TERM
printf 'redis-wave3-%s\n' "$value"
