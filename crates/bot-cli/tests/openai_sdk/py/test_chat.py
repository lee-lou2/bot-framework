"""Official Python SDK non-streaming chat completion tests: schema fields, usage, finish_reason, idempotency, and request validation."""
import json
import unittest

import openai

from common import *

TOOL = {"type": "function", "function": {"name": "f", "parameters": {"type": "object", "properties": {}}}}
NULLABLE_FIELDS = sorted(SCHEMA.request_nullable - {"stream", "stream_options", "metadata", "audio", "modalities", "prediction", "reasoning_effort", "service_tier"})
# Unsupported fields that are non-nullable in the official schema must be rejected even when null.
NOT_NULLABLE_UNSUPPORTED = ("tools", "tool_choice", "functions", "function_call")
REFUSED = {
    "max_tokens": ({"max_tokens": 64}, "max_tokens"), "max_completion_tokens": ({"max_completion_tokens": 64}, "max_completion_tokens"), "stop": ({"stop": ["###"]}, "stop"),
    "n": ({"n": 2}, "n"), "tools": ({"tools": [TOOL]}, "tools"), "tool_choice": ({"tool_choice": "auto"}, "tool_choice"), "functions": ({"functions": [TOOL["function"]]}, "functions"),
    "function_call": ({"function_call": "auto"}, "function_call"), "parallel_tool_calls": ({"parallel_tool_calls": True}, "parallel_tool_calls"), "logprobs": ({"logprobs": True}, "logprobs"),
    "top_logprobs": ({"top_logprobs": 2}, "top_logprobs"), "audio": ({"audio": {"voice": "alloy", "format": "mp3"}}, "audio"), "modalities": ({"modalities": ["text"]}, "modalities"),
    "store": ({"store": True}, "store"), "response_format": ({"response_format": {"type": "json_object"}}, "response_format"), "unknown_key": ({"surprise": 1}, None),
    "unknown_key_null": ({"surprise": None}, None), "reasoning_effort": ({"reasoning_effort": "low"}, "reasoning_effort"),
}


class Answer(Wired):
    def test_the_answer_has_every_official_field_and_measured_usage(self):
        script(text=ANSWER, usage=[9, 4])
        with Delta() as d:
            reply = client().chat.completions.with_raw_response.create(model=MODEL, messages=USER)
        data = reply.http_response.json()
        SCHEMA.check("CreateChatCompletionResponse", data)
        self.assertEqual(reply.http_response.status_code, 200)
        self.assertTrue(data["id"].startswith("chatcmpl-") and data["object"] == "chat.completion" and data["model"] == MODEL)
        self.assertEqual(data["choices"], [{"index": 0, "message": {"role": "assistant", "content": ANSWER, "refusal": None}, "finish_reason": "stop", "logprobs": None}])
        self.assertEqual(data["usage"], {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13})
        generated = reply.headers["idempotency-key"]
        self.assertGreaterEqual(len(generated), 16, "server must synthesize an Idempotency-Key when omitted")
        self.assertTrue(reply.headers["x-request-id"])
        self.assertEqual((d.runs, d.model_calls), (1, 1))
        parsed = reply.parse()
        self.assertEqual((parsed.choices[0].message.content, parsed.usage.total_tokens), (ANSWER, 13))

    def test_usage_that_was_not_measured_is_left_out_never_zero_filled(self):
        script(text=ANSWER, usage=None)
        reply = client().chat.completions.with_raw_response.create(model=MODEL, messages=USER)
        data = reply.http_response.json()
        SCHEMA.check("CreateChatCompletionResponse", data)
        self.assertNotIn("usage", data, "unmeasured usage must be omitted")
        self.assertIsNone(reply.parse().usage)

    def test_a_long_normal_answer_under_the_host_cap_is_stop_in_full(self):
        long_text = ("long reply " * 600).rstrip()
        script(text=long_text, stop="end_turn")
        whole = client().chat.completions.create(model=MODEL, messages=USER)
        self.assertEqual((whole.choices[0].finish_reason, whole.choices[0].message.content), ("stop", long_text))

    def test_an_answer_over_the_host_cap_is_clipped_and_says_length(self):
        script(text=("★" * (HOST_CAP + 500)), stop="end_turn")
        clipped = client().chat.completions.create(model=MODEL, messages=USER)
        self.assertEqual(clipped.choices[0].finish_reason, "length")
        self.assertEqual(clipped.choices[0].message.content, "★" * HOST_CAP, "clipped replies must report finish_reason=length")

    def test_a_provider_limit_stop_that_leaves_no_final_answer_is_422_run_failed(self):
        script(text="truncated reply", stop="max_tokens")
        with Delta() as d:
            status, headers, body = raw("POST", "/chat/completions", body=chat_body())
        self.assertEqual(status, 422, body[:200])
        assert_error(self, json.loads(body), 422, "server_error", "run_failed")
        self.assertEqual(headers["x-should-retry"], "false")
        self.assertEqual(d.runs, 1)

    def test_an_earlier_turn_that_hit_the_limit_does_not_make_a_normal_final_answer_length(self):
        script(stop="max_tokens", then={"text": ANSWER})
        reply = client().chat.completions.create(model=MODEL, messages=USER)
        self.assertEqual((reply.choices[0].finish_reason, reply.choices[0].message.content), ("stop", ANSWER))

    def test_the_same_key_runs_once_and_replays_the_same_answer(self):
        script(text=ANSWER)
        key = fresh_key()
        send = lambda c=None: (c or client()).chat.completions.with_raw_response.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        with Delta() as d:
            first, second = send(), send()
            third = send(client(ROTATED))
        self.assertEqual((d.runs, d.model_calls), (1, 1), "same key must execute only once")
        bodies = [r.http_response.json() for r in (first, second, third)]
        self.assertEqual(bodies[0], bodies[1])
        self.assertEqual(bodies[0], bodies[2])
        self.assertTrue(all(r.headers["idempotency-key"] == key for r in (first, second, third)))

    def test_a_keyless_request_marked_as_an_sdk_resend_is_refused_and_runs_nothing(self):
        with Delta() as d, self.assertRaises(openai.BadRequestError) as caught:
            client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"x-stainless-retry-count": "1"})
        assert_error(self, {"error": caught.exception.body}, 400, "invalid_request_error", "idempotency_key_required")
        self.assertEqual((d.runs, d.model_calls), (0, 0))
        script(text=ANSWER)
        self.assertEqual(client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"x-stainless-retry-count": "0"}).choices[0].message.content, ANSWER)

    def test_the_same_key_with_another_request_is_a_conflict_and_runs_nothing(self):
        script(text=ANSWER)
        key = fresh_key()
        client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        with Delta() as d:
            with self.assertRaises(openai.BadRequestError) as caught:
                client().chat.completions.create(model=MODEL, messages=[{"role": "user", "content": "another question"}], extra_headers={"Idempotency-Key": key})
        assert_error(self, {"error": caught.exception.body}, 400, "invalid_request_error", "idempotency_key_conflict")
        self.assertEqual((d.runs, d.model_calls), (0, 0))

    def test_another_subject_using_the_same_key_and_body_gets_its_own_run_and_never_the_first_answer(self):
        script(text="first subject reply")
        script(text="second subject reply")
        key = fresh_key()
        send = lambda api_key: client(api_key).chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        with Delta() as d:
            mine, theirs = send(KEY), send(OTHER)
        self.assertEqual((mine.choices[0].message.content, theirs.choices[0].message.content), ("first subject reply", "second subject reply"))
        self.assertNotEqual(mine.id, theirs.id)
        self.assertEqual((d.runs, d.model_calls), (2, 2), "idempotency keys are scoped per subject")

    def test_the_same_key_may_change_only_presentation_options(self):
        script(text=ANSWER)
        key = fresh_key()
        first = client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        with Delta() as d:
            again = client().chat.completions.create(model=MODEL, messages=USER, user="someone", metadata={"k": "v"}, extra_headers={"Idempotency-Key": key})
        self.assertEqual((again.id, d.runs), (first.id, 0), "user and metadata are excluded from the semantic fingerprint")

    def test_accepted_shapes_and_ignored_options(self):
        script(text=ANSWER)
        messages = [{"role": "system", "content": "s"}, {"role": "developer", "content": "d"}, {"role": "user", "name": "kim", "content": [{"type": "text", "text": "t"}]},
                    {"role": "assistant", "content": "a"}, {"role": "user", "content": "final question"}]
        reply = client().chat.completions.create(model=MODEL, messages=messages, temperature=0.2, top_p=0.9, seed=7, user="e2e", metadata={"k": "v"}, store=False, n=1,
                                                 response_format={"type": "text"}, extra_body={name: None for name in NULLABLE_FIELDS if name not in ("temperature", "top_p", "seed", "n", "store")})
        self.assertEqual(reply.choices[0].message.content, ANSWER)

    def test_null_is_not_sent_only_for_fields_the_official_schema_lets_be_null(self):
        script(text=ANSWER)
        reply = client().chat.completions.create(model=MODEL, messages=USER, extra_body={name: None for name in NULLABLE_FIELDS})
        self.assertEqual(reply.choices[0].message.content, ANSWER)
        for name in NOT_NULLABLE_UNSUPPORTED + ("surprise",):
            with self.subTest(name), Delta() as d, self.assertRaises(openai.BadRequestError) as caught:
                client().chat.completions.create(model=MODEL, messages=USER, extra_body={name: None})
            assert_error(self, {"error": caught.exception.body}, 400, "invalid_request_error", "unsupported_parameter", param=None if name == "surprise" else name)
            self.assertEqual((d.runs, d.model_calls), (0, 0))

    def test_unsupported_fields_are_refused_by_name_and_nothing_runs(self):
        for label, (extra, param) in REFUSED.items():
            with self.subTest(label):
                with Delta() as d, self.assertRaises(openai.BadRequestError) as caught:
                    client().chat.completions.create(model=MODEL, messages=USER, extra_body=extra)
                assert_error(self, {"error": caught.exception.body}, 400, "invalid_request_error", "unsupported_parameter", param=param)
                self.assertEqual((d.runs, d.model_calls), (0, 0))

    def test_unsupported_roles_and_parts_are_refused(self):
        cases = {
            "tool role": [{"role": "user", "content": "q"}, {"role": "tool", "content": "r", "tool_call_id": "c"}],
            "assistant tool_calls": [{"role": "user", "content": "q"}, {"role": "assistant", "content": None, "tool_calls": [{"id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}}]}],
            "image part": [{"role": "user", "content": [{"type": "text", "text": "t"}, {"type": "image_url", "image_url": {"url": "https://example.test/x.png"}}]}],
            "audio part": [{"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}]}],
            "unknown message field": [{"role": "user", "content": "q", "surprise": 1}],
            "function role": [{"role": "user", "content": "q"}, {"role": "function", "name": "f", "content": "r"}],
        }
        for label, messages in cases.items():
            with self.subTest(label):
                with Delta() as d, self.assertRaises(openai.BadRequestError) as caught:
                    client().chat.completions.create(model=MODEL, messages=messages)
                assert_error(self, {"error": caught.exception.body}, 400, "invalid_request_error", "unsupported_parameter", **({"param": "messages[0]"} if label == "unknown message field" else {}))
                self.assertNotIn("surprise", json.dumps(caught.exception.body), "unknown field names must not be echoed")
                self.assertEqual((d.runs, d.model_calls), (0, 0))

    def test_malformed_requests_are_invalid_request(self):
        cases = {
            "unknown role": {"model": MODEL, "messages": [{"role": "banana", "content": "q"}, {"role": "user", "content": "q"}]},
            "empty messages": {"model": MODEL, "messages": []}, "no user message": {"model": MODEL, "messages": [{"role": "assistant", "content": "a"}]},
            "missing model": {"messages": USER}, "missing messages": {"model": MODEL}, "bad temperature": {"model": MODEL, "messages": USER, "temperature": "hot"},
            "stream_options without stream": {"model": MODEL, "messages": USER, "stream_options": {"include_usage": True}}, "non-text content": {"model": MODEL, "messages": [{"role": "user", "content": 7}]},
        }
        for label, body in cases.items():
            with self.subTest(label):
                with Delta() as d:
                    status, _, reply = raw("POST", "/chat/completions", body=body)
                self.assertEqual(status, 400)
                assert_error(self, json.loads(reply), 400, "invalid_request_error", "invalid_request")
                self.assertEqual((d.runs, d.model_calls), (0, 0))
        for label, body in {"not json": b"{not json", "array": b"[]", "null": b"null", "duplicate key": b'{"model":"a","model":"b","messages":[{"role":"user","content":"q"}]}'}.items():
            with self.subTest(label):
                status, _, reply = raw("POST", "/chat/completions", body=body)
                self.assertEqual(status, 400, label)
                assert_error(self, json.loads(reply), 400, "invalid_request_error", "invalid_request")


if __name__ == "__main__":
    unittest.main(verbosity=2)
