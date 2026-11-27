// Pure unit tests for hold-resolution pinning rules (ui/holdpin.js).
import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const P = createRequire(import.meta.url)("../../ui/holdpin.js");
const FIELDS = { confirmed: "receipt", not_executed: "evidence" };
const steps = (...rows) => rows.map(([effect, state, hold]) => ({ effect, state, step: "s-" + effect, hold_id: hold === undefined ? null : hold }));
const bundle = (...rows) => ({ hold_id: null, steps: steps(...rows) });
const single = (id) => ({ hold_id: id, steps: [] });

test("the first confirmation pins the step, its hold id, the verdict and the typed text", () => {
  const pin = P.arm("confirmed", "e-2", "  msg-9  ", "h-7");
  assert.deepEqual(pin, { kind: "confirmed", effect: "e-2", text: "  msg-9  ", hold: "h-7" });
  assert.deepEqual(P.payload(pin, FIELDS), { verdict: "confirmed", receipt: "msg-9", effect: "e-2", hold_id: "h-7" });
  assert.deepEqual(P.payload(P.arm("not_executed", "e-3", "No record at destination", "h-8"), FIELDS), { verdict: "not_executed", evidence: "No record at destination", effect: "e-3", hold_id: "h-8" });
});

test("a call-wide hold sends its own hold id and no effect field", () => {
  const body = P.payload(P.arm("not_executed", null, "evidence", "h-1"), FIELDS);
  assert.deepEqual(body, { verdict: "not_executed", evidence: "evidence", hold_id: "h-1" });
  assert.equal("effect" in body, false);
  assert.equal(P.arm("confirmed", undefined, "x", "h-1").effect, null);
});

test("a target without a hold id can be neither armed nor sent: the server takes no judgment without one", () => {
  const view = bundle(["e-2", "held"]);
  assert.equal(P.holdIdFor(view, "e-2"), null);
  assert.equal(P.decidable(view, "e-2"), false);
  for (const none of [undefined, null, ""]) assert.equal(P.arm("confirmed", "e-2", "r", none), null, String(none));
  assert.equal(P.payload(P.arm("confirmed", "e-2", "r"), FIELDS), null);
  assert.equal(P.payload({ kind: "confirmed", effect: "e-2", text: "r", hold: null }, FIELDS), null);
  const mixed = bundle(["A", "held"], ["B", "held", "h-B"]);
  assert.deepEqual([P.decidable(mixed, "A"), P.decidable(mixed, "B")], [false, true]);
  assert.deepEqual([P.decidable(single("h-9"), null), P.decidable(single(null), null), P.decidable(bundle(["A", "confirmed"]), null)], [true, false, false]);
  assert.deepEqual(P.retarget("A", null, mixed), { effect: "A", holdId: null, changed: false });
});

test("the card and form key carries the hold kind: a tool hold and a reply hold of one call are two cards", () => {
  const tool = { profile: "personal", run: "r-1", call: "c-1", kind: "tool", hold_id: "h-t", steps: [] };
  const reply = { ...tool, kind: "reply", hold_id: "h-r" };
  const dm = { ...tool, kind: "private" };
  const keys = [tool, reply, dm].map(P.keyOf);
  assert.equal(new Set(keys).size, 3, "holds differing only by kind must have distinct keys");
  assert.deepEqual(keys[0].split("|"), ["personal", "r-1", "c-1", "tool"]);
  assert.equal(P.keyOf({ ...tool, hold_id: "h-new", steps: [{ effect: "e" }] }), keys[0]);
  assert.notEqual(P.keyOf({ ...tool, run: "r-2" }), keys[0]);
  assert.notEqual(P.keyOf({ ...tool, call: "c-2" }), keys[0]);
  assert.notEqual(P.keyOf({ ...tool, profile: "work" }), keys[0]);
  assert.notEqual(P.keyOf({ ...tool, kind: "future" }), keys[0]);
});

test("two cards of one call are judged separately: each pins its own hold id and neither pin touches the other", () => {
  const tool = { profile: "personal", run: "r-1", call: "c-1", kind: "tool", hold_id: "h-t", steps: [] };
  const reply = { ...tool, kind: "reply", hold_id: "h-r" };
  const forms = {};
  for (const hold of [tool, reply]) forms[P.keyOf(hold)] = { holdId: P.holdIdFor(hold, null) };
  const pinTool = P.arm("confirmed", null, "msg-1", forms[P.keyOf(tool)].holdId);
  const pinReply = P.arm("not_executed", null, "No record at destination", forms[P.keyOf(reply)].holdId);
  assert.deepEqual(P.payload(pinTool, FIELDS), { verdict: "confirmed", receipt: "msg-1", hold_id: "h-t" });
  assert.deepEqual(P.payload(pinReply, FIELDS), { verdict: "not_executed", evidence: "No record at destination", hold_id: "h-r" });
  assert.deepEqual(P.reconcile(pinReply, reply), { ok: true });
  assert.deepEqual(P.reconcile(pinTool, tool), { ok: true });
});

test("the hold id comes from the step for stepped holds and from the hold itself for call-wide ones", () => {
  const view = bundle(["A", "held", "h-A"], ["B", "held", "h-B"]);
  assert.equal(P.holdIdFor(view, "B"), "h-B");
  assert.equal(P.holdIdFor(view, "zz"), null);
  assert.equal(P.holdIdFor(single("h-9"), null), "h-9");
  assert.equal(P.holdIdFor(single("h-9"), undefined), "h-9");
});

test("nothing is sent before a confirmation exists", () => {
  assert.equal(P.payload(null, FIELDS), null);
  assert.deepEqual(P.reconcile(null, bundle(["e-1", "held"])), { ok: true });
});

test("the reported race: step A resolved elsewhere while armed — the receipt for A must never reach B", () => {
  const pin = P.arm("confirmed", "A", "r-A", "h-A");
  const after = bundle(["A", "confirmed", null], ["B", "held", "h-B"]);
  assert.deepEqual(P.held(after.steps), ["B"]);
  const check = P.reconcile(pin, after);
  assert.equal(check.ok, false, "confirmation must be dropped when the pinned step is no longer held");
  assert.equal(check.reason, "gone");
  assert.ok(P.WHY[check.reason].length > 10);
  assert.deepEqual([P.payload(pin, FIELDS).effect, P.payload(pin, FIELDS).hold_id], ["A", "h-A"]);
});

test("the pin stays valid while the same hold is still open, whatever else changes", () => {
  const pin = P.arm("not_executed", "B", "evidence", "h-B");
  assert.deepEqual(P.reconcile(pin, bundle(["A", "confirmed"], ["B", "held", "h-B"])), { ok: true });
  assert.deepEqual(P.reconcile(pin, bundle(["A", "held", "h-A"], ["B", "held", "h-B"], ["C", "held", "h-C"])), { ok: true });
});

test("the same step held again under a new hold id is a new hold: the old confirmation is dropped", () => {
  const pin = P.arm("confirmed", "A", "r-for-H1", "h-1");
  const renewed = P.reconcile(pin, bundle(["A", "held", "h-2"], ["B", "held", "h-B"]));
  assert.deepEqual(renewed, { ok: false, reason: "renewed" });
  assert.ok(P.WHY.renewed);
  const callPin = P.arm("not_executed", null, "evidence", "h-1");
  assert.deepEqual(P.reconcile(callPin, single("h-1")), { ok: true });
  assert.deepEqual(P.reconcile(callPin, single("h-2")), { ok: false, reason: "renewed" });
});

test("a step judged not-executed (shown as started) is no longer selectable", () => {
  const pin = P.arm("confirmed", "A", "r", "h-A");
  assert.equal(P.reconcile(pin, bundle(["A", "started"], ["B", "held", "h-B"])).ok, false);
  assert.equal(P.reconcile(pin, bundle(["A", "rejected"], ["B", "held", "h-B"])).ok, false);
  assert.equal(P.reconcile(pin, bundle()).ok, false);
  assert.equal(P.reconcile(pin, { steps: undefined }).ok, false);
});

test("a call-wide pin is invalid once the hold turns into a stepped one", () => {
  const pin = P.arm("confirmed", null, "r", "h-1");
  assert.deepEqual(P.reconcile(pin, single("h-1")), { ok: true });
  const stepped = P.reconcile(pin, bundle(["A", "held", "h-A"]));
  assert.equal(stepped.ok, false);
  assert.equal(stepped.reason, "stepped");
  assert.ok(P.WHY.stepped);
});

test("held() lists only held steps in order and tolerates missing data", () => {
  assert.deepEqual(P.held(steps(["A", "held"], ["B", "started"], ["C", "held"], ["D", "confirmed"])), ["A", "C"]);
  assert.deepEqual(P.held(undefined), []);
  assert.deepEqual(P.held([]), []);
});

test("the selected step follows the list, and a changed target or a renewed hold means the typed text must go", () => {
  assert.deepEqual(P.retarget("B", "h-B", bundle(["A", "held", "h-A"], ["B", "held", "h-B"])), { effect: "B", holdId: "h-B", changed: false });
  assert.deepEqual(P.retarget("A", "h-A", bundle(["A", "confirmed"], ["B", "held", "h-B"])), { effect: "B", holdId: "h-B", changed: true });
  assert.deepEqual(P.retarget("A", "h-A", bundle(["A", "started"], ["B", "held", "h-B"], ["C", "held", "h-C"])), { effect: "B", holdId: "h-B", changed: true });
  assert.deepEqual(P.retarget("A", "h-1", bundle(["A", "held", "h-2"], ["B", "held", "h-B"])), { effect: "A", holdId: "h-2", changed: true });
  assert.deepEqual(P.retarget(null, "h-1", single("h-2")), { effect: null, holdId: "h-2", changed: true });
  assert.deepEqual(P.retarget(null, "h-1", single("h-1")), { effect: null, holdId: "h-1", changed: false });
  assert.deepEqual(P.retarget(null, undefined, bundle(["A", "held", "h-A"], ["B", "held", "h-B"])), { effect: "A", holdId: "h-A", changed: false });
  assert.deepEqual(P.retarget(undefined, null, single("h-1")), { effect: null, holdId: "h-1", changed: false });
  assert.deepEqual(P.retarget("A", null, bundle(["A", "held"], ["B", "held"])), { effect: "A", holdId: null, changed: false });
  assert.deepEqual(P.retarget("A", null, bundle(["A", "confirmed"], ["B", "held"])), { effect: "B", holdId: null, changed: true });
  assert.deepEqual(P.retarget("A", "h-A", bundle(["A", "confirmed"])), { effect: null, holdId: null, changed: true });
});

test("only «already decided / different» and «not found» answers make the screen drop the typed text", () => {
  assert.equal(P.staleWord("conflict"), true);
  assert.equal(P.staleWord("not_found"), true);
  for (const other of ["invalid", "denied", "csrf", "unavailable", "stale", "changed", "no_body", "", undefined, null]) {
    assert.equal(P.staleWord(other), false, String(other));
  }
});

test("a held step that already got a not-executed verdict is not selectable, whatever state the store reports", () => {
  const judged = (state) => ({ effect: "A", state, resolution: "not_executed", step: "s-A", hold_id: "h-A" });
  for (const state of ["held", "started"]) assert.equal(P.open(judged(state)), false, state);
  assert.equal(P.open({ effect: "B", state: "held", resolution: null }), true);
  assert.equal(P.open({ effect: "B", state: "held" }), true, "compatible with stores omitting resolution");
  assert.equal(P.open({ effect: "C", state: "started", resolution: null }), false, "started steps are not selectable");
  const view = { hold_id: null, steps: [judged("held"), { effect: "B", state: "held", hold_id: "h-B" }] };
  assert.deepEqual(P.held(view.steps), ["B"]);
  assert.equal(P.reconcile(P.arm("not_executed", "A", "evidence", "h-A"), view).ok, false);
  assert.deepEqual(P.retarget("A", "h-A", view), { effect: "B", holdId: "h-B", changed: true });
});

test("the double failure: A rejected as already decided, then B (the only one left) starts clean", () => {
  let text = "RCPT-FOR-A";
  if (P.staleWord("conflict")) text = "";
  const sel = P.retarget("A", "h-A", bundle(["A", "confirmed"], ["B", "held", "h-B"]));
  if (sel.changed) text = "";
  assert.equal(sel.effect, "B");
  assert.equal(text, "", "receipt typed for A must not remain in B's input");
  assert.equal(P.payload(null, { confirmed: "receipt" }), null);
});

test("the lost-answer retry: H1 committed, the same target is held again as H2 — the H1 retry keeps H1's id, H2 starts clean", () => {
  const pin = P.arm("confirmed", "A", "r-1", "h-1");
  assert.equal(P.payload(pin, FIELDS).hold_id, "h-1");
  assert.equal(P.reconcile(pin, bundle(["A", "held", "h-2"])).ok, false);
  assert.equal(P.retarget("A", "h-1", bundle(["A", "held", "h-2"])).changed, true);
});

test("a confirmed step pins its separate final receipt without allowing a whole-call retry", () => {
  const h = { hold_id: "h-final", steps: steps(["one", "confirmed"]) };
  const pin = P.arm("confirmed", null, "overall receipt", h.hold_id);
  assert.deepEqual(P.reconcile(pin, h), { ok: true });
  assert.equal(P.reconcile(pin, { ...h, hold_id: "h-next" }).ok, false);
  assert.equal(P.reconcile(P.arm("not_executed", null, "evidence", h.hold_id), h).ok, false);
});
