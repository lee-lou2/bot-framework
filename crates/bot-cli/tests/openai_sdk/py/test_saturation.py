"""진행 중인 작업이 입구의 한도(`max_active_runs`, 이 시험은 1)를 채웠을 때: 429+Retry-After, SDK 기본 재시도의 **실제 wire 시도 수**와 run 0, 키 없는 재전송은 400, 이미 접수한 키는 포화 중에도 같은 작업으로 이어진다.
시도 수는 입구를 믿지 않고 프록시가 센다(0·1·2 회차 표지 `x-stainless-retry-count`)."""
import unittest

import openai

from common import *

HOLD_MS = WAIT_MS + 8000  # 자리를 차지한 작업이 시험 본문 동안 계속 돈다


class Saturation(Wired):
    def setUp(self):
        settle()
        super().setUp()
        script(text=ANSWER, delay_ms=HOLD_MS)
        self.holder = fresh_key()
        first = client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": self.holder})
        self.assertTrue(first.choices[0].message.content.startswith(NOTICE_PREFIX), "자리를 차지한 작업이 아직 돈다")
        self.assertEqual(stats()["active"], 1)
        reset_wire()

    def tearDown(self):
        settle()
        super().tearDown()

    def test_an_explicit_key_429_is_retried_by_the_default_sdk_twice_and_creates_no_run(self):
        key = fresh_key()
        with Delta() as d:
            with self.assertRaises(openai.RateLimitError) as caught:
                default_client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        attempts = chat_attempts()
        self.assertEqual([a["status"] for a in attempts], [429, 429, 429])
        self.assertEqual([a["req"].get("x-stainless-retry-count") for a in attempts], ["0", "1", "2"])
        self.assertEqual({a["req"].get("idempotency-key") for a in attempts}, {key})
        self.assertEqual({a["res"].get("retry-after") for a in attempts}, {"1"}, "실행 수 포화는 다음 확인 간격 1초")
        assert_error(self, {"error": caught.exception.body}, 429, "rate_limit_error", "rate_limit_exceeded")
        self.assertEqual((d.runs, d.model_calls), (0, 0))

    def test_a_keyless_429_then_the_sdk_resend_is_400_idempotency_key_required_and_no_run(self):
        with Delta() as d:
            with self.assertRaises(openai.BadRequestError) as caught:
                default_client().chat.completions.create(model=MODEL, messages=USER)
        attempts = chat_attempts()
        self.assertEqual([a["status"] for a in attempts], [429, 400])
        self.assertEqual([a["req"].get("x-stainless-retry-count") for a in attempts], ["0", "1"])
        self.assertTrue(all("idempotency-key" not in a["req"] for a in attempts))
        assert_error(self, {"error": caught.exception.body}, 400, "invalid_request_error", "idempotency_key_required")
        self.assertEqual((d.runs, d.model_calls), (0, 0), "새 작업이 만들어지지 않는다")

    def test_with_retries_off_the_statuses_are_checked_directly(self):
        with Delta() as d:
            with self.assertRaises(openai.RateLimitError) as caught:
                client().chat.completions.create(model=MODEL, messages=USER)
            self.assertEqual(caught.exception.response.headers["retry-after"], "1")
            with self.assertRaises(openai.BadRequestError) as marker:
                client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"x-stainless-retry-count": "1"})
            assert_error(self, {"error": marker.exception.body}, 400, "invalid_request_error", "idempotency_key_required")
            with self.assertRaises(openai.RateLimitError):
                client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": fresh_key(), "x-stainless-retry-count": "2"})
        self.assertEqual((d.runs, d.model_calls), (0, 0), "표지는 실행 권한의 근거가 아니다: 키가 있으면 재시도 표지가 있어도 새 접수로 다룬다")

    def test_a_key_that_was_already_accepted_is_still_served_while_saturated(self):
        with Delta() as d:
            reply = default_client().chat.completions.with_raw_response.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": self.holder})
        self.assertEqual([a["status"] for a in chat_attempts()], [200], "429가 아니라 같은 작업을 조회한다")
        self.assertTrue(reply.parse().choices[0].message.content.startswith(NOTICE_PREFIX))
        self.assertEqual((d.runs, d.model_calls), (0, 0), "기존 작업 재조회는 새 슬롯을 쓰지 않는다")

    def test_once_the_slot_is_free_the_same_explicit_key_runs_exactly_once(self):
        key = fresh_key()
        with self.assertRaises(openai.RateLimitError):
            client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        settle()
        script(text=ANSWER)
        with Delta() as d:
            reply = default_client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        self.assertEqual((reply.choices[0].message.content, d.runs, d.model_calls), (ANSWER, 1, 1))


if __name__ == "__main__":
    unittest.main(verbosity=2)
