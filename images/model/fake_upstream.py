#!/usr/bin/env python3
"""모델 VM 이미지 점검용 가짜 상류(`verify.sh`가 쓴다). 실제 모델도 실제 키도 쓰지 않는다.

`POST /v1/messages`에 고정한 한 줄 답(스트리밍이면 SSE)만 하고, 받은 요청마다 한 줄 JSON(메서드·경로·헤더 유무·크기)을 표준 출력에 남긴다.
그 밖의 경로는 404, `CONNECT`(프록시로 쓰인 경우)는 403이다 — 점검은 이 기록으로 CLI가 `/v1/messages` 말고 다른 곳에 접속하려 하지 않았는지 본다.

사용: fake_upstream.py <바인드할 주소> <포트>   (호스트 전용 망의 게이트웨이 주소에만 바인드한다. 0.0.0.0 금지)
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
        # 프록시로 쓰였을 때(점검이 HTTPS_PROXY를 이 서버로 준다): CLI가 다른 호스트에 접속하려 한 시도를 기록하고 거절한다.
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
        pass  # 거절한 접속이 끊기며 생기는 오류는 기록에 섞지 않는다


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    Server((sys.argv[1], int(sys.argv[2])), Handler).serve_forever()
