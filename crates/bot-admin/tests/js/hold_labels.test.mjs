import test from "node:test";
import assert from "node:assert/strict";
import { all, button, install, load, textOf } from "./support/fakedom.mjs";

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

test("the not-executed form explains what will happen to the call", async () => {
  const tool = { kind: "tool", profile: "personal", call: "call", tool: "fixture.post", reason: "effects_unresolved", since_ms: 0 };
  const step = { effect: "step", step: "post", state: "held", hold_id: "hold-2" };
  const confirmed = { effect: "first", step: "prepare", state: "confirmed", receipt: "receipt-1" };
  const started = { effect: "other", step: "other", state: "started" };
  const cases = [
    { steps: [confirmed, step], next: "호출 최종 결과 카드가 열립니다." },
    { steps: [confirmed, step, started], next: "남은 단계가 끝날 때까지 대기합니다." },
    { steps: [step], next: "같은 호출 ID로 다시 이어 갑니다." },
  ];
  const items = cases.map(({ steps }, i) => ({ ...tool, run: "run-" + i, steps }));
  window.BotApi = { q: () => "", get: async (path) => ({ items: path.endsWith("/holds") ? items : [] }) };
  load("holdpin.js", "decide.js");
  const box = document.createElement("section");
  const view = window.BotDecide.create({ me: () => ({ operator: true }), profile: () => "personal", noteProfiles() {} }, box);
  await view.load({ profile: "personal", current: () => true });

  const cards = all(box, (node) => node.classList.contains("hcard"));
  assert.equal(cards.length, cases.length);
  cards.forEach((card, i) => {
    const form = all(card, (node) => node.localName === "form" && node.getAttribute("aria-labelledby").startsWith("hf-not_executed-"))[0];
    const help = textOf(all(form, (node) => node.classList.contains("rf-help"))[0]);
    const input = all(form, (node) => node.localName === "input")[0];
    input.value = "수신처 조회에서 해당 요청 없음";
    form.dispatch("submit");
    const sum = all(form, (node) => node.classList.contains("rf-sum"))[0];
    const summary = textOf(sum);
    assert.equal(sum.hidden, false);
    button(form, "취소").click();
    const { next } = cases[i];
    assert.ok(help.includes(next), help);
    assert.ok(summary.includes(next), summary);
    assert.ok(summary.includes(input.value), summary);
    assert.equal(help.includes("같은 호출 ID로"), i === 2);
    assert.equal(summary.includes("같은 호출 ID로"), i === 2);
  });
});
