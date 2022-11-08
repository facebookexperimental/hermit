# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

import http.server
import socketserver
import sys
from http import HTTPStatus


class Hello(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        self.send_response(HTTPStatus.OK)
        self.end_headers()
        self.wfile.write(b"Hello world\n")


requests = -1
if len(sys.argv) > 1:
    requests = int(sys.argv[1])
    sys.stderr.write("[server] Answering only " + str(requests) + " requests.\n")

with socketserver.TCPServer(("", 0), Hello) as server:
    address, port = server.server_address
    print("{}:{}".format(address, port), flush=True)
    if requests > 0:
        for _i in range(0, requests):
            server.handle_request()
    else:
        server.serve_forever()
