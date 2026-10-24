#!/bin/sh

set -eu

redis_server=${1:?redis-server path is required}
redis_cli=${2:?redis-cli path is required}
root=${3:?scratch directory is required}

case "$root" in
  /tmp/hermit-frontier-redis-*) ;;
  *)
    printf 'unsafe Redis scratch directory: %s\n' "$root" >&2
    exit 2
    ;;
esac

socket=$root/redis.sock
pidfile=$root/redis.pid
logfile=$root/redis.log

cleanup() {
  if [ -S "$socket" ]; then
    "$redis_cli" -s "$socket" SHUTDOWN NOSAVE >/dev/null 2>&1 || true
  fi
  if [ -f "$pidfile" ]; then
    pid=$(cat "$pidfile" 2>/dev/null || true)
    case "$pid" in
      ''|*[!0-9]*) ;;
      *) kill "$pid" >/dev/null 2>&1 || true ;;
    esac
  fi
  rm -rf "$root"
}
trap cleanup EXIT HUP INT TERM

rm -rf "$root"
mkdir -m 700 "$root"

"$redis_server" \
  --daemonize yes \
  --port 0 \
  --unixsocket "$socket" \
  --unixsocketperm 700 \
  --save '' \
  --appendonly no \
  --pidfile "$pidfile" \
  --logfile "$logfile" \
  --dir "$root"

attempt=0
while ! "$redis_cli" -s "$socket" PING >/dev/null 2>&1; do
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 100 ]; then
    cat "$logfile" >&2
    exit 1
  fi
  sleep 0.01
done

"$redis_cli" --raw -s "$socket" \
  MSET alpha one beta two visits 0 >/dev/null
visits=$("$redis_cli" --raw -s "$socket" INCRBY visits 3)
fields=$("$redis_cli" --raw -s "$socket" \
  HSET profile name hermit mode strict)
time_value=$("$redis_cli" --raw -s "$socket" TIME | tr '\n' '.')
random_key=$("$redis_cli" --raw -s "$socket" RANDOMKEY)

printf 'ping=PONG visits=%s hash-fields=%s time=%s random-key=%s\n' \
  "$visits" "$fields" "$time_value" "$random_key"

"$redis_cli" -s "$socket" SHUTDOWN NOSAVE >/dev/null
rm -f "$pidfile"
trap - EXIT HUP INT TERM
rm -rf "$root"
