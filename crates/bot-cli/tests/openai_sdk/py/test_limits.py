"""본문 한도(이 시험의 입구는 `gateway.limits.body_bytes` 4096): 한도를 넘으면 413 `request_too_large`(선언 길이·실제 누적 모두), 접수도 실행도 없고 SDK가 다시 보내지 않는다."""
import http.client
import json
import unittest
import urllib.parse

import openai

from common import *

LIMIT = 4096


def body_of(size):
    padding = size - len(json.dumps(chat_body(messages=[{"role": "user", "content": ""}])))
    return chat_body(messages=[{"role": "user", "content": "가" * 0 + "x" * padding}])


class Limits(Wired):
    def test_a_body_just_under_the_limit_runs(self):
        script(text=ANSWER)
        status, _, reply = raw("POST", "/chat/completions", body=body_of(LIMIT - 64))
        self.assertEqual(status, 200, reply[:200])

    def test_a_body_over_the_limit_is_413_and_the_default_sdk_does_not_resend_it(self):
        with Delta() as d:
            with self.assertRaises(openai.APIStatusError) as caught:
                default_client().chat.completions.create(model=MODEL, messages=[{"role": "user", "content": "x" * (LIMIT + 1)}])
        self.assertEqual(caught.exception.status_code, 413)
        assert_error(self, {"error": caught.exception.body}, 413, "invalid_request_error", "request_too_large")
        self.assertEqual([a["status"] for a in chat_attempts()], [413])
        self.assertEqual((d.runs, d.model_calls), (0, 0))

    def test_a_chunked_body_is_limited_by_the_bytes_actually_received(self):
        url = urllib.parse.urlsplit(BASE)
        payload = json.dumps(body_of(LIMIT + 2048)).encode()
        conn = http.client.HTTPConnection(url.hostname, url.port, timeout=30)
        with Delta() as d:
            conn.request("POST", url.path + "/chat/completions", body=iter([payload[i:i + 512] for i in range(0, len(payload), 512)]),
                         headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}, encode_chunked=True)
            reply = conn.getresponse()
            data = reply.read()
        self.assertEqual(reply.status, 413)
        assert_error(self, json.loads(data), 413, "invalid_request_error", "request_too_large")
        self.assertEqual((d.runs, d.model_calls), (0, 0))


if __name__ == "__main__":
    unittest.main(verbosity=2)
