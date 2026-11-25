// 사전 승인 화면의 순수 규칙(ui/preapproval.js). 화면 요소 없이 시험한다.
import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { row } from "./support/contract.mjs";

const R = createRequire(import.meta.url)("../../ui/preapproval.js");

test("a row is usable only when every required field has exactly the right type", () => {
  assert.equal(R.valid(row()), true);
  assert.equal(R.valid({ ...row(), extra: { anything: 1 } }), true, "모르는 필드는 무시한다");
  const bad = {
    "문자열이 아닌 profile": { profile: 7 }, "빈 agent": { agent: "" }, "trigger 없음": { trigger: undefined }, "grant_id가 null": { grant_id: null },
    "epoch가 문자열": { epoch: "4" }, "epoch가 소수": { epoch: 4.5 }, "epoch가 음수": { epoch: -1 }, "epoch가 안전하지 않은 큰 수": { epoch: 2 ** 60 }, "epoch가 NaN": { epoch: NaN },
    "paused가 문자열": { paused: "false" }, "paused가 0": { paused: 0 }, "paused 없음": { paused: undefined },
    "used_today가 문자열": { used_today: "3" }, "used_today가 소수": { used_today: 2.5 }, "used_today가 음수": { used_today: -2 },
    "max_per_day가 null": { max_per_day: null }, "max_per_day가 불리언": { max_per_day: true },
    "제어 문자": { trigger: "no\ntify" }, "키 구분자 |": { agent: "a|b" }, "너무 긴 글": { grant_id: "x".repeat(257) },
  };
  for (const [name, over] of Object.entries(bad)) assert.equal(R.valid({ ...row(), ...over }), false, name);
  for (const nope of [null, undefined, 7, "row", [], [row()]]) assert.equal(R.valid(nope), false, String(nope));
});

test("a list response must be {items: [...]}; unreadable and duplicated rows are skipped by count and never get a button", () => {
  for (const shape of [null, undefined, [], [row()], {}, { items: null }, { items: "x" }, { items: {} }, { rows: [row()] }]) assert.equal(R.parse(shape), null, JSON.stringify(shape));
  const a = row({ trigger: "a" }), b = row({ trigger: "b", paused: true }), twin1 = row({ trigger: "dup", grant_id: "g-1" }), twin2 = row({ trigger: "dup", grant_id: "g-2" });
  const got = R.parse({ items: [b, { ...a, epoch: "x" }, a, twin1, twin2, 42] });
  assert.deepEqual(got.rows.map(R.key), ["personal|main/developer|a", "personal|main/developer|b"]);
  assert.equal(got.skipped, 4, "읽을 수 없는 2건(epoch가 문자열인 행·숫자 42)과, 같은 키가 둘이라 어느 쪽이 현재인지 모르는 2건");
  assert.deepEqual(R.parse({ items: [] }), { rows: [], skipped: 0 });
});

test("rows are sorted by (profile, agent, trigger) whatever order the server used", () => {
  const rows = [row({ profile: "b", trigger: "x" }), row({ profile: "a", agent: "main/z", trigger: "x" }), row({ profile: "a", agent: "main/a", trigger: "y" }), row({ profile: "a", agent: "main/a", trigger: "b" })];
  assert.deepEqual(R.parse({ items: rows }).rows.map(R.key), ["a|main/a|b", "a|main/a|y", "a|main/z|x", "b|main/developer|x"]);
  assert.deepEqual(R.parse({ items: rows.slice().reverse() }).rows.map(R.key), R.parse({ items: rows }).rows.map(R.key), "입력 순서와 상관없다");
});

test("the offered action is the opposite of the current state and only that action can be armed", () => {
  const running = row(), paused = row({ paused: true });
  assert.equal(R.offered(running), "pause");
  assert.equal(R.offered(paused), "resume");
  assert.equal(R.arm(running, "resume"), null, "동작 중인 행은 재개할 수 없다");
  assert.equal(R.arm(paused, "pause"), null, "이미 정지된 행은 정지할 수 없다");
  assert.equal(R.arm(running, "delete"), null);
  assert.equal(R.arm({ ...running, epoch: "4" }, "pause"), null, "필수 필드가 틀린 행은 고정하지 않는다");
  assert.deepEqual(R.arm(running, "pause"), { key: "personal|main/developer|notify", action: "pause", profile: "personal", agent: "main/developer", trigger: "notify", grant_id: "grant-0001-aaaa", epoch: 4 });
});

test("the request goes to one path per identifier, each encoded exactly once, and carries only grant_id and the epoch that was seen", () => {
  const pin = R.arm(row(), "pause");
  assert.equal(R.path(pin), "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/pause", "agent의 «/»는 한 조각 main%2Fdeveloper다");
  assert.equal(R.path(R.arm(row({ paused: true }), "resume")), "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/resume");
  const odd = R.arm(row({ profile: "p", agent: "main/a b?c#d%e", trigger: "t&u=v" }), "pause");
  assert.equal(R.path(odd), "/v1/admin/preapprovals/p/main%2Fa%20b%3Fc%23d%25e/t%26u%3Dv/pause", "한 번만 인코딩한다(% → %25, 이중 인코딩 없음)");
  assert.deepEqual(R.payload(pin), { grant_id: "grant-0001-aaaa", epoch: 4 });
  assert.deepEqual(Object.keys(R.payload(pin)).sort(), ["epoch", "grant_id"], "서버는 모르는 키를 받지 않는다");
  assert.equal(R.payload(null), null);
  assert.throws(() => R.path({ ...pin, action: "delete" }), /알 수 없는 동작/);
});

test("the payload is what was pinned, not what the row says now", () => {
  const pin = R.arm(row({ epoch: 4 }), "pause");
  const later = row({ epoch: 9, grant_id: "grant-0002-bbbb" });
  assert.deepEqual(R.payload(pin), { grant_id: "grant-0001-aaaa", epoch: 4 });
  assert.notDeepEqual(R.payload(pin), { grant_id: later.grant_id, epoch: later.epoch });
});

test("a pending resume survives a refresh only while the same approval is at the same epoch and still paused", () => {
  const pin = R.arm(row({ paused: true, epoch: 5 }), "resume");
  assert.deepEqual(R.reconcile(pin, [row({ paused: true, epoch: 5, used_today: 20 })]), { ok: true }, "사용량이 바뀌어도 같은 확인이다");
  assert.deepEqual(R.reconcile(null, []), { ok: true });
  assert.deepEqual(R.reconcile(pin, []), { ok: false, reason: "gone" });
  assert.deepEqual(R.reconcile(pin, [row({ trigger: "other", paused: true, epoch: 5 })]), { ok: false, reason: "gone" });
  assert.deepEqual(R.reconcile(pin, [row({ paused: true, epoch: 5, grant_id: "grant-0002-bbbb" })]), { ok: false, reason: "renewed" });
  assert.deepEqual(R.reconcile(pin, [row({ paused: true, epoch: 7 })]), { ok: false, reason: "changed" }, "그 사이 상태가 두 번 바뀌어 같은 상태로 돌아왔어도 세대가 다르다");
  assert.deepEqual(R.reconcile(pin, [row({ paused: false, epoch: 6 })]), { ok: false, reason: "changed" }, "다른 곳에서 이미 재개했다");
  for (const why of ["gone", "renewed", "changed"]) assert.ok(R.WHY[why].includes("확인을 거두었"), why);
});

test("only a valid reply for the same target and approval whose state is the requested result is accepted", () => {
  const pause = R.arm(row({ epoch: 4 }), "pause"), resume = R.arm(row({ paused: true, epoch: 5 }), "resume");
  assert.deepEqual(R.settle(pause, row({ paused: true, epoch: 5 })), row({ paused: true, epoch: 5 }));
  assert.deepEqual(R.settle(resume, row({ paused: false, epoch: 6 })), row({ paused: false, epoch: 6 }));
  const rejected = {
    "요청한 상태가 아니다(정지 요청에 동작 중)": [pause, row({ paused: false, epoch: 5 })],
    "재개 요청에 정지됨": [resume, row({ paused: true, epoch: 6 })],
    "다른 트리거 행": [pause, row({ trigger: "other", paused: true })],
    "다른 승인 ID": [pause, row({ paused: true, grant_id: "grant-0002-bbbb" })],
    "필수 필드 틀림": [pause, { ...row({ paused: true }), epoch: "5" }],
    "행이 아닌 본문": [pause, { ok: true }],
    "래핑한 행": [pause, { item: row({ paused: true }) }],
    "빈 본문": [pause, null],
  };
  for (const [name, [pin, reply]] of Object.entries(rejected)) assert.equal(R.settle(pin, reply), null, name);
});

test("replacing a row keeps the others, the order and does not touch the input", () => {
  const rows = R.parse({ items: [row({ trigger: "a" }), row({ trigger: "b" })] }).rows, frozen = JSON.stringify(rows);
  const next = R.replace(rows, row({ trigger: "a", paused: true, epoch: 5 }));
  assert.deepEqual(next.map((r) => [r.trigger, r.paused]), [["a", true], ["b", false]]);
  assert.equal(JSON.stringify(rows), frozen);
  assert.deepEqual(R.replace(rows, row({ trigger: "0new" })).map((r) => r.trigger), ["0new", "a", "b"], "없는 행이면 정렬 위치에 끼운다");
});

test("usage is clamped, rounded down and flags the daily limit", () => {
  assert.deepEqual(R.usage(row({ used_today: 3, max_per_day: 24 })), { used: 3, max: 24, pct: 12, left: 21, full: false });
  assert.deepEqual(R.usage(row({ used_today: 24, max_per_day: 24 })), { used: 24, max: 24, pct: 100, left: 0, full: true });
  assert.deepEqual(R.usage(row({ used_today: 30, max_per_day: 24 })), { used: 30, max: 24, pct: 100, left: 0, full: true }, "상한을 넘은 값도 막대는 100에서 멈춘다");
  assert.equal(R.usage(row({ used_today: 1, max_per_day: 3 })).pct, 33);
  assert.equal(R.usage(row({ used_today: 0, max_per_day: 0 })).full, true);
  assert.deepEqual(R.counts([row(), row({ paused: true }), row({ paused: true })]), { total: 3, paused: 2 });
});

test("every server word of the contract has a sentence and an unknown outcome says it may or may not have been applied", () => {
  for (const word of ["stale", "denied", "not_found", "invalid", "unavailable", "network", "timeout", "mismatch"]) assert.ok(R.ERR[word] && R.ERR[word].length > 10, word);
  for (const word of ["unavailable", "network", "timeout"]) assert.ok(R.ERR[word].includes("적용됐는지 확인하지 못했습니다"), word);
  assert.ok(R.ERR.stale.includes("목록을 다시 불러왔"));
});
