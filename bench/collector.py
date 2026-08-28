#!/usr/bin/env python3
"""A minimal OTLP/HTTP collector for bench/tracing.sh.

It is deliberately not an OpenTelemetry Collector: this has to *assert* what
arrived, so it appends each request body verbatim to a file and answers 200.
A real collector would accept the batch and tell us nothing about its shape.
"""
import http.server
import sys

OUT = sys.argv[2]


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        # The content type is part of the OTLP/HTTP contract; recording it
        # means the benchmark can fail on a batch a real collector would
        # reject outright rather than on one it merely disliked.
        with open(OUT, "ab") as f:
            f.write(self.headers.get("Content-Type", "").encode() + b"\t" + body + b"\n")
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
