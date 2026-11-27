"""Official Python SDK (2.29.0) models endpoint tests: visibility, sorting, stable created timestamp, hidden-model 404 parity, and non-member 403."""
import json
import time
import unittest

import openai

from common import *


class Models(Wired):
    def test_the_list_is_sorted_visible_only_and_matches_the_official_schema(self):
        reply = client().models.with_raw_response.list()
        data = reply.http_response.json()
        SCHEMA.check("ListModelsResponse", data)
        ids = [m["id"] for m in data["data"]]
        self.assertEqual(ids, sorted(ids))
        self.assertIn(MODEL, ids)
        self.assertNotIn(HIDDEN, ids, "models outside this key's role must be omitted")
        self.assertTrue(all(m["object"] == "model" and m["owned_by"] == "bot-framework" and isinstance(m["created"], int) for m in data["data"]))
        self.assertEqual(reply.parse().object, "list")

    def test_retrieve_one_model_and_created_is_stable_not_the_current_time(self):
        first = client().models.with_raw_response.retrieve(MODEL).http_response.json()
        SCHEMA.check("Model", first)
        listed = next(m for m in client().models.list().data if m.id == MODEL)
        time.sleep(1.2)
        second = client().models.retrieve(MODEL)
        self.assertEqual(first["created"], listed.created)
        self.assertEqual(first["created"], second.created, "created timestamp must be stable across requests")
        self.assertEqual(second.id, MODEL)

    def test_a_model_the_key_may_not_use_is_indistinguishable_from_a_missing_one(self):
        def refusal(path_model):
            status, headers, body = raw("GET", f"/models/{path_model}")
            return status, headers["content-type"].split(";")[0], json.loads(body)

        hidden, missing = refusal(HIDDEN), refusal("no-such-model-x")
        self.assertEqual((hidden[0], missing[0]), (404, 404))
        assert_error(self, hidden[2], 404, "invalid_request_error", "model_not_found")
        scrub = lambda body, name: json.loads(json.dumps(body, ensure_ascii=False).replace(name, "<model>"))
        self.assertEqual(scrub(hidden[2], HIDDEN), scrub(missing[2], "no-such-model-x"), "hidden and missing models must return identical 404 bodies")
        self.assertEqual(hidden[1], missing[1])
        with self.assertRaises(openai.NotFoundError):
            client().models.retrieve(HIDDEN)
        with Delta() as d:
            with self.assertRaises(openai.NotFoundError):
                client().chat.completions.create(model=HIDDEN, messages=USER)
        self.assertEqual((d.runs, d.model_calls), (0, 0), "inaccessible models must not be accepted or executed")

    def test_the_second_key_slot_of_the_same_subject_sees_the_same_models(self):
        ids = lambda key: [m.id for m in client(key).models.list().data]
        self.assertEqual(ids(KEY), ids(ROTATED))

    def test_a_valid_key_that_is_not_a_member_is_refused_403_everywhere_and_nothing_runs(self):
        with Delta() as d:
            for call in (lambda c: c.models.list(), lambda c: c.models.retrieve(MODEL), lambda c: c.chat.completions.create(model=MODEL, messages=USER)):
                with self.assertRaises(openai.PermissionDeniedError) as caught:
                    call(client(GUEST))
                self.assertEqual(caught.exception.status_code, 403)
                assert_error(self, {"error": caught.exception.body}, 403, "permission_error", "access_denied")
        self.assertEqual((d.runs, d.model_calls), (0, 0))


if __name__ == "__main__":
    unittest.main(verbosity=2)
