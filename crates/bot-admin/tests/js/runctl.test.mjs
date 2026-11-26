// 실행 상세의 명령 가시성과 확인·재전송에 쓰는 요청을 확인한다.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import test from "node:test";

const { offered, canConfirm, create } = createRequire(import.meta.url)("../../ui/runctl.js");
const detail = (state = "dead") => ({
  run: { profile: "personal", run: "r1", root: "r1", parent: null, state, generation: 3 },
  control: { claim_epoch: 5, cancel_requested: false, held: false },
});

test("run controls require an operator and a non-private root observation", () => {
  const d = detail();
  assert.equal(offered(d, false), null);
  assert.equal(offered({ ...d, control: null }, true), null);
  assert.equal(offered({ ...d, run: { ...d.run, root: "other", parent: "other" } }, true), null);
  assert.equal(offered(d, true), "resume");
  for (const state of ["ready", "retry", "waiting", "leased"]) assert.equal(offered(detail(state), true), "cancel");
  for (const state of ["succeeded", "cancelled"]) assert.equal(offered(detail(state), true), null);
  assert.equal(offered({ ...d, control: { ...d.control, cancel_requested: true } }, true), null);
  assert.equal(offered({ ...d, control: { ...d.control, held: true } }, true), "held");
});

test("a resume confirmation rejects a double click and expires", () => {
  for (const elapsed of [0, 599, 12000]) assert.equal(canConfirm(elapsed), false);
  for (const elapsed of [600, 11999]) assert.equal(canConfirm(elapsed), true);
});

function surface(post) {
  globalThis.BotDom = { h: (_, attrs, children) => ({
    ...attrs, children, classList: { toggle() {} }, isConnected: true, closest: () => null,
  }) };
  globalThis.BotApi = { seg: encodeURIComponent, post };
  const nav = [];
  const controller = create({ toast() {}, refresh() {}, go: (...args) => nav.push(args) });
  return { controller, nav };
}

test("run resume pins the first observation and resends the identical command after a lost response", async (t) => {
  const now = t.mock.method(Date, "now", () => 1000);
  const sent = [];
  const { controller } = surface(async (path, body) => {
    sent.push({ path, body });
    if (sent.length === 1) throw { word: "network" };
    return { ok: true };
  });
  const d = detail();
  const button = controller.mount(d, "resume").children[0];
  button.onclick();
  button.onclick();
  assert.equal(sent.length, 0);
  d.run.generation = 7;
  now.mock.mockImplementation(() => 1700);
  await button.onclick();
  assert.match(sent[0].body.command_id, /^[0-9a-f]{32}$/);
  assert.deepEqual(sent[0], {
    path: "/v1/admin/runs/personal/r1/resume",
    body: { command_id: sent[0].body.command_id, state: "dead", generation: 3, claim_epoch: 5 },
  });
  await button.onclick();
  assert.deepEqual(sent, [sent[0], sent[0]]);
  assert.equal(button.disabled, true);
});

test("a blocked run opens the hold view without sending a command", () => {
  const { controller, nav } = surface(() => assert.fail("보류에는 명령을 보내지 않는다"));
  const d = detail("waiting");
  d.control.held = true;
  const button = controller.mount(d, offered(d, true)).children[0];
  button.onclick();
  assert.deepEqual(nav, [["decide", { profile: "personal" }]]);
});
