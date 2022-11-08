#!/usr/bin/env python3
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

import http.server
import ssl
import sys

httpd = http.server.HTTPServer(("0.0.0.0", 0), http.server.SimpleHTTPRequestHandler)
httpd.socket = ssl.wrap_socket(
    httpd.socket,
    certfile="/var/facebook/x509_identities/server.pem",
    ca_certs="/var/facebook/rootcanal/ca.pem",
    server_side=True,
)

requests = -1
if len(sys.argv) > 1:
    requests = int(sys.argv[1])
    sys.stderr.write("[server] Answering only " + str(requests) + " requests.\n")


print("{}:{}".format(httpd.server_name, httpd.server_port), flush=True)
if requests > 0:
    for _i in range(0, requests):
        httpd.handle_request()
else:
    httpd.serve_forever()
