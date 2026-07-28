# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

import http.server
import secrets
import sys
import threading
import time
import urllib.request


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        payload = (
            f"path={self.path} time={time.time_ns()} "
            f"token={secrets.token_hex(8)}\n"
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


port = int(sys.argv[1])
server = http.server.HTTPServer(("127.0.0.1", port), Handler)
server.timeout = 10
thread = threading.Thread(target=server.handle_request)
thread.start()

opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
with opener.open(f"http://127.0.0.1:{port}/frontier", timeout=10) as response:
    print(f"status={response.status} {response.read().decode().strip()}")

thread.join()
server.server_close()
