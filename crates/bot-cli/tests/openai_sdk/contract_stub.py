#!/usr/bin/env python3
"""Reference stub for the OpenAI-compatible gateway contract (stdlib only).
Used solely to self-test the SDK suites and verify mutations (STUB_MUTANT); product tests (`openai_compat`) run against the real gateway.

Env: STUB_PROFILE=main|tight, STUB_MUTANT=<name>, STUB_KEEPALIVE_S=15; optional port as argv[1].
Control: POST /__ctl/script {"text","usage":[in,out]|null,"stop":"end_turn"|"max_tokens","delay_ms","fail","approval":true}, GET /__ctl/stats."""
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
HOST_CAP = 12000
# API key -> (subject, is_member). Visibility is per subject; secret-agent exists but is outside build's role.
KEYS = {"build-api-key": ("build", True), "rotated-api-key": ("build", True), "other-api-key": ("ops", True), "guest-api-key": ("guest", False)}
VISIBLE = {"build": {"assistant"}, "ops": {"assistant"}}
EXISTING = ["assistant", "secret-agent"]
STATE = {"runs": {}, "model_calls": 0, "tool_runs": 0, "tool_reads": 0, "scripts": []}
# Request fields where null means omitted, derived from the pinned OpenAPI request schema.
NULLABLE = set(json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "schema", "openai-subset.json")))["x-source"]["request_nullable"])
LOCK = threading.RLock()  # Reentrant because intake creates Run while holding LOCK.
NOTICE_ACTIVE = "Request is still in progress. Query again with the same Idempotency-Key and request."
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
    """`param` is the known unsupported field path, null for top-level unknown keys, or nearest known parent for nested unknown keys (never echoes caller names)."""
    return Reject(400, "invalid_request_error", "unsupported_parameter", "This parameter or value is not supported by this server.", param)


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
            earlier_limit, script = True, script["then"]
        if script.get("fail") or (script.get("stop") == "max_tokens" and MUTANT != "limit_as_stop"):
            self.state = "failed"
        elif script.get("approval"):
            if MUTANT == "approval_bypass":
                with LOCK:
                    STATE["tool_runs"] += 1
                self.text = script.get("text", "")
            else:
                self.text = "No approver is configured. The action requiring approval was not executed."
            self.state = "final"
        else:
            self.text, self.finish = script.get("text", "e2e-answer-ok"), "stop"
            if len(self.text) > HOST_CAP and MUTANT != "no_clip":
                self.text, self.finish = self.text[:HOST_CAP], ("stop" if MUTANT == "clip_as_stop" else "length")
            if earlier_limit and MUTANT == "earlier_limit_as_length":
                self.finish = "length"
            counts = script.get("usage", [9, 4])
            if script.get("tool_call"):  # Multi-turn run: sum tokens only when both turns report usage.
                first = script.get("first_usage", [5, 4])
                counts = [first[0] + counts[0], first[1] + counts[1]] if first is not None and counts is not None else None
                if MUTANT == "usage_partial_sum" and counts is None:
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
                continue
            raise unsupported(name if name in UNSUPPORTED else (name if MUTANT == "echo_unknown_name" else None))
    model, messages = body.get("model"), body.get("messages")
    if not isinstance(model, str) or not model:
        raise invalid("model is required.", "model")
    if not isinstance(messages, list) or not messages:
        raise invalid("messages must be a non-empty array.", "messages")
    for i, message in enumerate(messages):
        if not isinstance(message, dict) or not isinstance(message.get("role"), str):
            raise invalid("invalid message object.", f"messages[{i}]")
        if message["role"] in ("tool", "function"):
            raise unsupported(f"messages[{i}].role")
        if message["role"] not in ROLES:
            raise invalid("unknown message role.", f"messages[{i}].role")
        for name in message:
            if name not in ("role", "content", "name"):
                raise unsupported(f"messages[{i}]")
        content = message.get("content")
        if isinstance(content, list):
            for j, part in enumerate(content):
                if not isinstance(part, dict) or part.get("type") != "text" or set(part) - {"type", "text"}:
                    raise unsupported(f"messages[{i}].content[{j}]")
        elif not isinstance(content, str):
            raise invalid("content must be a string.", f"messages[{i}].content")
    if not any(m["role"] == "user" for m in messages):
        raise invalid("at least one user message is required.", "messages")
    if body.get("n") not in (None, 1) and MUTANT != "n_ignored":
        raise unsupported("n")
    if body.get("store") not in (None, False):
        raise unsupported("store")
    if body.get("response_format") not in (None, {"type": "text"}):
        raise unsupported("response_format")
    stream, options = body.get("stream"), body.get("stream_options")
    if stream is not None and not isinstance(stream, bool):
        raise invalid("stream must be a boolean.", "stream")
    if options is not None:
        if not stream:
            raise invalid("stream_options requires stream=true.", "stream_options")
        if not isinstance(options, dict) or set(options) - {"include_usage"} or not isinstance(options.get("include_usage", False), bool):
            raise invalid("invalid stream_options.", "stream_options")
    for name, kind in (("temperature", (int, float)), ("top_p", (int, float)), ("seed", int), ("user", str), ("metadata", dict)):
        if body.get(name) is not None and (not isinstance(body[name], kind) or isinstance(body[name], bool)):
            raise invalid(f"invalid {name}.", name)


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
    return Reject(404, "invalid_request_error", "model_not_found", "Model does not exist or is not accessible with this key.")


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
            raise Reject(401, "authentication_error", "invalid_api_key", "Missing or invalid API key.")
        key = values[0][7:].strip()
        if key not in KEYS and not (MUTANT == "any_key" and self.command == "POST"):
            raise Reject(401, "authentication_error", "invalid_api_key", f"Invalid API key {key}." if MUTANT == "key_echo" else "Missing or invalid API key.")
        subject, member = KEYS.get(key, KEYS["build-api-key"])
        if not member:
            raise Reject(403, "permission_error", "access_denied", "Access denied for this API key.")
        return subject

    def read_body(self):
        too_large = Reject(413, "invalid_request_error", "request_too_large", "Request body exceeds size limit.")
        if (self.headers.get("Transfer-Encoding") or "").lower() == "chunked":
            out = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    self.rfile.readline()
                    if len(out) > BODY_LIMIT and MUTANT != "no_body_limit":
                        raise too_large
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
            raise Reject(404, "invalid_request_error", "not_found", "Unsupported endpoint path.")
        subject = ({"build"} and "build") if MUTANT == "no_auth_models" and path.startswith("/v1/models") else self.authenticate()
        allowed = ("POST",) if path == "/v1/chat/completions" else ("GET",)
        if self.command not in allowed:
            raise Reject(405, "invalid_request_error", "method_not_allowed", "HTTP method is not allowed for this path.", headers={"Allow": ", ".join(allowed)})
        visible = VISIBLE.get(subject, set())
        if path == "/v1/models":
            return self.send_json(200, {"object": "list", "data": [model_object(m) for m in sorted(EXISTING) if MUTANT == "list_unfiltered" or m in visible]})
        if path.startswith("/v1/models/"):
            model = path[len("/v1/models/"):]
            if model not in EXISTING or (model not in visible and MUTANT != "forbidden_differs"):
                raise model_not_found()
            if model not in visible:
                raise Reject(403, "permission_error", "access_denied", "Model is not accessible.")
            return self.send_json(200, model_object(model))
        if not (self.headers.get("Content-Type") or "").lower().startswith("application/json"):
            raise Reject(415, "invalid_request_error", "unsupported_media_type", "Content-Type must be application/json.")
        try:
            body = json.loads(raw, object_pairs_hook=pairs_to_dict)
        except ValueError:
            raise invalid("Invalid JSON or duplicate object keys.")
        if not isinstance(body, dict):
            raise invalid("Request body must be a JSON object.")
        validate(body)
        if body["model"] not in EXISTING or (body["model"] not in visible and MUTANT != "chat_forbidden_allowed"):
            raise model_not_found()
        self.chat(subject, body)

    def chat(self, principal, body):
        explicit = self.headers.get("Idempotency-Key")
        retried = int(self.headers.get("x-stainless-retry-count") or 0) > 0
        if explicit is None and retried and MUTANT != "no_retry_marker":
            raise invalid("Retried requests require an Idempotency-Key.", code="idempotency_key_required")
        key = explicit or uuid.uuid4().hex
        digest = hashlib.sha256(json.dumps({"model": body["model"], "messages": body["messages"]}, sort_keys=True).encode()).hexdigest()
        slot = (principal if MUTANT != "shared_keys" else "*", key)
        with LOCK:
            run = STATE["runs"].get(slot)
            if run is not None and run.fingerprint != digest and MUTANT != "no_conflict":
                raise invalid("A different request already exists for this Idempotency-Key.", code="idempotency_key_conflict")
            if run is None or MUTANT == "no_idempotency":
                if active_runs() >= MAX_ACTIVE:
                    status = {"saturated_500": 500, "saturated_409": 409}.get(MUTANT, 429)
                    headers = {} if MUTANT == "saturated_no_retry_after" else {"Retry-After": "1"}
                    raise Reject(status, "rate_limit_error", "rate_limit_exceeded", "Rate limit exceeded.", headers=headers)
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
            return self.fail(Reject(status, "server_error", "run_failed", "Run failed.", headers={**headers, **retry_headers}))
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
            error = {"error": {"message": "Run failed.", "type": "server_error", "param": None, "code": "run_failed"}}
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
