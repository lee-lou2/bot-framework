"""Error payload and SDK exception tests: 401/403/404/405/415/422, key non-echoing, and no SDK retries on post-admission failure."""
import json
import unittest

import openai

from common import *

ENDPOINTS = (("GET", "/models", None), ("GET", f"/models/{MODEL}", None), ("POST", "/chat/completions", chat_body()))


class Errors(Wired):
    def test_401_for_a_missing_blank_wrong_non_bearer_or_doubled_key_on_every_endpoint(self):
        variants = {
            "no header": dict(key=None), "blank bearer": dict(key=""), "wrong key": dict(key=BAD), "basic scheme": dict(key=None, extra_auth=[f"Basic {KEY}"]),
            "two bearer headers": dict(key=KEY, extra_auth=[f"Bearer {KEY}"]),
        }
        secrets = [KEY, ROTATED, GUEST, BAD]
        with Delta() as d:
            for method, path, body in ENDPOINTS:
                for label, how in variants.items():
                    with self.subTest(path=path, variant=label):
                        status, _, reply = raw(method, path, body=body, **how)
                        self.assertEqual(status, 401)
                        assert_error(self, json.loads(reply), 401, "authentication_error", "invalid_api_key")
                        for secret in secrets:
                            self.assertNotIn(secret.encode(), reply, "error body must not echo API keys")
        self.assertEqual((d.runs, d.model_calls), (0, 0))
        with self.assertRaises(openai.AuthenticationError) as caught:
            client(BAD).models.list()
        self.assertEqual(caught.exception.status_code, 401)

    def test_404_for_an_unknown_model_and_405_with_allow_for_a_wrong_method(self):
        with self.assertRaises(openai.NotFoundError) as caught:
            client().chat.completions.create(model="no-such-model-x", messages=USER)
        assert_error(self, {"error": caught.exception.body}, 404, "invalid_request_error", "model_not_found")
        status, _, reply = raw("GET", "/no-such-endpoint")
        self.assertEqual(status, 404)
        assert_error(self, json.loads(reply), 404, "invalid_request_error", "not_found")
        for method, path, allow in (("DELETE", "/models", "GET"), ("POST", "/models", "GET"), ("GET", "/chat/completions", "POST"), ("PUT", "/chat/completions", "POST")):
            with self.subTest(method=method, path=path):
                status, headers, reply = raw(method, path, body=b"" if method != "GET" else None)
                self.assertEqual(status, 405)
                self.assertIn(allow, headers["allow"])
                assert_error(self, json.loads(reply), 405, "invalid_request_error", "method_not_allowed")

    def test_415_for_a_body_that_is_not_json_content_type_and_nothing_runs(self):
        with Delta() as d:
            for content_type in ("text/plain", "application/x-www-form-urlencoded", None):
                with self.subTest(content_type=content_type):
                    status, _, reply = raw("POST", "/chat/completions", body=chat_body(), content_type=content_type)
                    self.assertEqual(status, 415)
                    assert_error(self, json.loads(reply), 415, "invalid_request_error", "unsupported_media_type")
        self.assertEqual((d.runs, d.model_calls), (0, 0))

    def test_a_run_that_failed_after_acceptance_is_422_that_the_default_sdk_never_retries(self):
        script(fail=True)
        with Delta() as d:
            with self.assertRaises(openai.UnprocessableEntityError) as caught:
                default_client().chat.completions.create(model=MODEL, messages=USER)
        attempts = chat_attempts()
        self.assertEqual([a["status"] for a in attempts], [422], "422 with x-should-retry=false must not be retried")
        self.assertEqual(attempts[0]["res"]["x-should-retry"], "false")
        assert_error(self, {"error": caught.exception.body}, 422, "server_error", "run_failed")
        self.assertEqual(d.runs, 1, "post-admission failure must not spawn retries")
        self.assertTrue(all(a["status"] not in RETRYABLE and a["status"] < 500 for a in attempts))


if __name__ == "__main__":
    unittest.main(verbosity=2)
