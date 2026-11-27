"""Multi-turn usage tests: tokens are summed and reported only when every turn reports usage (never partial or zero-filled)."""
import json
import unittest

from common import *
from test_stream import events, parsed

COMBOS = {
    "both reported": (None, [9, 4], {"prompt_tokens": 14, "completion_tokens": 8, "total_tokens": 22}),  # First turn defaults to 5/4.
    "first missing": ("missing", [9, 4], None),
    "last missing": (None, None, None),
    "both missing": ("missing", None, None),
}


def plan(first, last):
    out = {"text": ANSWER, "tool_call": "list", "usage": last}
    if first == "missing":
        out["first_usage"] = None
    return out


class MixedUsage(Wired):
    def test_json_usage_is_the_sum_only_when_every_turn_was_measured(self):
        for label, (first, last, expected) in COMBOS.items():
            with self.subTest(label):
                script(**plan(first, last))
                with Delta() as d:
                    reply = client().chat.completions.with_raw_response.create(model=MODEL, messages=USER)
                data = reply.http_response.json()
                SCHEMA.check("CreateChatCompletionResponse", data)
                self.assertEqual((d.runs, d.tool_reads, d.tool_runs), (1, 1, 0), "read tool runs once; write tool does not run")
                self.assertEqual(data["choices"][0]["message"]["content"], ANSWER)
                if expected is None:
                    self.assertNotIn("usage", data)
                else:
                    self.assertEqual(data["usage"], expected)

    def test_the_stream_has_a_usage_chunk_only_when_every_turn_was_measured_and_the_same_key_replays_it(self):
        for label, (first, last, expected) in COMBOS.items():
            with self.subTest(label):
                script(**plan(first, last))
                key = fresh_key()
                body = chat_body(stream=True, stream_options={"include_usage": True})
                with Delta() as d:
                    status, _, raw_body = raw("POST", "/chat/completions", body=body, headers={"Idempotency-Key": key})
                    again = client().chat.completions.with_raw_response.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
                self.assertEqual((status, d.runs), (200, 1), "re-querying must not start a new run")
                data, _ = events(raw_body)
                chunks = parsed(data)
                stats_chunks = [c for c in chunks if not c["choices"]]
                self.assertTrue(all(c["usage"] is None for c in chunks if c["choices"]), "content chunks must carry usage: null")
                self.assertEqual(data[-1], "[DONE]")
                again_data = again.http_response.json()
                if expected is None:
                    self.assertEqual(stats_chunks, [], "expected no usage chunk")
                    self.assertNotIn("usage", again_data)
                else:
                    self.assertEqual([c["usage"] for c in stats_chunks], [expected], "expected one usage chunk")
                    self.assertEqual(again_data["usage"], expected, "replayed JSON usage must match")


if __name__ == "__main__":
    unittest.main(verbosity=2)
