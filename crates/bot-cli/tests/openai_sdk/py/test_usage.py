"""한 실행 안에 모델 차례가 둘(도구 한 번 + 최종 답)일 때의 사용량: 두 차례가 **모두** 보고돼야만 합계(JSON `usage`·SSE 통계 청크)를 낸다. 하나라도 빠지면 JSON은 `usage` 키가 없고 SSE는 통계 청크가 없다 — 0이나 일부 합으로 채우지 않는다."""
import json
import unittest

from common import *
from test_stream import events, parsed

COMBOS = {  # 이름 → (첫 차례 계수, 마지막 차례 계수, 기대 합계 또는 None)
    "both reported": (None, [9, 4], {"prompt_tokens": 14, "completion_tokens": 8, "total_tokens": 22}),  # 첫 차례 기본 5/4
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
                self.assertEqual((d.runs, d.tool_reads, d.tool_runs), (1, 1, 0), "조회 도구는 한 번 돌고 쓰기 도구는 돌지 않는다")
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
                    again = client().chat.completions.with_raw_response.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})  # 같은 키·JSON으로 다시 조회
                self.assertEqual((status, d.runs), (200, 1), "재조회는 새 실행이 아니다")
                data, _ = events(raw_body)
                chunks = parsed(data)
                stats_chunks = [c for c in chunks if not c["choices"]]
                self.assertTrue(all(c["usage"] is None for c in chunks if c["choices"]), "일반 청크는 usage:null")
                self.assertEqual(data[-1], "[DONE]")
                again_data = again.http_response.json()
                if expected is None:
                    self.assertEqual(stats_chunks, [], "통계 청크 없음")
                    self.assertNotIn("usage", again_data)
                else:
                    self.assertEqual([c["usage"] for c in stats_chunks], [expected], "통계 청크 하나")
                    self.assertEqual(again_data["usage"], expected, "재조회에서도 측정값은 같다")


if __name__ == "__main__":
    unittest.main(verbosity=2)
