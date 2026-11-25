"""적합성 검사기 자체의 시험: 고정 확인, 3.0 `nullable` 변환(형·enum·`$ref` 옆), 공식 스키마가 요구하는 필드. 서버가 필요 없다."""
import copy
import json
import pathlib
import tempfile
import unittest

import conformance
from conformance import Schema, to_json_schema

SCHEMA = Schema()
COMPLETION = {
    "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "assistant",
    "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi", "refusal": None}, "finish_reason": "stop", "logprobs": None}],
}
CHUNK = {"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "assistant", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}]}
ERROR = {"error": {"message": "m", "type": "invalid_request_error", "param": None, "code": "invalid_request"}}


def without(doc, *path):
    out = copy.deepcopy(doc)
    node = out
    for key in path[:-1]:
        node = node[key]
    del node[path[-1]]
    return out


class Pinned(unittest.TestCase):
    def test_the_subset_is_the_pinned_official_schema(self):
        self.assertEqual(SCHEMA.source["commit"], "498c71ddf6f1c45b983f972ccabca795da211a3e")
        self.assertEqual(SCHEMA.source["spec_sha256"], "6a6c681b182002de4ad335fa80214158f2d8f383a418f865c87876c3ed67cad9")
        self.assertEqual(SCHEMA.source["openapi"], "3.0.0")

    def test_an_edited_subset_is_refused_before_anything_is_checked(self):
        with tempfile.TemporaryDirectory() as tmp:
            edited = pathlib.Path(tmp) / "subset.json"
            edited.write_text(conformance.SUBSET.read_text(encoding="utf-8").replace('"required"', '"required_"', 1), encoding="utf-8")
            with self.assertRaises(AssertionError):
                Schema(subset=edited)

    def test_a_pin_that_names_another_commit_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            pin = pathlib.Path(tmp) / "pin.json"
            doc = json.loads(conformance.PIN.read_text(encoding="utf-8"))
            doc["openapi"]["commit"] = "0" * 40
            pin.write_text(json.dumps(doc), encoding="utf-8")
            with self.assertRaises(AssertionError):
                Schema(pin=pin)


class Nullable(unittest.TestCase):
    def accepts(self, node, value):
        return not list(__import__("jsonschema").Draft202012Validator(to_json_schema(node)).iter_errors(value))

    def test_a_nullable_type_takes_null_and_still_checks_the_type(self):
        node = {"type": "string", "nullable": True}
        self.assertTrue(self.accepts(node, None) and self.accepts(node, "x"))
        self.assertFalse(self.accepts(node, 1))
        self.assertFalse(self.accepts({"type": "string"}, None), "nullable이 없으면 null은 거절")

    def test_a_nullable_enum_takes_null_but_no_other_value(self):
        node = {"type": "string", "enum": ["stop", "length"], "nullable": True}
        self.assertTrue(self.accepts(node, None) and self.accepts(node, "stop"))
        self.assertFalse(self.accepts(node, "other"))

    def test_a_nullable_ref_beside_the_ref_is_any_of_the_ref_and_null(self):
        node = {"$ref": "#/defs/Usage", "nullable": True}
        converted = to_json_schema(node)
        self.assertEqual(sorted(converted), ["anyOf"])
        self.assertIn({"type": "null"}, converted["anyOf"])

    def test_boolean_exclusive_bounds_become_numeric(self):
        converted = to_json_schema({"type": "integer", "minimum": 1, "exclusiveMinimum": True})
        self.assertEqual(converted["exclusiveMinimum"], 1)
        self.assertNotIn("minimum", converted)

    def test_no_3_0_only_shape_is_left_in_the_converted_official_schemas(self):
        self.assertEqual(list(conformance.leftovers(SCHEMA.components)), [])
        self.assertGreaterEqual(sum(1 for _ in conformance.leftovers(SCHEMA.doc["components"])), 20, "원본에는 nullable이 많다 — 변환이 하나도 안 된 것이 아니다")


class RequestNullable(unittest.TestCase):
    def test_the_fields_where_null_means_not_sent_come_from_the_pinned_request_schema(self):
        nullable = SCHEMA.request_nullable
        for name in ("max_tokens", "max_completion_tokens", "stop", "logprobs", "n", "seed", "temperature", "top_p", "store", "stream", "stream_options", "metadata"):
            self.assertIn(name, nullable)
        for name in ("tools", "tool_choice", "functions", "function_call", "messages", "model", "user", "response_format", "parallel_tool_calls"):
            self.assertNotIn(name, nullable, "공식 요청 스키마에서 null을 허용하지 않는다")


class OfficialShapes(unittest.TestCase):
    def test_a_well_formed_completion_chunk_and_error_pass(self):
        SCHEMA.check("CreateChatCompletionResponse", COMPLETION)
        SCHEMA.check("CreateChatCompletionStreamResponse", CHUNK)
        SCHEMA.check("ErrorResponse", ERROR)
        SCHEMA.check("ListModelsResponse", {"object": "list", "data": [{"id": "assistant", "object": "model", "created": 1, "owned_by": "bot-framework"}]})
        SCHEMA.check("Model", {"id": "assistant", "object": "model", "created": 1, "owned_by": "bot-framework"})

    def test_the_fields_the_official_schema_requires_are_enforced(self):
        for path in (("choices", 0, "logprobs"), ("choices", 0, "message", "refusal"), ("choices", 0, "finish_reason"), ("model",), ("created",)):
            with self.subTest(path=path):
                self.assertTrue(SCHEMA.errors("CreateChatCompletionResponse", without(COMPLETION, *path)))
        for path in ("param", "code", "type", "message"):
            with self.subTest(error=path):
                self.assertTrue(SCHEMA.errors("ErrorResponse", without(ERROR, "error", path)), "오류 객체는 null이어도 네 필드를 다 가진다")

    def test_null_where_the_schema_allows_it_and_not_where_it_does_not(self):
        SCHEMA.check("CreateChatCompletionStreamResponse", {**CHUNK, "usage": None})
        self.assertTrue(SCHEMA.errors("CreateChatCompletionResponse", {**COMPLETION, "usage": None}), "비스트리밍 usage는 null이 아니라 생략")
        self.assertTrue(SCHEMA.errors("CreateChatCompletionResponse", {**COMPLETION, "object": "chat.completion.chunk"}))
        self.assertTrue(SCHEMA.errors("CreateChatCompletionResponse", {**COMPLETION, "usage": {"prompt_tokens": 1, "completion_tokens": 1}}), "usage는 세 수를 다 가진다")

    def test_the_stream_finish_reason_is_a_nullable_enum(self):
        for reason in (None, "stop", "length"):
            SCHEMA.check("CreateChatCompletionStreamResponse", {**CHUNK, "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
        self.assertTrue(SCHEMA.errors("CreateChatCompletionStreamResponse", {**CHUNK, "choices": [{"index": 0, "delta": {}, "finish_reason": "done"}]}))


if __name__ == "__main__":
    unittest.main(verbosity=2)
