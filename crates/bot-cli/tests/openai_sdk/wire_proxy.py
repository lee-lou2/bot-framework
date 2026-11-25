#!/usr/bin/env python3
"""투명 프록시: 두 공식 SDK의 `base_url`을 이 프록시로 두면 입구에 **실제로 간 요청**(SDK 재시도 포함)의 수·상태·핵심 헤더를 입구를 믿지 않고 센다. 돌아오는 모든 응답(JSON·SSE 조각·오류)은
공식 OpenAPI(고정 커밋)로 그 자리에서 검사한다 — 양 SDK·원시 요청 어느 쪽 출력이든 빠지지 않는다. 요청 본문과 키는 기록하지 않는다(경로·상태·고정 헤더 이름만). SSE는 받는 대로 흘려 보낸다.

사용: wire_proxy.py <입구 포트> [프록시 포트]   첫 줄에 프록시 포트를 찍는다. 검사하려면 WIRE_VALIDATE=1(jsonschema 필요).
조회: GET /__wire/log · GET /__wire/summary · POST /__wire/reset(기록) · POST /__wire/reset-all(집계까지). /v1 밖이라 입구와 겹치지 않는다."""
import http.client
import json
import os
import pathlib
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = ("127.0.0.1", 0)
LOG, SUMMARY = [], {"validated": {}, "violations": [], "statuses": {}, "sse": {"keepalive": 0, "done": 0}}
LOCK = threading.Lock()
REQ_HEADERS = ("idempotency-key", "x-stainless-retry-count", "content-type", "accept")
RES_HEADERS = ("idempotency-key", "x-request-id", "retry-after", "retry-after-ms", "x-should-retry", "content-type", "allow", "cache-control", "x-accel-buffering")
HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers", "transfer-encoding", "upgrade", "host", "content-length"}
CAP = 4 * 1024 * 1024
SCHEMA = None


def load_schema():
    global SCHEMA
    if os.environ.get("WIRE_VALIDATE") == "1":
        sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent / "py"))
        from conformance import Schema
        SCHEMA = Schema()


def check(entry, name, instance, where=""):
    with LOCK:
        SUMMARY["validated"][name] = SUMMARY["validated"].get(name, 0) + 1
    problems = SCHEMA.errors(name, instance)
    if problems:
        with LOCK:
            SUMMARY["violations"].append({"n": entry["n"], "method": entry["method"], "path": entry["path"], "schema": name + where, "problems": problems[:5]})


def validate(entry, ctype, body):
    """응답 하나를 경로·상태로 정한 스키마로 검사한다(SSE는 `data:` 한 줄씩)."""
    if SCHEMA is None:
        return
    path, status = entry["path"].split("?")[0], entry["status"]
    if status >= 400:
        name = "ErrorResponse"
    elif entry["method"] == "GET" and path == "/v1/models":
        name = "ListModelsResponse"
    elif entry["method"] == "GET" and path.startswith("/v1/models/"):
        name = "Model"
    elif entry["method"] == "POST" and path == "/v1/chat/completions":
        name = "stream" if ctype.startswith("text/event-stream") else "CreateChatCompletionResponse"
    else:
        return
    if name != "stream":
        try:
            check(entry, name, json.loads(body))
        except ValueError:
            with LOCK:
                SUMMARY["violations"].append({"n": entry["n"], "path": entry["path"], "schema": name, "problems": ["본문이 JSON이 아니다"]})
        return
    text = body.decode("utf-8", errors="replace")
    events = [e for e in text.split("\n\n") if e != ""]
    if not text.endswith("\n\n"):
        with LOCK:
            SUMMARY["violations"].append({"n": entry["n"], "path": entry["path"], "schema": "sse", "problems": ["마지막 이벤트가 빈 줄로 끝나지 않는다"]})
    for event in events:
        if event.startswith(":"):
            with LOCK:
                SUMMARY["sse"]["keepalive"] += 1
            if event != ": keep-alive":
                with LOCK:
                    SUMMARY["violations"].append({"n": entry["n"], "path": entry["path"], "schema": "sse", "problems": [f"주석 줄이 keep-alive가 아니다: {event[:40]}"]})
        elif event.startswith("data: ") and "\n" not in event:
            payload = event[len("data: "):]
            if payload == "[DONE]":
                with LOCK:
                    SUMMARY["sse"]["done"] += 1
                continue
            try:
                chunk = json.loads(payload)
            except ValueError:
                with LOCK:
                    SUMMARY["violations"].append({"n": entry["n"], "path": entry["path"], "schema": "sse", "problems": ["data가 JSON이 아니다"]})
                continue
            check(entry, "ErrorResponse" if "error" in chunk else "CreateChatCompletionStreamResponse", chunk, "(sse)")
        else:
            with LOCK:
                SUMMARY["violations"].append({"n": entry["n"], "path": entry["path"], "schema": "sse", "problems": [f"틀에 맞지 않는 이벤트: {event[:40]!r}"]})


class Proxy(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_wire(self):
        if self.path.startswith("/__wire/"):
            with LOCK:
                if self.path in ("/__wire/reset", "/__wire/reset-all"):
                    LOG.clear()  # reset은 기록만 비운다(시험마다), reset-all은 검사 집계까지(실행을 시작할 때 한 번)
                    if self.path.endswith("-all"):
                        SUMMARY.update({"validated": {}, "violations": [], "statuses": {}, "sse": {"keepalive": 0, "done": 0}})
                    return self.reply(200, b"{}")
                return self.reply(200, json.dumps(LOG if self.path == "/__wire/log" else {**SUMMARY, "requests": len(LOG)}).encode())
        chunked = (self.headers.get("Transfer-Encoding") or "").lower() == "chunked"
        if chunked:  # 청크 본문은 조각 그대로 읽어 같은 모양으로 넘긴다(입구가 실제로 받은 바이트로 한도를 세는지 보려고)
            pieces = []
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    self.rfile.readline()
                    break
                pieces.append(self.rfile.read(size))
                self.rfile.readline()
            body = b"".join(pieces)
        else:
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else b""
        headers = [(k, v) for k, v in self.headers.items() if k.lower() not in HOP]  # 같은 이름의 헤더 줄이 겹쳐도 그대로 전달한다
        entry = {"method": self.command, "path": self.path, "req": {h: self.headers.get(h) for h in REQ_HEADERS if self.headers.get(h) is not None}}
        with LOCK:
            entry["n"] = len(LOG) + 1
            LOG.append(entry)
        try:
            upstream = http.client.HTTPConnection(*UPSTREAM, timeout=600)
            upstream.putrequest(self.command, self.path, skip_host=True, skip_accept_encoding=True)
            for name, value in [("Host", "%s:%d" % UPSTREAM)] + headers:
                upstream.putheader(name, value)
            if chunked:
                upstream.putheader("Transfer-Encoding", "chunked")
                upstream.endheaders()
                for piece in pieces:
                    upstream.send(b"%x\r\n%s\r\n" % (len(piece), piece))
                upstream.send(b"0\r\n\r\n")
            else:
                upstream.putheader("Content-Length", str(len(body)))
                upstream.endheaders(body or None)
            reply = upstream.getresponse()
        except OSError:
            entry["status"] = 0
            return self.reply(502, b'{"error":{"message":"proxy","type":"proxy","param":null,"code":"proxy"}}')
        entry["status"] = reply.status
        entry["res"] = {h: reply.getheader(h) for h in RES_HEADERS if reply.getheader(h) is not None}
        with LOCK:
            SUMMARY["statuses"][str(reply.status)] = SUMMARY["statuses"].get(str(reply.status), 0) + 1
        self.send_response(reply.status)
        for key, value in reply.getheaders():
            if key.lower() not in HOP:
                self.send_header(key, value)
        self.send_header("Connection", "close")
        self.end_headers()
        kept, size = [], 0
        try:
            while True:  # 받는 대로 흘려 보낸다(SSE 지연 없음)
                chunk = reply.read1(65536) if hasattr(reply, "read1") else reply.read(65536)
                if not chunk:
                    break
                if size < CAP:
                    kept.append(chunk)
                    size += len(chunk)
                self.wfile.write(chunk)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            entry["client_closed"] = True  # 클라이언트가 먼저 끊었다: 끝까지 못 읽은 본문은 검사하지 않는다
        else:
            validate(entry, reply.getheader("content-type") or "", b"".join(kept))
        self.close_connection = True
        upstream.close()

    def reply(self, status, body):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = do_wire


def main():
    global UPSTREAM
    UPSTREAM = ("127.0.0.1", int(sys.argv[1]))
    load_schema()
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[2]) if len(sys.argv) > 2 else 0), Proxy)
    print(server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
