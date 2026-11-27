"""Saturation tests (max_active_runs=1): 429+Retry-After, exact wire retry counts, keyless resend 400, and existing-key replay."""
import unittest

import openai

from common import *

HOLD_MS = WAIT_MS + 8000


class Saturation(Wired):
    def setUp(self):
        settle()
        super().setUp()
        script(text=ANSWER, delay_ms=HOLD_MS)
        self.holder = fresh_key()
        first = client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": self.holder})
        self.assertTrue(first.choices[0].message.content.startswith(NOTICE_PREFIX), "holder run must still be active")
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
        self.assertEqual({a["res"].get("retry-after") for a in attempts}, {"1"}, "saturation must return Retry-After: 1")
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
        self.assertEqual((d.runs, d.model_calls), (0, 0), "rejected retry must not create a run")

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
        self.assertEqual((d.runs, d.model_calls), (0, 0), "retry header does not bypass capacity checks")

    def test_a_key_that_was_already_accepted_is_still_served_while_saturated(self):
        with Delta() as d:
            reply = default_client().chat.completions.with_raw_response.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": self.holder})
        self.assertEqual([a["status"] for a in chat_attempts()], [200], "existing key must query the run instead of 429")
        self.assertTrue(reply.parse().choices[0].message.content.startswith(NOTICE_PREFIX))
        self.assertEqual((d.runs, d.model_calls), (0, 0), "existing key replay must not consume a new slot")

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
