#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail
export LC_ALL=C
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export TZ=UTC

function run_http_workload {
    local work_dir=$1
    local port=$2
    local server_pid='' response_hash

    rm -rf -- "$work_dir"
    mkdir -p -- "$work_dir"
    trap 'if [[ -n ${server_pid:-} ]]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi' EXIT

    cat >"$work_dir/server.py" <<'PY'
import http.server
import os
import pathlib
import sys
import time


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/payload":
            self.send_error(404)
            return
        body = (
            "hermit-http-server\n"
            f"observed-ns={time.time_ns()}\n"
            f"nonce={os.urandom(16).hex()}\n"
        ).encode("ascii")
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
pathlib.Path(sys.argv[2]).write_text("ready\n", encoding="ascii")
server.handle_request()
server.server_close()
PY

    python3 "$work_dir/server.py" "$port" "$work_dir/ready" \
        >"$work_dir/server.log" 2>&1 &
    server_pid=$!

    for _ in $(seq 1 500); do
        [[ -s $work_dir/ready ]] && break
        if ! kill -0 "$server_pid" 2>/dev/null; then
            wait "$server_pid" || true
            server_pid=
            cat "$work_dir/server.log" >&2
            return 1
        fi
        sleep 0.01
    done
    if [[ ! -s $work_dir/ready ]]; then
        cat "$work_dir/server.log" >&2
        return 1
    fi

    curl --fail --silent --show-error \
        "http://127.0.0.1:$port/payload" >"$work_dir/response.txt"
    wait "$server_pid"
    server_pid=

    [[ $(sed -n '1p' "$work_dir/response.txt") == hermit-http-server ]]
    [[ $(wc -l <"$work_dir/response.txt") -eq 3 ]]
    response_hash=$(sha256sum "$work_dir/response.txt" | cut -d' ' -f1)
    printf 'http-server:%s\n' "$response_hash"
}

if [[ ${1:-} == --guest ]]; then
    run_http_workload "$2" "$3"
    exit
fi

# shellcheck source=tests/e2e/lib/applications/common.sh
source "$(dirname -- "$0")/common.sh"
require_commands curl python3 sed seq sha256sum timeout

work_root=$(mktemp -d "${TMPDIR:-/tmp}/hermit-http-e2e.XXXXXX")
trap 'rm -rf -- "$work_root"' EXIT
port=$((20000 + $$ % 20000))

native_first=$(run_http_workload "$work_root/native" "$port")
native_second=$(run_http_workload "$work_root/native" "$port")
assert_native_nondeterminism 'HTTP server workload' "$native_first" "$native_second"

run_hermit_verify 'HTTP server workload' \
    /bin/bash "$0" --guest "$work_root/verified" "$port" >/dev/null
printf 'http-server:verified\n'
