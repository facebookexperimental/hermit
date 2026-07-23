#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$ROOT/../.." && pwd)
HERMIT="$REPO/target/debug/hermit"
FIXTURES="$ROOT/fixtures"
RESULTS="$ROOT/results"
RUSTC=$(rustup which rustc)
TIMEOUT_SECONDS=${TIMEOUT_SECONDS:-90}

mkdir -p "$RESULTS"
printf 'workload\tmode\texit_code\tduration_ms\tclassification\n' > "$ROOT/results.tsv"
printf 'workload\tcommand\n' > "$ROOT/commands.tsv"

nginx_cmd='set -eu; printf "nginx-shell:start\n" >&2; mkdir -p /tmp/nginx-client-body /tmp/nginx-proxy /tmp/nginx-fastcgi /tmp/nginx-uwsgi /tmp/nginx-scgi; nginx -e stderr -c /tmp/wave2/nginx.conf & server=$!; trap '\''nginx -e stderr -c /tmp/wave2/nginx.conf -s quit >/dev/null 2>&1 || true'\'' EXIT; sleep 0.5; kill -0 "$server"; printf "nginx-shell:probe\n" >&2; body=$(curl -fsS http://127.0.0.1:18080/); test "$body" = nginx-ok; nginx -e stderr -c /tmp/wave2/nginx.conf -s quit; wait "$server"; trap - EXIT; printf "nginx-ok\n"'
redis_cmd='set -eu; printf "redis-shell:start\n" >&2; redis-server /tmp/wave2/redis.conf & server=$!; trap '\''redis-cli -h 127.0.0.1 -p 16379 shutdown nosave >/dev/null 2>&1 || true'\'' EXIT; sleep 0.5; pong=""; for i in $(seq 1 30); do pong=$(redis-cli -h 127.0.0.1 -p 16379 ping 2>/dev/null) && break; sleep 0.05; done; printf "redis-shell:probe=%s\n" "$pong" >&2; test "$pong" = PONG; test "$(redis-cli -h 127.0.0.1 -p 16379 set wave2 deterministic)" = OK; test "$(redis-cli -h 127.0.0.1 -p 16379 get wave2)" = deterministic; redis-cli -h 127.0.0.1 -p 16379 shutdown nosave >/dev/null; printf "redis-shell:shutdown\n" >&2; wait "$server"; printf "redis-shell:waited\n" >&2; trap - EXIT; printf "redis-ok\n"'
sqlite_cmd='exec sqlite3 :memory: "CREATE TABLE t(v INTEGER); WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO t SELECT x FROM n; SELECT '\''sqlite-ok '\'' || count(*) || '\'' '\'' || sum(v) FROM t;"'
python_cmd='exec /usr/bin/python3 -c '\''import threading; results=[0]*4; threads=[threading.Thread(target=lambda i=i: results.__setitem__(i, sum(n ^ i for n in range(100000)))) for i in range(4)]; [t.start() for t in threads]; [t.join() for t in threads]; print("python-ok", sum(results))'\'''
python_meta_cmd='exec /usr/local/bin/python3 -c '\''import threading; results=[0]*4; threads=[threading.Thread(target=lambda i=i: results.__setitem__(i, sum(n ^ i for n in range(100000)))) for i in range(4)]; [t.start() for t in threads]; [t.join() for t in threads]; print("python-ok", sum(results))'\'''
java_cmd='printf "java-shell:runtime\n" >&2; exec java -jar /tmp/wave2/wave2.jar'
node_cmd='printf "node-shell:runtime\n" >&2; exec node -e '\''const {Worker}=require("worker_threads"); const code=`const {parentPort,workerData}=require("worker_threads");let s=0;for(let n=0;n<100000;n++)s+=n^workerData;parentPort.postMessage(s)`; Promise.all([0,1,2,3].map(i=>new Promise((resolve,reject)=>{const w=new Worker(code,{eval:true,workerData:i});w.once("message",resolve);w.once("error",reject)}))).then(v=>console.log("node-ok",v.reduce((a,b)=>a+b,0)))'\'''
gcc_cmd='set -eu; gcc -O2 -pthread /tmp/wave2/hello.c -o /tmp/wave2-gcc; exec /tmp/wave2-gcc'
rustc_cmd="set -eu; '$RUSTC' -O /tmp/wave2/hello.rs -o /tmp/wave2-rustc; exec /tmp/wave2-rustc"

read -r -a workloads <<< "${WORKLOADS:-nginx redis sqlite3 python3 python3_meta java node gcc rustc}"
read -r -a modes <<< "${MODES:-run verify chaos}"
for workload in "${workloads[@]}"; do
    var="${workload//3/}_cmd"
    if [[ "$workload" == sqlite3 ]]; then var=sqlite_cmd; fi
    if [[ "$workload" == python3 ]]; then var=python_cmd; fi
    command=${!var}
    printf '%s\t%s\n' "$workload" "$command" >> "$ROOT/commands.tsv"

    for mode in "${modes[@]}"; do
        mkdir -p "$RESULTS/$workload"
        stdout="$RESULTS/$workload/$mode.stdout"
        stderr="$RESULTS/$workload/$mode.stderr"
        mode_flags=()
        case "$mode" in
            run) ;;
            verify) mode_flags+=(--verify) ;;
            chaos) mode_flags+=(--chaos --seed=1) ;;
            chaos_random) mode_flags+=(--chaos --seed=1 --sched-heuristic=random) ;;
        esac

        case_timeout=$TIMEOUT_SECONDS
        case "$workload" in
            nginx|redis|python3_meta) case_timeout=${FAILURE_TIMEOUT_SECONDS:-10} ;;
        esac
        case "$workload:$mode" in
            node:verify) case_timeout=${VERIFY_TIMEOUT_SECONDS:-180} ;;
            node:chaos) case_timeout=${NODE_CHAOS_TIMEOUT_SECONDS:-60} ;;
        esac

        start=$(date +%s%3N)
        set +e
        setsid timeout -k 5s "${case_timeout}s" "$HERMIT" run \
            --base-env=minimal \
            --no-virtualize-cpuid \
            --preemption-timeout=disabled \
            --bind "$FIXTURES:/tmp/wave2" \
            "${mode_flags[@]}" \
            -- /bin/sh -c "$command" >"$stdout" 2>"$stderr" &
        guard_pid=$!
        wait "$guard_pid"
        exit_code=$?
        if [[ $exit_code -eq 124 || $exit_code -eq 137 ]]; then
            kill -TERM -- "-$guard_pid" 2>/dev/null || true
            sleep 0.2
            kill -KILL -- "-$guard_pid" 2>/dev/null || true
        fi
        set -e
        end=$(date +%s%3N)
        duration_ms=$((end - start))

        if [[ $exit_code -eq 0 ]]; then
            classification=pass
        elif [[ $exit_code -eq 124 || $exit_code -eq 137 ]]; then
            classification=timeout
        elif grep -Eqi 'unsupported syscall|not handling.*syscall|ENOSYS' "$stderr"; then
            classification=missing_syscall
        elif grep -Eqi 'verification failed|diverg|desync|does not match|mismatch' "$stderr"; then
            classification=non_determinism
        elif grep -Eqi 'panic|segmentation fault|SIGSEGV|fatal runtime error' "$stderr"; then
            classification=crash
        else
            classification=nonzero_exit
        fi

        printf '%s\t%s\t%s\t%s\t%s\n' \
            "$workload" "$mode" "$exit_code" "$duration_ms" "$classification" \
            >> "$ROOT/results.tsv"
        printf '%-8s %-6s exit=%-3s duration=%-6sms %s\n' \
            "$workload" "$mode" "$exit_code" "$duration_ms" "$classification"
    done
done
