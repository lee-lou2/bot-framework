// Pure unit tests for pre-approval rules (ui/preapproval.js).
import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { row } from "./support/contract.mjs";

const R = createRequire(import.meta.url)("../../ui/preapproval.js");

test("a row is usable only when every required field has exactly the right type", () => {
  assert.equal(R.valid(row()), true);
  assert.equal(R.valid({ ...row(), extra: { anything: 1 } }), true, "unknown fields are ignored");
  const bad = {
    "non-string profile": { profile: 7 }, "empty agent": { agent: "" }, "missing trigger": { trigger: undefined }, "null grant_id": { grant_id: null },
    "string epoch": { epoch: "4" }, "fractional epoch": { epoch: 4.5 }, "negative epoch": { epoch: -1 }, "unsafe integer epoch": { epoch: 2 ** 60 }, "NaN epoch": { epoch: NaN },
    "string paused": { paused: "false" }, "numeric paused": { paused: 0 }, "missing paused": { paused: undefined },
    "string used_today": { used_today: "3" }, "fractional used_today": { used_today: 2.5 }, "negative used_today": { used_today: -2 },
    "null max_per_day": { max_per_day: null }, "boolean max_per_day": { max_per_day: true },
    "control char": { trigger: "no\ntify" }, "key separator |": { agent: "a|b" }, "overlong text": { grant_id: "x".repeat(257) },
  };
  for (const [name, over] of Object.entries(bad)) assert.equal(R.valid({ ...row(), ...over }), false, name);
  for (const nope of [null, undefined, 7, "row", [], [row()]]) assert.equal(R.valid(nope), false, String(nope));
});

test("a list response must be {items: [...]}; unreadable and duplicated rows are skipped by count and never get a button", () => {
  for (const shape of [null, undefined, [], [row()], {}, { items: null }, { items: "x" }, { items: {} }, { rows: [row()] }]) assert.equal(R.parse(shape), null, JSON.stringify(shape));
  const a = row({ trigger: "a" }), b = row({ trigger: "b", paused: true }), twin1 = row({ trigger: "dup", grant_id: "g-1" }), twin2 = row({ trigger: "dup", grant_id: "g-2" });
  const got = R.parse({ items: [b, { ...a, epoch: "x" }, a, twin1, twin2, 42] });
  assert.deepEqual(got.rows.map(R.key), ["personal|main/developer|a", "personal|main/developer|b"]);
  assert.equal(got.skipped, 4, "2 unreadable rows plus 2 ambiguous duplicate-key rows");
  assert.deepEqual(R.parse({ items: [] }), { rows: [], skipped: 0 });
});

test("rows are sorted by (profile, agent, trigger) whatever order the server used", () => {
  const rows = [row({ profile: "b", trigger: "x" }), row({ profile: "a", agent: "main/z", trigger: "x" }), row({ profile: "a", agent: "main/a", trigger: "y" }), row({ profile: "a", agent: "main/a", trigger: "b" })];
  assert.deepEqual(R.parse({ items: rows }).rows.map(R.key), ["a|main/a|b", "a|main/a|y", "a|main/z|x", "b|main/developer|x"]);
  assert.deepEqual(R.parse({ items: rows.slice().reverse() }).rows.map(R.key), R.parse({ items: rows }).rows.map(R.key), "order-independent");
});

test("the offered action is the opposite of the current state and only that action can be armed", () => {
  const running = row(), paused = row({ paused: true });
  assert.equal(R.offered(running), "pause");
  assert.equal(R.offered(paused), "resume");
  assert.equal(R.arm(running, "resume"), null, "active row cannot be resumed");
  assert.equal(R.arm(paused, "pause"), null, "paused row cannot be paused again");
  assert.equal(R.arm(running, "delete"), null);
  assert.equal(R.arm({ ...running, epoch: "4" }, "pause"), null, "invalid row cannot be armed");
  assert.deepEqual(R.arm(running, "pause"), { key: "personal|main/developer|notify", action: "pause", profile: "personal", agent: "main/developer", trigger: "notify", grant_id: "grant-0001-aaaa", epoch: 4 });
});

test("the request goes to one path per identifier, each encoded exactly once, and carries only grant_id and the epoch that was seen", () => {
  const pin = R.arm(row(), "pause");
  assert.equal(R.path(pin), "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/pause", "slash in agent stays one segment");
  assert.equal(R.path(R.arm(row({ paused: true }), "resume")), "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/resume");
  const odd = R.arm(row({ profile: "p", agent: "main/a b?c#d%e", trigger: "t&u=v" }), "pause");
  assert.equal(R.path(odd), "/v1/admin/preapprovals/p/main%2Fa%20b%3Fc%23d%25e/t%26u%3Dv/pause", "single-encoded without double encoding");
  assert.deepEqual(R.payload(pin), { grant_id: "grant-0001-aaaa", epoch: 4 });
  assert.deepEqual(Object.keys(R.payload(pin)).sort(), ["epoch", "grant_id"], "server rejects unknown keys");
  assert.equal(R.payload(null), null);
  assert.throws(() => R.path({ ...pin, action: "delete" }), /Unknown action/);
});

test("the payload is what was pinned, not what the row says now", () => {
  const pin = R.arm(row({ epoch: 4 }), "pause");
  const later = row({ epoch: 9, grant_id: "grant-0002-bbbb" });
  assert.deepEqual(R.payload(pin), { grant_id: "grant-0001-aaaa", epoch: 4 });
  assert.notDeepEqual(R.payload(pin), { grant_id: later.grant_id, epoch: later.epoch });
});

test("a pending resume survives a refresh only while the same approval is at the same epoch and still paused", () => {
  const pin = R.arm(row({ paused: true, epoch: 5 }), "resume");
  assert.deepEqual(R.reconcile(pin, [row({ paused: true, epoch: 5, used_today: 20 })]), { ok: true }, "usage change does not invalidate confirmation");
  assert.deepEqual(R.reconcile(null, []), { ok: true });
  assert.deepEqual(R.reconcile(pin, []), { ok: false, reason: "gone" });
  assert.deepEqual(R.reconcile(pin, [row({ trigger: "other", paused: true, epoch: 5 })]), { ok: false, reason: "gone" });
  assert.deepEqual(R.reconcile(pin, [row({ paused: true, epoch: 5, grant_id: "grant-0002-bbbb" })]), { ok: false, reason: "renewed" });
  assert.deepEqual(R.reconcile(pin, [row({ paused: true, epoch: 7 })]), { ok: false, reason: "changed" }, "epoch changed after round-trip toggle");
  assert.deepEqual(R.reconcile(pin, [row({ paused: false, epoch: 6 })]), { ok: false, reason: "changed" }, "already resumed elsewhere");
  for (const why of ["gone", "renewed", "changed"]) assert.ok(R.WHY[why].includes("Confirmation cleared"), why);
});

test("only a valid reply for the same target and approval whose state is the requested result is accepted", () => {
  const pause = R.arm(row({ epoch: 4 }), "pause"), resume = R.arm(row({ paused: true, epoch: 5 }), "resume");
  assert.deepEqual(R.settle(pause, row({ paused: true, epoch: 5 })), row({ paused: true, epoch: 5 }));
  assert.deepEqual(R.settle(resume, row({ paused: false, epoch: 6 })), row({ paused: false, epoch: 6 }));
  const rejected = {
    "wrong state for pause": [pause, row({ paused: false, epoch: 5 })],
    "wrong state for resume": [resume, row({ paused: true, epoch: 6 })],
    "different trigger": [pause, row({ trigger: "other", paused: true })],
    "different grant_id": [pause, row({ paused: true, grant_id: "grant-0002-bbbb" })],
    "invalid required field": [pause, { ...row({ paused: true }), epoch: "5" }],
    "non-row body": [pause, { ok: true }],
    "wrapped row": [pause, { item: row({ paused: true }) }],
    "null body": [pause, null],
  };
  for (const [name, [pin, reply]] of Object.entries(rejected)) assert.equal(R.settle(pin, reply), null, name);
});

test("replacing a row keeps the others, the order and does not touch the input", () => {
  const rows = R.parse({ items: [row({ trigger: "a" }), row({ trigger: "b" })] }).rows, frozen = JSON.stringify(rows);
  const next = R.replace(rows, row({ trigger: "a", paused: true, epoch: 5 }));
  assert.deepEqual(next.map((r) => [r.trigger, r.paused]), [["a", true], ["b", false]]);
  assert.equal(JSON.stringify(rows), frozen);
  assert.deepEqual(R.replace(rows, row({ trigger: "0new" })).map((r) => r.trigger), ["0new", "a", "b"], "missing row inserts in sorted position");
});

test("usage is clamped, rounded down and flags the daily limit", () => {
  assert.deepEqual(R.usage(row({ used_today: 3, max_per_day: 24 })), { used: 3, max: 24, pct: 12, left: 21, full: false });
  assert.deepEqual(R.usage(row({ used_today: 24, max_per_day: 24 })), { used: 24, max: 24, pct: 100, left: 0, full: true });
  assert.deepEqual(R.usage(row({ used_today: 30, max_per_day: 24 })), { used: 30, max: 24, pct: 100, left: 0, full: true }, "meter percentage clamps at 100");
  assert.equal(R.usage(row({ used_today: 1, max_per_day: 3 })).pct, 33);
  assert.equal(R.usage(row({ used_today: 0, max_per_day: 0 })).full, true);
  assert.deepEqual(R.counts([row(), row({ paused: true }), row({ paused: true })]), { total: 3, paused: 2 });
});

test("every server word of the contract has a sentence and an unknown outcome says it may or may not have been applied", () => {
  for (const word of ["stale", "denied", "not_found", "invalid", "unavailable", "network", "timeout", "mismatch"]) assert.ok(R.ERR[word] && R.ERR[word].length > 10, word);
  for (const word of ["unavailable", "network", "timeout"]) assert.ok(R.ERR[word].includes("could not confirm whether the change was applied"), word);
  assert.ok(R.ERR.stale.includes("The list was reloaded"));
});
