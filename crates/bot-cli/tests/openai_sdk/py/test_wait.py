"""Wait deadline tests (tight profile): 200 progress notice on timeout, same-key replay after completion, disconnect survival, and concurrent dedup."""
import json
import socket
import time
import unittest
import urllib.parse

from common import *
from test_stream import events, parsed

SLOW_MS = WAIT_MS + 2500
BODY = chat_body()


def start_and_hang_up(key, *, stream, after):
    url = urllib.parse.urlsplit(BASE)
    payload = json.dumps(chat_body(stream=True) if stream else BODY).encode()
    head = (f"POST {url.path}/chat/completions HTTP/1.1\r\nHost: {url.hostname}\r\nAuthorization: Bearer {KEY}\r\nContent-Type: application/json\r\n"
            f"Idempotency-Key: {key}\r\nContent-Length: {len(payload)}\r\n\r\n").encode()
    with socket.create_connection((url.hostname, url.port), timeout=10) as sock:
        sock.sendall(head + payload)
        if stream:
            sock.settimeout(10)
            assert sock.recv(4096), "stream did not start"
        time.sleep(after)


class Wait(Wired):
    def setUp(self):
        settle()
        super().setUp()

    def tearDown(self):
        settle()
        super().tearDown()

    def test_the_wait_ends_with_a_200_notice_and_the_same_request_collects_the_answer(self):
        script(text=ANSWER, delay_ms=SLOW_MS)
        key = fresh_key()
        send = lambda: client().chat.completions.with_raw_response.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        with Delta() as d:
            began = time.monotonic()
            first = send()
            took = time.monotonic() - began
            self.assertLess(took, WAIT_MS / 1000 + 1.5, "must return within wait deadline")
            data = first.http_response.json()
            SCHEMA.check("CreateChatCompletionResponse", data)
            message = data["choices"][0]["message"]["content"]
            self.assertTrue(message.startswith(NOTICE_PREFIX) and NOTICE_RETRY in message, message)
            self.assertEqual((first.http_response.status_code, data["choices"][0]["finish_reason"], first.headers["idempotency-key"]), (200, "stop", key))
            self.assertNotIn("usage", data, "progress notice must omit usage")
            self.assertEqual(stats()["active"], 1, "run must keep executing after deadline")
            settle()
            second = send().http_response.json()
            third = send().http_response.json()
        self.assertEqual((second["choices"][0]["message"]["content"], second["usage"]["total_tokens"]), (ANSWER, 13))
        self.assertEqual(second, third, "completed run must replay identically")
        self.assertEqual(second["id"], data["id"], "id/created/model must stay fixed for the same run")
        self.assertEqual((d.runs, d.model_calls), (1, 1), "re-querying must not start a new run")

    def test_the_stream_gets_the_same_notice_as_content_and_the_answer_comes_back_with_the_key(self):
        script(text=ANSWER, delay_ms=SLOW_MS)
        key = fresh_key()
        status, _, body = raw("POST", "/chat/completions", body=chat_body(stream=True), headers={"Idempotency-Key": key})
        data, _ = events(body)
        chunks = parsed(data)
        text = "".join(c["choices"][0]["delta"].get("content") or "" for c in chunks)
        self.assertTrue(status == 200 and text.startswith(NOTICE_PREFIX) and NOTICE_RETRY in text, text)
        self.assertEqual((data[-1], chunks[-1]["choices"][0]["finish_reason"]), ("[DONE]", "stop"))
        settle()
        with Delta() as d:
            again = client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        self.assertEqual((again.choices[0].message.content, d.runs, d.model_calls), (ANSWER, 0, 0))

    def test_a_client_that_hangs_up_after_acceptance_does_not_cancel_the_run(self):
        script(text=ANSWER, delay_ms=1500)
        key = fresh_key()
        with Delta() as d:
            start_and_hang_up(key, stream=False, after=0.2)
            settle()
            answer = client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        self.assertEqual((answer.choices[0].message.content, d.runs, d.model_calls), (ANSWER, 1, 1))

    def test_a_streaming_client_that_hangs_up_does_not_cancel_the_run(self):
        script(text=ANSWER, delay_ms=1500)
        key = fresh_key()
        with Delta() as d:
            start_and_hang_up(key, stream=True, after=0.1)
            settle()
            answer = client().chat.completions.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key})
        self.assertEqual((answer.choices[0].message.content, d.runs, d.model_calls), (ANSWER, 1, 1))

    def test_two_requests_with_one_key_at_once_run_once(self):
        import concurrent.futures
        script(text=ANSWER, delay_ms=SLOW_MS)
        key = fresh_key()
        send = lambda _: client().chat.completions.with_raw_response.create(model=MODEL, messages=USER, extra_headers={"Idempotency-Key": key}).http_response.json()
        with Delta() as d, concurrent.futures.ThreadPoolExecutor(2) as pool:
            first, second = pool.map(send, range(2))
            settle()
        self.assertEqual((first["id"], d.runs, d.model_calls), (second["id"], 1, 1))


if __name__ == "__main__":
    unittest.main(verbosity=2)
