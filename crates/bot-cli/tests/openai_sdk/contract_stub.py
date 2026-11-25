#!/usr/bin/env python3
"""OpenAI 호환 입구 1단계 계약의 «기준 동작» 대역(표준 라이브러리만). 진짜 입구(PG·게이트웨이) 없이 SDK 종단 시험 자체를 검증하려고 만든다 — 시험이 계약을 어긴 구현을 잡는지
(변이 확인) 볼 때만 쓰고, 제품 시험(`openai_compat`)은 이 대역을 쓰지 않는다. 오류 표·요청 필드 표·멱등성·대기·SSE·usage 규칙은 합의 계약(docs/gateway.md의 OpenAI 호환 API)을 따른다.

환경: STUB_PROFILE=main(기본, 대기 120 s·동시 32·본문 256 KiB) | tight(대기 0.5 s·동시 1·본문 4 KiB) · STUB_MUTANT(변이 확인 전용) · 포트는 첫 인자.
시험 제어(모델이 다음 요청에 할 일): POST /__ctl/script {"text","usage":[입력,출력]|null,"stop":"end_turn"|"max_tokens","delay_ms","fail","approval":true(사람 승인이 필요한 도구를 부르려 한다)} — 요청 순서대로 한 번씩 쓴다. GET /__ctl/stats → {"runs","model_calls","tool_runs","active"}."""
import hashlib
import json
import os
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PROFILE = os.environ.get("STUB_PROFILE", "main")
WAIT_MS, MAX_ACTIVE, BODY_LIMIT = {"main": (120000, 32, 256 * 1024), "tight": (3000, 1, 4096)}[PROFILE]
KEEPALIVE_S = float(os.environ.get("STUB_KEEPALIVE_S", "15"))
MUTANT = os.environ.get("STUB_MUTANT", "")
CREATED = 1700000000
HOST_CAP = 12000  # 최종 글의 호스트 상한(글자 수)
# 키 → (주체, 멤버인가). 보이는 모델은 주체가 정한다(build만 assistant, secret-agent는 있지만 build의 역할 밖).
KEYS = {"build-api-key": ("build", True), "rotated-api-key": ("build", True), "other-api-key": ("ops", True), "guest-api-key": ("guest", False)}
VISIBLE = {"build": {"assistant"}, "ops": {"assistant"}}
EXISTING = ["assistant", "secret-agent"]
STATE = {"runs": {}, "model_calls": 0, "tool_runs": 0, "tool_reads": 0, "scripts": []}
# 요청에서 null이 «보내지 않음»과 같은 필드: 공식 요청 스키마의 nullable(고정 스키마 조각이 담고 있다). 그 밖의 알 수 없거나 지원하지 않는 필드는 null이어도 거절한다.
NULLABLE = set(json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "schema", "openai-subset.json")))["x-source"]["request_nullable"])
LOCK = threading.RLock()  # 접수가 잠근 채 Run을 만들어 계수를 올린다(같은 스레드가 다시 잠근다)
NOTICE_ACTIVE = "작업을 처리 중입니다. 같은 Idempotency-Key와 같은 요청으로 다시 조회하세요."
ROLES = {"system", "developer", "user", "assistant"}
UNSUPPORTED = {"tools", "tool_choice", "functions", "function_call", "parallel_tool_calls", "logprobs", "top_logprobs", "audio", "modalities", "prediction", "web_search_options",
               "max_tokens", "max_completion_tokens", "stop", "frequency_penalty", "presence_penalty", "logit_bias", "reasoning_effort", "service_tier", "safety_identifier"}
KNOWN = {"model", "messages", "stream", "stream_options", "n", "temperature", "top_p", "seed", "user", "metadata", "store", "response_format"}


class Reject(Exception):
    def __init__(self, status, kind, code, message, param=None, headers=None):
        super().__init__(message)
        self.status, self.kind, self.code, self.message, self.param, self.headers = status, kind, code, message, param, headers or {}


def invalid(message, param=None, code="invalid_request"):
    return Reject(400, "invalid_request_error", code, message, param)


def unsupported(param):
    """`param`은 등록된 미지원 옵션이면 그 필드 경로, 최상위 미등록 키면 null, 중첩 미등록 키면 가장 가까운 알려진 부모 경로다 — 호출자가 지어낸 이름·값은 반사하지 않는다."""
    return Reject(400, "invalid_request_error", "unsupported_parameter", "지원하지 않는 필드입니다.", param)


def pairs_to_dict(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError("duplicate key")
        out[key] = value
    return out


class Run:
    def __init__(self, principal, fingerprint, model, script):
        self.id, self.created, self.principal, self.fingerprint, self.model = str(uuid.uuid4()), int(time.time()), principal, fingerprint, model
        self.done, self.state, self.text, self.finish, self.usage = threading.Event(), "active", "", "stop", None
        with LOCK:
            STATE["model_calls"] += 1
            STATE["tool_reads"] += 1 if script.get("tool_call") else 0
        threading.Thread(target=self.execute, args=(script,), daemon=True).start()

    def execute(self, script):
        time.sleep(script.get("delay_ms", 0) / 1000)
        earlier_limit = False
        if script.get("stop") == "max_tokens" and script.get("then"):
            earlier_limit, script = True, script["then"]  # 앞선 차례가 제한으로 끝났어도 정상 최종 답이 있으면 그 답이 결과다(length를 만들지 않는다)
        if script.get("fail") or (script.get("stop") == "max_tokens" and MUTANT != "limit_as_stop"):  # 공급자 제한 종료로 최종 답을 못 남겼다: 실패(성공 문장으로 포장하지 않는다)
            self.state = "failed"
        elif script.get("approval"):
            # 승인할 사람이 없는 HTTP 호출: 도구는 실행하지 않고, 모델이 성공이라고 말하려 해도 호스트가 정한 안내로 답한다
            if MUTANT == "approval_bypass":
                with LOCK:
                    STATE["tool_runs"] += 1
                self.text = script.get("text", "")
            else:
                self.text = "승인할 사람이 정해져 있지 않아 도구를 실행하지 않았습니다."
            self.state = "final"
        else:
            self.text, self.finish = script.get("text", "e2e-answer-ok"), "stop"
            if len(self.text) > HOST_CAP and MUTANT != "no_clip":  # 호스트 상한을 넘은 최종 글은 잘리고 length다(조용히 자르지 않는다)
                self.text, self.finish = self.text[:HOST_CAP], ("stop" if MUTANT == "clip_as_stop" else "length")
            if earlier_limit and MUTANT == "earlier_limit_as_length":
                self.finish = "length"
            counts = script.get("usage", [9, 4])
            if script.get("tool_call"):  # 한 실행 안의 두 모델 차례: 첫 차례(기본 5/4)와 마지막 차례가 모두 보고됐을 때만 합이 계량이다
                first = script.get("first_usage", [5, 4])
                counts = [first[0] + counts[0], first[1] + counts[1]] if first is not None and counts is not None else None
                if MUTANT == "usage_partial_sum" and counts is None:  # 일부 차례만 보고됐는데 보고된 쪽만 합산해 내보낸다
                    seen = first or script.get("usage", [9, 4])
                    counts = seen
            if counts is not None:
                self.usage = {"prompt_tokens": counts[0], "completion_tokens": counts[1], "total_tokens": counts[0] + counts[1]}
            elif MUTANT == "usage_zero_filled":
                self.usage = {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
            self.state = "final"
        self.done.set()


def active_runs():
    return sum(1 for r in STATE["runs"].values() if not r.done.is_set())


def validate(body):
    for name in body:
        if (name in UNSUPPORTED and not (MUTANT == "max_tokens_accepted" and name == "max_tokens")) or name not in KNOWN | ({"max_tokens"} if MUTANT == "max_tokens_accepted" else set()):
            if body[name] is None and name in NULLABLE:
                continue  # 공식 스키마가 null을 허용하는 선택 필드의 null은 생략과 같다
            raise unsupported(name if name in UNSUPPORTED else (name if MUTANT == "echo_unknown_name" else None))
    model, messages = body.get("model"), body.get("messages")
    if not isinstance(model, str) or not model:
        raise invalid("model이 필요합니다.", "model")
    if not isinstance(messages, list) or not messages:
        raise invalid("messages는 비어 있지 않은 배열이어야 합니다.", "messages")
    for i, message in enumerate(messages):
        if not isinstance(message, dict) or not isinstance(message.get("role"), str):
            raise invalid("메시지 형태가 맞지 않습니다.", f"messages[{i}]")
        if message["role"] in ("tool", "function"):
            raise unsupported(f"messages[{i}].role")  # 공식 역할이지만 이 서버는 도구 역할을 받지 않는다
        if message["role"] not in ROLES:
            raise invalid("알 수 없는 역할입니다.", f"messages[{i}].role")  # 공식 역할이 아니다: 요청 형태 오류
        for name in message:
            if name not in ("role", "content", "name"):
                raise unsupported(f"messages[{i}]")  # 중첩 미등록 키: 가장 가까운 알려진 부모 경로
        content = message.get("content")
        if isinstance(content, list):
            for j, part in enumerate(content):
                if not isinstance(part, dict) or part.get("type") != "text" or set(part) - {"type", "text"}:
                    raise unsupported(f"messages[{i}].content[{j}]")
        elif not isinstance(content, str):
            raise invalid("content는 글이어야 합니다.", f"messages[{i}].content")
    if not any(m["role"] == "user" for m in messages):
        raise invalid("user 메시지가 하나 이상 필요합니다.", "messages")
    if body.get("n") not in (None, 1) and MUTANT != "n_ignored":
        raise unsupported("n")
    if body.get("store") not in (None, False):
        raise unsupported("store")
    if body.get("response_format") not in (None, {"type": "text"}):
        raise unsupported("response_format")
    stream, options = body.get("stream"), body.get("stream_options")
    if stream is not None and not isinstance(stream, bool):
        raise invalid("stream은 true 또는 false여야 합니다.", "stream")
    if options is not None:
        if not stream:
            raise invalid("stream_options는 stream이 true일 때만 씁니다.", "stream_options")
        if not isinstance(options, dict) or set(options) - {"include_usage"} or not isinstance(options.get("include_usage", False), bool):
            raise invalid("stream_options 형태가 맞지 않습니다.", "stream_options")
    for name, kind in (("temperature", (int, float)), ("top_p", (int, float)), ("seed", int), ("user", str), ("metadata", dict)):
        if body.get(name) is not None and (not isinstance(body[name], kind) or isinstance(body[name], bool)):
            raise invalid(f"{name} 형태가 맞지 않습니다.", name)


def completion(run, content, finish, usage):
    choice = {"index": 0, "message": {"role": "assistant", "content": content, "refusal": None}, "finish_reason": finish, "logprobs": None}
    if MUTANT == "no_logprobs":
        del choice["logprobs"]
    out = {"id": "chatcmpl-" + run.id, "object": "chat.completion", "created": run.created if MUTANT != "created_now" else int(time.time()), "model": run.model, "choices": [choice]}
    if usage is not None:
        out["usage"] = usage
    return out


def model_object(model):
    return {"id": model, "object": "model", "created": CREATED if MUTANT != "created_now" else int(time.time()), "owned_by": "bot-framework"}


def model_not_found():
    return Reject(404, "invalid_request_error", "model_not_found", "없는 모델이거나 접근할 수 없습니다.")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send_json(self, status, body, headers=None):
        raw = json.dumps(body, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(raw)

    def fail(self, reject):
        error = {"message": reject.message, "type": reject.kind, "param": reject.param, "code": reject.code}
        if MUTANT == "error_without_param":
            del error["param"]
        self.send_json(reject.status, {"error": error}, reject.headers)

    def authenticate(self):
        values = self.headers.get_all("Authorization") or []
        if len(values) != 1 or not values[0].startswith("Bearer ") or not values[0][7:].strip():
            raise Reject(401, "authentication_error", "invalid_api_key", "API 키가 올바르지 않습니다.")
        key = values[0][7:].strip()
        if key not in KEYS and not (MUTANT == "any_key" and self.command == "POST"):
            raise Reject(401, "authentication_error", "invalid_api_key", f"API 키 {key}가 올바르지 않습니다." if MUTANT == "key_echo" else "API 키가 올바르지 않습니다.")
        subject, member = KEYS.get(key, KEYS["build-api-key"])
        if not member:
            raise Reject(403, "permission_error", "access_denied", "이 키는 접근할 수 없습니다.")
        return subject

    def read_body(self):
        """본문을 읽는다. 선언 길이도 실제로 받은 바이트도 한도를 넘으면 413(읽지 않고 연결을 닫는다)."""
        too_large = Reject(413, "invalid_request_error", "request_too_large", "요청 본문이 너무 큽니다.")
        if (self.headers.get("Transfer-Encoding") or "").lower() == "chunked":
            out = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    self.rfile.readline()
                    if len(out) > BODY_LIMIT and MUTANT != "no_body_limit":
                        raise too_large  # 실제로 받은 바이트로 센다(끝까지 읽은 뒤)
                    return out
                out += self.rfile.read(size)
                self.rfile.readline()
                if len(out) > 1024 * 1024:
                    self.close_connection = True
                    raise too_large
        length = int(self.headers.get("Content-Length") or 0)
        if length > BODY_LIMIT and MUTANT != "no_body_limit":
            self.close_connection = True
            raise too_large
        return self.rfile.read(length) if length else b""

    def control(self, path, raw):
        with LOCK:
            if path.endswith("/stats") and self.command == "GET":
                return self.send_json(200, {"runs": len(STATE["runs"]), "model_calls": STATE["model_calls"], "tool_runs": STATE["tool_runs"], "tool_reads": STATE["tool_reads"], "active": active_runs(), "queued": len(STATE["scripts"])})
            if path.endswith("/script") and self.command == "POST":
                STATE["scripts"].append(json.loads(raw))
                return self.send_json(200, {"queued": len(STATE["scripts"])})
        self.send_json(404, {})

    def route(self):
        path = self.path.split("?")[0]
        raw = self.read_body()
        if path.startswith("/__ctl/"):
            return self.control(path, raw)
        known = path in ("/v1/models", "/v1/chat/completions") or path.startswith("/v1/models/")
        if not known:
            self.authenticate() if MUTANT != "unauth_404" else None
            raise Reject(404, "invalid_request_error", "not_found", "없는 경로입니다.")
        subject = ({"build"} and "build") if MUTANT == "no_auth_models" and path.startswith("/v1/models") else self.authenticate()
        allowed = ("POST",) if path == "/v1/chat/completions" else ("GET",)
        if self.command not in allowed:
            raise Reject(405, "invalid_request_error", "method_not_allowed", "허용하지 않는 메서드입니다.", headers={"Allow": ", ".join(allowed)})
        visible = VISIBLE.get(subject, set())
        if path == "/v1/models":
            return self.send_json(200, {"object": "list", "data": [model_object(m) for m in sorted(EXISTING) if MUTANT == "list_unfiltered" or m in visible]})
        if path.startswith("/v1/models/"):
            model = path[len("/v1/models/"):]
            if model not in EXISTING or (model not in visible and MUTANT != "forbidden_differs"):
                raise model_not_found()
            if model not in visible:
                raise Reject(403, "permission_error", "access_denied", "접근할 수 없는 모델입니다.")
            return self.send_json(200, model_object(model))
        if not (self.headers.get("Content-Type") or "").lower().startswith("application/json"):
            raise Reject(415, "invalid_request_error", "unsupported_media_type", "JSON만 받습니다.")
        try:
            body = json.loads(raw, object_pairs_hook=pairs_to_dict)
        except ValueError:
            raise invalid("JSON을 읽을 수 없거나 같은 키가 겹칩니다.")
        if not isinstance(body, dict):
            raise invalid("본문은 JSON 객체여야 합니다.")
        validate(body)
        if body["model"] not in EXISTING or (body["model"] not in visible and MUTANT != "chat_forbidden_allowed"):
            raise model_not_found()
        self.chat(subject, body)

    def chat(self, principal, body):
        explicit = self.headers.get("Idempotency-Key")
        retried = int(self.headers.get("x-stainless-retry-count") or 0) > 0
        if explicit is None and retried and MUTANT != "no_retry_marker":
            raise invalid("키 없는 재전송은 받지 않습니다. Idempotency-Key를 보내세요.", code="idempotency_key_required")
        key = explicit or uuid.uuid4().hex
        digest = hashlib.sha256(json.dumps({"model": body["model"], "messages": body["messages"]}, sort_keys=True).encode()).hexdigest()
        slot = (principal if MUTANT != "shared_keys" else "*", key)
        with LOCK:
            run = STATE["runs"].get(slot)
            if run is not None and run.fingerprint != digest and MUTANT != "no_conflict":
                raise invalid("같은 키에 다른 요청입니다.", code="idempotency_key_conflict")
            if run is None or MUTANT == "no_idempotency":
                if active_runs() >= MAX_ACTIVE:
                    status = {"saturated_500": 500, "saturated_409": 409}.get(MUTANT, 429)
                    headers = {} if MUTANT == "saturated_no_retry_after" else {"Retry-After": "1"}
                    raise Reject(status, "rate_limit_error", "rate_limit_exceeded", "실행 한도에 닿았습니다.", headers=headers)
                script = STATE["scripts"].pop(0) if STATE["scripts"] else {}
                run = Run(principal, digest, body["model"], script)
                STATE["runs"][slot] = run
        headers = {"Idempotency-Key": key, "x-request-id": "req-" + uuid.uuid4().hex[:16]}
        include_usage = bool((body.get("stream_options") or {}).get("include_usage"))
        if body.get("stream"):
            return self.stream(run, headers, include_usage)
        reserve = min(2.0, WAIT_MS / 4000)
        if not run.done.wait(WAIT_MS / 1000 - reserve if MUTANT != "wait_forever" else 3600):
            return self.send_json(200, completion(run, NOTICE_ACTIVE, "stop", None), headers)
        if run.state == "failed":
            retry_headers = {"x-should-retry": "false"} if MUTANT not in ("failed_status_500", "failed_status_429") else {}
            status = {"failed_status_500": 500, "failed_status_429": 429}.get(MUTANT, 422)
            return self.fail(Reject(status, "server_error", "run_failed", "작업이 실패했습니다.", headers={**headers, **retry_headers}))
        self.send_json(200, completion(run, run.text, run.finish, run.usage), headers)

    def stream(self, run, headers, include_usage):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        if MUTANT != "no_cache_control":
            self.send_header("Cache-Control", "no-cache")
        self.send_header("X-Accel-Buffering", "no")
        for key, value in headers.items():
            self.send_header(key, value)
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

        def emit(choices, usage="absent"):
            chunk = {"id": "chatcmpl-" + run.id, "object": "chat.completion.chunk", "created": run.created, "model": run.model, "choices": choices}
            if usage != "absent":
                chunk["usage"] = usage
            elif include_usage or MUTANT == "usage_key_always":
                chunk["usage"] = None
            self.wfile.write(b"data: " + json.dumps(chunk, ensure_ascii=False).encode() + b"\n\n")
            self.wfile.flush()

        emit([{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}])
        deadline = time.monotonic() + WAIT_MS / 1000 - min(2.0, WAIT_MS / 4000)
        while not run.done.is_set() and time.monotonic() < deadline:
            run.done.wait(min(KEEPALIVE_S, max(0.05, deadline - time.monotonic())))
            if not run.done.is_set() and time.monotonic() < deadline and MUTANT != "no_keepalive":
                self.wfile.write(b": keep-alive\n\n")
                self.wfile.flush()
        if not run.done.is_set():
            emit([{"index": 0, "delta": {"content": NOTICE_ACTIVE}, "finish_reason": None}])
            emit([{"index": 0, "delta": {}, "finish_reason": "stop"}])
        elif run.state == "failed":
            error = {"error": {"message": "작업이 실패했습니다.", "type": "server_error", "param": None, "code": "run_failed"}}
            self.wfile.write(b"data: " + json.dumps(error, ensure_ascii=False).encode() + b"\n\n")
            self.wfile.flush()
            return
        else:
            for i in range(0, len(run.text), 8):
                emit([{"index": 0, "delta": {"content": run.text[i:i + 8]}, "finish_reason": None}])
            emit([{"index": 0, "delta": {}, "finish_reason": run.finish}])
            if include_usage and (run.usage is not None or MUTANT == "usage_chunk_unknown"):
                emit([], usage=run.usage if run.usage is not None else {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})
        if MUTANT != "no_done":
            self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def handle_any(self):
        try:
            self.route()
        except Reject as reject:
            self.fail(reject)
        except (BrokenPipeError, ConnectionResetError):
            pass

    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = handle_any


def main():
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 0), Handler)
    print(server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
