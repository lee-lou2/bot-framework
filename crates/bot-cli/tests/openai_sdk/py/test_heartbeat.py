"""기다리는 동안 SSE 연결은 살아 있다: 최대 15초마다 `: keep-alive` 주석 줄, 그 사이 첫 청크(역할)는 곧바로 나가고, 끝나면 답과 `[DONE]`. 실제로 17초쯤 걸린다."""
import http.client
import json
import time
import unittest
import urllib.parse

from common import *

HOLD_S = 17


class Heartbeat(Wired):
    def test_a_waiting_stream_sends_the_role_chunk_at_once_and_a_keep_alive_within_15_seconds(self):
        script(text=ANSWER, delay_ms=HOLD_S * 1000)
        url = urllib.parse.urlsplit(BASE)
        conn = http.client.HTTPConnection(url.hostname, url.port, timeout=40)
        body = json.dumps(chat_body(stream=True)).encode()
        began = time.monotonic()
        conn.request("POST", url.path + "/chat/completions", body=body, headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})
        reply = conn.getresponse()
        lines, marks = [], []
        while True:
            line = reply.fp.readline()
            if not line:
                break
            lines.append(line)
            marks.append(time.monotonic() - began)
            if line.startswith(b"data: [DONE]"):
                break
        conn.close()
        self.assertEqual(reply.status, 200)
        first_data = next(i for i, l in enumerate(lines) if l.startswith(b"data: "))
        self.assertLess(marks[first_data], 3, "첫 청크(역할)는 기다리지 않고 나간다")
        beats = [marks[i] for i, l in enumerate(lines) if l == b": keep-alive\n"]
        self.assertTrue(beats, "기다리는 동안 keep-alive가 한 번은 와야 한다")
        self.assertLessEqual(beats[0] - marks[first_data], 15.5)
        gaps = [b - a for a, b in zip([marks[first_data]] + beats, beats)]
        self.assertTrue(all(g <= 15.5 for g in gaps), gaps)
        text = b"".join(lines).decode()
        payloads = [json.loads(l[len(b"data: "):]) for l in lines if l.startswith(b"data: ") and not l.startswith(b"data: [DONE]")]
        self.assertEqual("".join(c["choices"][0]["delta"].get("content") or "" for c in payloads if c["choices"]), ANSWER, "조각으로 나뉘어 와도 이은 글이 답이다")
        self.assertTrue(text.endswith("data: [DONE]\n"), text[-60:])


if __name__ == "__main__":
    unittest.main(verbosity=2)
