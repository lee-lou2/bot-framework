#!/usr/bin/env python3
"""Fake upstream for model VM image verification (`verify.sh`). Uses no real model or API key.

Responds to `POST /v1/messages` with a fixed reply (SSE when streaming) and logs one JSON line
per request to stdout. Returns 404 for other paths and 403 for `CONNECT` (proxy attempts) so
verification can detect outbound requests to other hosts.

Usage: fake_upstream.py <bind-host> <port>  (bind only to the host-only network gateway; never 0.0.0.0)
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TEXT = "FAKE-OK"


def sse(name, data):
    return f"event: {name}\ndata: {json.dumps(data)}\n\n".encode()


def events(model):
    message = {"id": "msg_fake1", "type": "message", "role": "assistant", "model": model, "content": [],
               "stop_reason": None, "usage": {"input_tokens": 5, "output_tokens": 1}}
    return [
        sse("message_start", {"type": "message_start", "message": message}),
        sse("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        sse("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": TEXT}}),
        sse("content_block_stop", {"type": "content_block_stop", "index": 0}),
        sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 3}}),
        sse("message_stop", {"type": "message_stop"}),
    ]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def record(self, size):
        auth = self.headers.get("x-api-key") or self.headers.get("Authorization")
        print(json.dumps({"method": self.command, "path": self.path, "agent": self.headers.get("User-Agent"),
                          "has_auth": bool(auth), "bytes": size}), flush=True)

    def reply(self, status, body=b"", kind="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.record(0)
        self.reply(404)

    def do_CONNECT(self):
        # Record and reject proxy tunnel attempts when HTTPS_PROXY points here.
        self.record(0)
        self.reply(403)

    def do_POST(self):
        size = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(size) if size else b""
        self.record(len(body))
        if self.path.split("?")[0] != "/v1/messages":
            return self.reply(404)
        try:
            request = json.loads(body or b"{}")
        except ValueError:
            request = {}
        model = request.get("model", "fake-model")
        if not request.get("stream"):
            message = {"id": "msg_fake1", "type": "message", "role": "assistant", "model": model,
                       "content": [{"type": "text", "text": TEXT}], "stop_reason": "end_turn", "stop_sequence": None,
                       "usage": {"input_tokens": 5, "output_tokens": 3}}
            return self.reply(200, json.dumps(message).encode())
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        for event in events(model):
            self.wfile.write(f"{len(event):x}\r\n".encode() + event + b"\r\n")
        self.wfile.write(b"0\r\n\r\n")


class Server(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        pass  # Keep disconnect errors from rejected connections out of the log.


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    Server((sys.argv[1], int(sys.argv[2])), Handler).serve_forever()
