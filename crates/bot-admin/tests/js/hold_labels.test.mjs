import test from "node:test";
import assert from "node:assert/strict";
import { all, install, load, textOf } from "./support/fakedom.mjs";

install();
load("dom.js", "labels.js");
const { holdReason } = window.BotLabels;

test("the known call recovery reasons have operator-facing labels", () => {
  assert.equal(holdReason("response_unknown"), "최종 응답을 알 수 없음");
  assert.equal(holdReason("effects_unresolved"), "미확정 단계 있음");
  assert.equal(holdReason("previous_process_unconfirmed"), "이전 실행기 종료 미확인");
  assert.equal(holdReason("other"), "기타");
});

test("the last pending step warns that the final call needs its own receipt", async () => {
  const tool = { kind: "tool", profile: "personal", run: "run", call: "call", tool: "fixture.post", reason: "response_unknown", since_ms: 0 };
  const step = { effect: "step", step: "post", state: "held", hold_id: "hold-1" };
  const items = [
    { ...tool, steps: [step] },
    { ...tool, run: "run-other", hold_id: "hold-2", steps: [] },
  ];
  window.BotApi = { q: () => "", get: async (path) => ({ items: path.endsWith("/holds") ? items : [] }) };
  load("holdpin.js", "decide.js");
  const box = document.createElement("section");
  const view = window.BotDecide.create({ me: () => ({ operator: true }), profile: () => "personal", noteProfiles() {} }, box);
  await view.load({ profile: "personal", current: () => true });
  const hint = "마지막 단계의 영수증을 기록하면 호출 최종 결과 카드가 열립니다";
  const cards = all(box, (node) => node.classList.contains("hcard"));
  assert.equal(cards.length, 2);
  assert.ok(textOf(cards[0]).includes(hint));
  assert.ok(!textOf(cards[1]).includes(hint));
});
