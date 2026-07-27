#!/bin/sh

set -eu

redis_server=${1:?redis-server path is required}
redis_cli=${2:?redis-cli path is required}
mode=${3:-small}
instance=${4:-$$}
port=${5:?unique Redis port is required}
root=/tmp/hermit-redis-strict-$instance
current_pid=

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

case "$mode" in
  small|extended) ;;
  *)
    printf 'unknown workload mode: %s\n' "$mode" >&2
    exit 2
    ;;
esac

case "$port" in
  ''|*[!0-9]*) fail "Redis port must be numeric: $port" ;;
esac
if [ "$port" -lt 1024 ] || [ "$port" -gt 65535 ]; then
  fail "Redis port is outside the usable range: $port"
fi

endpoint_pid() {
  "$redis_cli" --raw -h 127.0.0.1 -p "$port" INFO server 2>/dev/null |
    sed -n 's/^process_id:\([0-9][0-9]*\).*/\1/p'
}

wait_for_pid_exit() {
  pid=$1
  attempt=0
  while kill -0 "$pid" 2>/dev/null; do
    attempt=$((attempt + 1))
    [ "$attempt" -lt 100 ] || return 1
    sleep 0.02
  done
}

cleanup_owned_server() {
  pid=$current_pid
  [ -n "$pid" ] || return 0

  observed_pid=$(endpoint_pid || true)
  if [ "$observed_pid" = "$pid" ]; then
    "$redis_cli" -h 127.0.0.1 -p "$port" shutdown nosave \
      >/dev/null 2>&1 || true
  fi
  if ! wait_for_pid_exit "$pid"; then
    kill -TERM "$pid" 2>/dev/null || true
    if ! wait_for_pid_exit "$pid"; then
      kill -KILL "$pid" 2>/dev/null || true
      wait_for_pid_exit "$pid" || true
    fi
  fi
  current_pid=
}

cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  cleanup_owned_server
  rm -rf "$root"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$root"

start_server() {
  if "$redis_cli" -h 127.0.0.1 -p "$port" PING >/dev/null 2>&1; then
    fail "Redis endpoint is already serving before launch: 127.0.0.1:$port"
  fi
  [ ! -e "$root/redis.pid" ] || fail "stale Redis pidfile before launch"

  "$redis_server" \
    --daemonize yes \
    --bind 127.0.0.1 \
    --protected-mode no \
    --port "$port" \
    --save '' \
    --appendonly no \
    --dbfilename dump.rdb \
    --pidfile "$root/redis.pid" \
    --logfile "$root/redis.log" \
    --dir "$root"

  attempt=0
  while :; do
    if [ -s "$root/redis.pid" ]; then
      current_pid=$(cat "$root/redis.pid")
      case "$current_pid" in
        ''|*[!0-9]*) fail "invalid Redis pidfile contents: $current_pid" ;;
      esac
      observed_pid=$(endpoint_pid || true)
      if kill -0 "$current_pid" 2>/dev/null && \
        [ "$observed_pid" = "$current_pid" ]; then
        return 0
      fi
    fi
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ]; then
      printf 'redis-server did not become ready as its recorded PID\n' >&2
      cat "$root/redis.log" >&2
      exit 1
    fi
    sleep 0.02
  done
}

redis() {
  "$redis_cli" --raw -h 127.0.0.1 -p "$port" "$@"
}

stop_server() {
  pid=$current_pid
  [ -n "$pid" ] || fail "cannot stop Redis without an owned PID"
  observed_pid=$(endpoint_pid || true)
  [ "$observed_pid" = "$pid" ] || \
    fail "Redis endpoint PID $observed_pid does not match owned PID $pid"

  redis SHUTDOWN NOSAVE >/dev/null
  wait_for_pid_exit "$pid" || fail "Redis PID $pid did not exit after shutdown"
  current_pid=
  rm -f "$root/redis.pid"
}

expect() {
  expected=$1
  shift
  actual=$(redis "$@")
  if [ "$actual" != "$expected" ]; then
    printf 'redis command failed: expected <%s>, got <%s>\n' \
      "$expected" "$actual" >&2
    exit 1
  fi
}

start_server
expect PONG PING
expect OK SET strict:string hermit
expect hermit GET strict:string
expect 1 INCR strict:counter
expect 2 INCR strict:counter
expect 3 RPUSH strict:list alpha beta gamma
expect "$(printf 'alpha\nbeta\ngamma')" LRANGE strict:list 0 -1

printf 'mode=%s\n' "$mode"
printf 'ping=PONG\n'
printf 'string=hermit\n'
printf 'counter=2\n'
printf 'list=alpha,beta,gamma\n'

if [ "$mode" = extended ]; then
  expect 2 HSET strict:hash field-a one field-b two
  expect one HGET strict:hash field-a
  expect two HGET strict:hash field-b
  expect 3 SADD strict:set gamma alpha beta
  expect 1 SISMEMBER strict:set alpha
  expect 1 SISMEMBER strict:set beta
  expect 1 SISMEMBER strict:set gamma
  expect 3 ZADD strict:zset 30 gamma 10 alpha 20 beta
  expect "$(printf 'alpha\nbeta\ngamma')" ZRANGE strict:zset 0 -1
  expect 42 EVAL 'return redis.call("INCRBY", KEYS[1], ARGV[1])' \
    1 strict:lua-counter 42
  expect '1-0' XADD strict:stream 1-0 field value
  expect "$(printf 'strict:stream\n1-0\nfield\nvalue')" \
    XREAD COUNT 1 STREAMS strict:stream 0-0

  redis BGSAVE >/dev/null
  attempt=0
  while [ ! -s "$root/dump.rdb" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 200 ]; then
      printf 'redis background save did not finish\n' >&2
      cat "$root/redis.log" >&2
      exit 1
    fi
    sleep 0.02
  done

  old_pid=$current_pid
  stop_server
  start_server
  new_pid=$current_pid
  [ "$new_pid" != "$old_pid" ] || fail "Redis restart reused PID $old_pid"
  expect hermit GET strict:string
  expect 2 GET strict:counter
  printf 'pid-turnover=ok\n'
  printf 'persistence=ok\n'
fi

stop_server
trap - EXIT HUP INT TERM
rm -rf "$root"
printf 'redis-strict-%s-ok\n' "$mode"
