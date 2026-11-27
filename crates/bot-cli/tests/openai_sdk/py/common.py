"""Shared helpers for official openai Python SDK tests; fails fast with KeyError if required env vars are missing."""
import http.client
import json
import os
import time
import unittest
import urllib.parse
import urllib.request
import uuid

import openai
from openai import OpenAI

from conformance import Schema

BASE = os.environ["OPENAI_BASE_URL"].rstrip("/")
WIRE = os.environ["OPENAI_WIRE_URL"].rstrip("/")
CONTROL = os.environ["OPENAI_E2E_CONTROL"].rstrip("/")
KEY, ROTATED, GUEST, OTHER = (os.environ[n] for n in ("OPENAI_API_KEY", "OPENAI_ROTATED_KEY", "OPENAI_GUEST_KEY", "OPENAI_OTHER_KEY"))
BAD = os.environ.get("OPENAI_BAD_KEY", "wrong-api-key-0000000000")
MODEL = os.environ.get("OPENAI_MODEL", "assistant")
HIDDEN = os.environ.get("OPENAI_HIDDEN_MODEL", "secret-agent")
WAIT_MS = int(os.environ["OPENAI_WAIT_MS"])
SCHEMA = Schema()
USER = [{"role": "user", "content": "hello e2e"}]
ANSWER = "e2e-answer-ok"
NOTICE_PREFIX = "Request is still in progress."
NOTICE_RETRY = "Query again with the same Idempotency-Key and request."
RETRYABLE = {408, 409, 429}
HOST_CAP = 12000


def fresh_key():
    return "e2e-" + uuid.uuid4().hex


def client(key=KEY, **options):
    """SDK client with retries disabled by default; retry tests use default_client()."""
    options.setdefault("max_retries", 0)
    return OpenAI(base_url=BASE, api_key=key, timeout=60, **options)


def default_client(key=KEY):
    return OpenAI(base_url=BASE, api_key=key, timeout=60)


def get_json(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as reply:
        return json.load(reply)


def script(**plan):
    request = urllib.request.Request(CONTROL + "/script", data=json.dumps(plan).encode(), method="POST", headers={"Content-Type": "application/json"})
    urllib.request.urlopen(request, timeout=10).read()


def stats():
    return get_json(CONTROL + "/stats")


def settle(seconds=20):
    deadline = time.monotonic() + seconds
    while stats()["active"] > 0:
        assert time.monotonic() < deadline, "active runs did not settle"
        time.sleep(0.1)


class Delta:
    """Measures server-side counter deltas (runs, model_calls, tool_runs, tool_reads) across a with-block."""

    def __enter__(self):
        self.before = stats()
        return self

    def __exit__(self, *exc):
        self.after = stats()

    def __getattr__(self, name):
        if name in ("before", "after"):
            raise AttributeError(f"Delta.{name}: read after exiting the with-block")
        return self.after[name] - self.before[name]


def raw(method, path, key=KEY, body=None, headers=None, extra_auth=(), content_type="application/json", read_timeout=60):
    """Sends a raw HTTP request bypassing the SDK (e.g. malformed bodies or duplicate auth headers)."""
    url = urllib.parse.urlsplit(BASE)
    conn = http.client.HTTPConnection(url.hostname, url.port, timeout=read_timeout)
    payload = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
    conn.putrequest(method, url.path + path)
    if key is not None:
        conn.putheader("Authorization", f"Bearer {key}")
    for value in extra_auth:
        conn.putheader("Authorization", value)
    if content_type:
        conn.putheader("Content-Type", content_type)
    for name, value in (headers or {}).items():
        conn.putheader(name, value)
    conn.putheader("Content-Length", str(len(payload or b"")))
    conn.endheaders(payload)
    reply = conn.getresponse()
    data = reply.read()
    out = (reply.status, {k.lower(): v for k, v in reply.getheaders()}, data)
    conn.close()
    return out


def chat_body(**extra):
    return {"model": MODEL, "messages": USER, **extra}


def wire_log():
    return get_json(WIRE + "/log")


def chat_attempts():
    return [e for e in wire_log() if e["path"].split("?")[0] == "/v1/chat/completions"]


def summary():
    return get_json(WIRE + "/summary")


def reset_wire():
    urllib.request.urlopen(urllib.request.Request(WIRE + "/reset", data=b"", method="POST"), timeout=10).read()


class Wired(unittest.TestCase):
    """Resets wire logs per test, drains unconsumed scripts on teardown, and asserts zero new schema/SSE violations."""

    def setUp(self):
        settle()
        reset_wire()
        self.violations_before = len(summary()["violations"])

    def tearDown(self):
        leftover = stats()["queued"]
        for _ in range(leftover):  # Drain leftover scripts so subsequent tests stay isolated.
            client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": fresh_key()})
        settle()
        fresh = summary()["violations"][self.violations_before:]
        self.assertEqual(fresh, [], "OpenAPI schema/SSE framing violations")
        self.assertEqual(leftover, 0, "queued model script was left unconsumed")


_ANY = object()


def assert_error(test, body, status, kind, code, param=_ANY, contains=None):
    """Validates an error body against ErrorResponse, fixed type/code, and optional param path (None enforces non-echoing)."""
    SCHEMA.check("ErrorResponse", body)
    error = body["error"]
    test.assertEqual(sorted(error), ["code", "message", "param", "type"])
    test.assertEqual((error["type"], error["code"]), (kind, code), body)
    if param is None:
        test.assertIsNone(error["param"], "unknown field names must not be echoed in param")
    elif param is not _ANY:
        test.assertTrue(isinstance(error["param"], str) and (error["param"] == param or error["param"].startswith((param + ".", param + "["))), (error["param"], param))
    if contains is not None:
        test.assertIn(contains, str(error["param"]))


__all__ = [n for n in dir() if not n.startswith("_")]
