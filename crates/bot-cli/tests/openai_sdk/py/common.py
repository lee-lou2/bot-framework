"""공통 도구: 환경, SDK 클라이언트, 원시 요청, 프록시 기록, 모델 제어·관찰 창구. 환경이 빠지면 KeyError로 실패한다(건너뛰지 않는다).

환경: OPENAI_BASE_URL(프록시의 …/v1) · OPENAI_WIRE_URL(프록시의 …/__wire) · OPENAI_E2E_CONTROL(제어·관찰 창구) · OPENAI_API_KEY · OPENAI_ROTATED_KEY(같은 주체의 두 번째 슬롯)
      · OPENAI_GUEST_KEY(유효하지만 멤버가 아닌 키) · OPENAI_OTHER_KEY(같은 모델을 쓰는 다른 멤버 주체의 키) · OPENAI_WAIT_MS(서버의 wait_ms) · 선택: OPENAI_BAD_KEY · OPENAI_MODEL(assistant) · OPENAI_HIDDEN_MODEL(있지만 이 키의 역할 밖)"""
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
NOTICE_PREFIX = "작업을 처리 중입니다."
NOTICE_RETRY = "같은 Idempotency-Key와 같은 요청으로 다시 조회하세요."
RETRYABLE = {408, 409, 429}
HOST_CAP = 12000  # 최종 글의 호스트 상한(글자 수): 넘으면 잘리고 finish_reason은 length


def fresh_key():
    return "e2e-" + uuid.uuid4().hex


def client(key=KEY, **options):
    """SDK 클라이언트. 기본은 재시도를 끈다(`max_retries=0`) — 재시도를 보는 시험은 `default_client()`를 쓴다."""
    options.setdefault("max_retries", 0)
    return OpenAI(base_url=BASE, api_key=key, timeout=60, **options)


def default_client(key=KEY):
    return OpenAI(base_url=BASE, api_key=key, timeout=60)  # SDK 기본: max_retries=2


def get_json(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as reply:
        return json.load(reply)


def script(**plan):
    """모델이 다음 요청에 할 일(text·usage=[입력,출력]|None·stop=end_turn|max_tokens·delay_ms·fail)을 정한다."""
    request = urllib.request.Request(CONTROL + "/script", data=json.dumps(plan).encode(), method="POST", headers={"Content-Type": "application/json"})
    urllib.request.urlopen(request, timeout=10).read()


def stats():
    return get_json(CONTROL + "/stats")


def settle(seconds=20):
    """진행 중인 작업이 모두 끝날 때까지 기다린다."""
    deadline = time.monotonic() + seconds
    while stats()["active"] > 0:
        assert time.monotonic() < deadline, "작업이 끝나지 않았다"
        time.sleep(0.1)


class Delta:
    """블록 안에서 늘어난 서버 쪽 수(작업 `runs`·모델 호출 `model_calls`·도구 실행 `tool_runs`)."""

    def __enter__(self):
        self.before = stats()
        return self

    def __exit__(self, *exc):
        self.after = stats()

    def __getattr__(self, name):
        if name in ("before", "after"):
            raise AttributeError(f"Delta.{name}: `with` 블록이 끝난 뒤에 읽는다")
        return self.after[name] - self.before[name]


def raw(method, path, key=KEY, body=None, headers=None, extra_auth=(), content_type="application/json", read_timeout=60):
    """SDK를 거치지 않은 원시 요청(없는 키·겹친 헤더·틀린 본문을 보려고). (상태, 헤더(소문자), 본문 바이트)"""
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
    """시험마다 프록시 기록을 비우고, 끝나면 그 사이 공식 스키마·SSE 틀 위반이 새로 생기지 않았는지 본다. 모델 큐가 남지 않았는지도 본다."""

    def setUp(self):
        settle()  # 앞 시험이 남긴 진행 중 작업이 이 시험의 한도·수를 흔들지 않게 한다
        reset_wire()
        self.violations_before = len(summary()["violations"])

    def tearDown(self):
        leftover = stats()["queued"]
        for _ in range(leftover):  # 소비되지 않은 모델 응답이 다음 시험으로 새지 않게 비운다(원인 시험을 아래에서 실패로 보고한다)
            client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": fresh_key()})
        settle()
        fresh = summary()["violations"][self.violations_before:]
        self.assertEqual(fresh, [], "공식 OpenAPI 스키마·SSE 틀 위반")
        self.assertEqual(leftover, 0, "시험이 쌓은 모델 응답이 소비되지 않고 남았다(요청이 모델까지 가지 않았다)")


_ANY = object()


def assert_error(test, body, status, kind, code, param=_ANY, contains=None):
    """오류 본문: 네 필드(null 포함)와 공식 스키마, 합의된 type·code. `param`을 생략하면 보지 않고, `None`이면 null이어야 하며(최상위 미등록 키: 지어낸 이름은 반사하지 않는다),
    문자열이면 그 필드이거나 그 아래 경로(`response_format.type`)다."""
    SCHEMA.check("ErrorResponse", body)
    error = body["error"]
    test.assertEqual(sorted(error), ["code", "message", "param", "type"])
    test.assertEqual((error["type"], error["code"]), (kind, code), body)
    if param is None:
        test.assertIsNone(error["param"], "지어낸 필드 이름은 오류에 반사하지 않는다")
    elif param is not _ANY:
        test.assertTrue(isinstance(error["param"], str) and (error["param"] == param or error["param"].startswith((param + ".", param + "["))), (error["param"], param))
    if contains is not None:
        test.assertIn(contains, str(error["param"]))


__all__ = [n for n in dir() if not n.startswith("_")]
