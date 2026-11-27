"""Official Python SDK streaming chat completion tests: chunk order, UTF-8 integrity, include_usage rules, clipping, and mid-stream errors."""
import json
import unittest

import openai

from common import *

TEXT = ("Hello 😀 streaming e2e — symbols ★€ and emoji must stay intact across chunks. " * 3).rstrip()


def events(body):
    assert body.endswith(b"\n\n"), "final SSE event must end with a blank line"
    data, comments = [], 0
    for event in body.decode("utf-8").split("\n\n")[:-1]:
        if event.startswith(":"):
            comments += 1
        else:
            assert event.startswith("data: ") and "\n" not in event, f"malformed SSE event {event[:40]!r}"
            data.append(event[len("data: "):])
    return data, comments


def parsed(data):
    return [json.loads(d) for d in data if d != "[DONE]"]


class Stream(Wired):
    def raw_stream(self, include_usage=None, **extra):
        options = {} if include_usage is None else {"stream_options": {"include_usage": include_usage}}
        return raw("POST", "/chat/completions", body=chat_body(stream=True, **options, **extra))

    def test_chunks_arrive_in_order_with_one_id_created_model_and_whole_utf8_text(self):
        script(text=TEXT)
        chunks = list(client().chat.completions.create(model=MODEL, messages=USER, stream=True))
        self.assertEqual((chunks[0].choices[0].delta.role, chunks[0].choices[0].delta.content, chunks[0].choices[0].finish_reason), ("assistant", "", None))
        self.assertEqual("".join(c.choices[0].delta.content or "" for c in chunks if c.choices), TEXT)
        self.assertEqual((chunks[-1].choices[0].finish_reason, chunks[-1].choices[0].delta.content), ("stop", None))
        self.assertEqual(len({(c.id, c.created, c.model) for c in chunks}), 1)
        self.assertTrue(all(c.object == "chat.completion.chunk" and c.usage is None for c in chunks))
        self.assertEqual(sum(1 for c in chunks if c.choices and c.choices[0].finish_reason), 1, "finish_reason must appear once")

    def test_framing_headers_and_every_event_without_usage_options(self):
        script(text=ANSWER)
        status, headers, body = self.raw_stream()
        self.assertEqual(status, 200)
        self.assertTrue(headers["content-type"].lower().replace(" ", "") == "text/event-stream;charset=utf-8", headers["content-type"])
        self.assertEqual((headers["cache-control"], headers["x-accel-buffering"]), ("no-cache", "no"))
        self.assertTrue(headers["idempotency-key"] and headers["x-request-id"])
        data, _ = events(body)
        self.assertEqual(data[-1], "[DONE]")
        self.assertTrue(all("usage" not in c for c in parsed(data)), "usage key must be omitted without include_usage")

    def test_include_usage_false_omits_the_key_in_every_chunk(self):
        script(text=ANSWER)
        data, _ = events(self.raw_stream(include_usage=False)[2])
        self.assertTrue(all("usage" not in c for c in parsed(data)))

    def test_include_usage_true_with_a_measured_count_adds_null_on_chunks_and_one_final_usage_chunk(self):
        script(text=ANSWER, usage=[9, 4])
        data, _ = events(self.raw_stream(include_usage=True)[2])
        chunks = parsed(data)
        self.assertEqual(data[-1], "[DONE]")
        self.assertEqual(chunks[-1]["choices"], [])
        self.assertEqual(chunks[-1]["usage"], {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13})
        self.assertTrue(all("usage" in c and c["usage"] is None for c in chunks[:-1]), "content chunks must carry usage: null")
        self.assertEqual(chunks[-2]["choices"][0]["finish_reason"], "stop", "usage chunk must follow finish chunk")
        script(text=ANSWER, usage=[9, 4])
        sdk = list(client().chat.completions.create(model=MODEL, messages=USER, stream=True, stream_options={"include_usage": True}))
        self.assertEqual((sdk[-1].choices, sdk[-1].usage.total_tokens), ([], 13))

    def test_include_usage_true_with_an_unmeasured_count_has_no_usage_chunk(self):
        script(text=ANSWER, usage=None)
        data, _ = events(self.raw_stream(include_usage=True)[2])
        chunks = parsed(data)
        self.assertTrue(all(c["choices"] for c in chunks), "unmeasured usage must not emit an empty-choices chunk")
        self.assertTrue(all("usage" in c and c["usage"] is None for c in chunks))
        self.assertEqual(data[-1], "[DONE]")

    def test_an_answer_over_the_host_cap_streams_length_and_a_long_normal_one_streams_stop(self):
        script(text="★" * (HOST_CAP + 500), stop="end_turn")
        chunks = parsed(events(self.raw_stream()[2])[0])
        self.assertEqual((chunks[-1]["choices"][0]["finish_reason"], chunks[-1]["choices"][0]["delta"]), ("length", {}))
        self.assertEqual("".join(c["choices"][0]["delta"].get("content") or "" for c in chunks), "★" * HOST_CAP)
        long_text = ("long normal reply " * 400).rstrip()
        script(text=long_text, stop="end_turn")
        chunks = parsed(events(self.raw_stream()[2])[0])
        self.assertEqual(chunks[-1]["choices"][0]["finish_reason"], "stop")
        self.assertEqual("".join(c["choices"][0]["delta"].get("content") or "" for c in chunks), long_text)

    def test_a_provider_limit_stop_that_leaves_no_final_answer_ends_the_stream_with_a_run_failed_event(self):
        script(text="truncated reply", stop="max_tokens")
        status, _, body = self.raw_stream()
        self.assertEqual(status, 200)
        data, _ = events(body)
        self.assertNotIn("[DONE]", data)
        assert_error(self, json.loads(data[-1]), 200, "server_error", "run_failed")
        self.assertTrue(all(not c.get("choices") or c["choices"][0]["finish_reason"] is None for c in parsed(data[:-1])), "failed stream must not emit a finish_reason")

    def test_the_stream_of_a_key_equals_the_json_answer_of_the_same_key_and_runs_once(self):
        script(text=TEXT)
        key = fresh_key()
        with Delta() as d:
            whole = client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
            chunks = list(client().chat.completions.create(model=MODEL, messages=USER, stream=True, extra_headers={"Idempotency-Key": key}))
        self.assertEqual((d.runs, d.model_calls), (1, 1))
        self.assertEqual("".join(c.choices[0].delta.content or "" for c in chunks if c.choices), whole.choices[0].message.content)
        self.assertEqual({c.id for c in chunks}, {whole.id})

    def test_a_failure_after_the_stream_began_is_an_error_event_with_no_finish_and_no_done(self):
        script(fail=True, delay_ms=300)
        status, _, body = self.raw_stream()
        self.assertEqual(status, 200)
        data, _ = events(body)
        self.assertNotIn("[DONE]", data)
        last = json.loads(data[-1])
        assert_error(self, last, 200, "server_error", "run_failed")
        self.assertTrue(all(not c["choices"] or c["choices"][0]["finish_reason"] is None for c in parsed(data[:-1])), "failed stream must not emit a finish_reason")
        script(fail=True, delay_ms=300)
        with self.assertRaises(openai.APIError):
            list(default_client().chat.completions.create(model=MODEL, messages=USER, stream=True))
        self.assertEqual(len(chat_attempts()), 2, "SDK must not retry a mid-stream failure")


if __name__ == "__main__":
    unittest.main(verbosity=2)
