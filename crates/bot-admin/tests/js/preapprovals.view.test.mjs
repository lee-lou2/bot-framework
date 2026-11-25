// 사전 승인 탭을 «실제 화면 코드»(dom.js·api.js·preapproval.js·preapprovals.js)로 돌린다: 작은 DOM 모조품 위에서 단추를 누르고, 가짜 서버(계약 규칙)가 답한다.
// 확인하는 것: 누른 순간 화면이 본 승인 ID·세대만 보낸다 · 응답 전에는 상태가 안 바뀐다 · 한 행에 요청은 하나 · 재개만 두 번 · 409·응답 유실·시간 초과 뒤 목록을 다시 읽는다.
import test, { mock } from "node:test";
import assert from "node:assert/strict";
import { install, load, fakeFetch, settle, deferred, button, cardFor, cardsOf, textOf, all } from "./support/fakedom.mjs";
import { contract, row } from "./support/contract.mjs";

const { body } = install();
load("dom.js", "api.js", "preapproval.js", "preapprovals.js");
const W = globalThis.window;

// 가짜 서버를 화면에 붙인다. 시험이 server.gate(응답 보류)·server.fail("before"=요청이 닿기 전 실패, "after"=처리한 뒤 응답 유실)·server.getGate(목록 응답 보류)를 둘 수 있다.
async function boot(server, { profile = "" } = {}) {
  let current = profile, view;
  const fetch = fakeFetch(async (c) => {
    if (c.method === "GET" && c.path === "/v1/admin/preapprovals" && server.getGate) await server.getGate.promise;
    if (c.method === "POST" && server.gate) await server.gate.promise;
    if (c.method === "POST" && server.fail === "before") throw new Error("서버에 닿지 못함");
    const res = server.handler(c);
    if (c.method === "POST" && server.fail === "after") throw new Error("응답을 잃음");
    return res;
  });
  globalThis.fetch = fetch;
  const toasts = [], notices = [], box = document.createElement("section");
  body.appendChild(box);
  const app = {
    me: () => ({ operator: true, principal: "local:owner" }), profile: () => current, go() {}, stamp() {}, noteProfiles() {},
    notice: (m) => notices.push(m), toast: (m, tone) => toasts.push({ m, tone }),
    refresh: () => view.load({ profile: current, signal: new AbortController().signal, current: () => true }).catch((e) => { notices.push(String(e.message || e)); }),
  };
  view = W.BotPreapprovals.create(app, box);
  await W.BotApi.me(true);
  await app.refresh();
  await settle();
  return { box, view, app, toasts, notices, fetch, server, setProfile: (p) => { current = p; } };
}
const posts = (t) => t.fetch.calls.filter((c) => c.method === "POST");
const lists = (t) => t.fetch.calls.filter((c) => c.method === "GET" && c.path === "/v1/admin/preapprovals");
const card = (t, name = "notify") => cardFor(t.box, name);
const said = (t, name = "notify") => textOf(card(t, name));

test("each pre-approved trigger shows its state, today's usage and the one button that fits", async () => {
  const t = await boot(contract([row(), row({ trigger: "weekly", paused: true, used_today: 24, max_per_day: 24, epoch: 7 })]));
  assert.equal(cardsOf(t.box).length, 2);
  assert.ok(said(t).includes("동작 중") && said(t).includes("오늘 3 / 24회") && said(t).includes("승인 ID grant-0001-a"), said(t));
  assert.ok(button(card(t), "긴급 정지") && !button(card(t), "재개"));
  assert.ok(said(t, "weekly").includes("정지됨") && said(t, "weekly").includes("오늘 상한 도달"), said(t, "weekly"));
  assert.ok(button(card(t, "weekly"), "재개") && !button(card(t, "weekly"), "긴급 정지"));
  assert.equal(all(card(t), (x) => x.localName === "i")[0].style.props["--pct"], "12", "막대는 사용 비율이다");
  assert.ok(textOf(t.box).includes("사전 승인 2개 · 정지 1개"));
});

test("pause sends exactly the approval id and epoch the screen saw, and the card keeps its state until the server answers", async () => {
  const s = contract(), t = await boot(s);
  s.gate = deferred();
  button(card(t), "긴급 정지").click();
  await settle();
  assert.equal(posts(t).length, 1);
  assert.equal(posts(t)[0].path, "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/pause");
  assert.deepEqual(posts(t)[0].body, { grant_id: "grant-0001-aaaa", epoch: 4 });
  assert.equal(posts(t)[0].headers["X-Bot-Csrf"], "csrf-1");
  assert.ok(said(t).includes("동작 중") && !said(t).includes("정지됨"), "응답 전에는 상태를 바꾸지 않는다");
  assert.ok(button(card(t), "정지하는 중…").disabled, "응답을 기다리는 동안 단추는 꺼진다");
  s.gate.resolve();
  await settle();
  assert.ok(said(t).includes("정지됨") && said(t).includes("정지했습니다 · notify") && said(t).includes("정지 세대 5"), said(t));
  assert.deepEqual(t.toasts.at(-1), { m: "정지했습니다 · notify", tone: "ok" });
  assert.equal(lists(t).length, 2, "성공한 뒤에도 목록을 다시 읽어 서버의 현재 상태를 보인다");
  assert.equal(s.audits, 1);
});

test("a second click while the first request is in flight sends nothing, even on a button that was drawn before", async () => {
  const s = contract(), t = await boot(s), old = button(card(t), "긴급 정지");
  s.gate = deferred();
  old.click();
  old.click();
  old.click();
  await settle();
  button(card(t), "정지하는 중…").click();
  assert.equal(posts(t).length, 1, "요청은 하나뿐이다");
  s.gate.resolve();
  await settle();
  assert.equal(posts(t).length, 1);
  assert.equal(s.audits, 1);
});

test("a stale answer (409) shows the stale sentence and re-reads the list: the card ends up in the server's state", async () => {
  const s = contract(), t = await boot(s);
  Object.assign(s.rows[0], { epoch: 6, paused: true }); // 다른 운영자가 그 사이 바꿨다
  button(card(t), "긴급 정지").click();
  await settle();
  assert.equal(posts(t).length, 1);
  assert.ok(said(t).includes("다른 곳에서 먼저 바뀌었거나"), said(t));
  assert.equal(t.toasts.at(-1).tone, "bad");
  assert.equal(lists(t).length, 2);
  assert.ok(said(t).includes("정지됨") && button(card(t), "재개"), "서버의 현재 상태(정지됨)를 보인다");
  assert.equal(s.audits, 0);
});

test("when the request never reaches the server the screen says it does not know, then shows what the server really has", async () => {
  const s = contract(), t = await boot(s);
  s.fail = "before";
  button(card(t), "긴급 정지").click();
  await settle();
  assert.ok(said(t).includes("적용됐는지 확인하지 못했습니다"), said(t));
  assert.ok(said(t).includes("동작 중") && button(card(t), "긴급 정지") && !button(card(t), "긴급 정지").disabled, "상태는 그대로이고 다시 누를 수 있다");
  assert.equal(lists(t).length, 2);
  s.fail = null;
  button(card(t), "긴급 정지").click();
  await settle();
  assert.ok(said(t).includes("정지됨"));
});

test("a lost reply: the server applied the pause but the answer was lost; the re-read list shows it and a resend would be the same command", async () => {
  const s = contract(), t = await boot(s);
  s.fail = "after";
  button(card(t), "긴급 정지").click();
  await settle();
  assert.equal(s.audits, 1);
  assert.ok(said(t).includes("적용됐는지 확인하지 못했습니다"), "응답을 못 받았으니 화면은 모른다고 말한다");
  assert.ok(said(t).includes("정지됨"), "목록을 다시 읽어 서버의 사실을 보인다: " + said(t));
  // 같은 명령(grant_id·옛 epoch 4)을 다시 보내도 서버는 중복 없이 같은 결과다.
  s.fail = null;
  const again = s.handler({ method: "POST", path: "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/pause", headers: { "X-Bot-Csrf": "csrf-1" }, body: { grant_id: "grant-0001-aaaa", epoch: 4 } });
  assert.equal(again.status, 200);
  assert.equal(s.audits, 1);
});

test("no answer within the time limit: the screen gives up waiting, says it does not know and re-reads the list", async () => {
  mock.timers.enable({ apis: ["setTimeout", "Date"] });
  try {
    const s = contract(), t = await boot(s);
    s.gate = deferred();
    button(card(t), "긴급 정지").click();
    await settle();
    assert.ok(button(card(t), "정지하는 중…"));
    mock.timers.tick(15000);
    await settle();
    assert.ok(said(t).includes("응답이 오지 않아 적용됐는지 확인하지 못했습니다"), said(t));
    assert.ok(!button(card(t), "정지하는 중…") && !button(card(t), "긴급 정지").disabled, "더는 기다리지 않는다");
    assert.equal(lists(t).length, 2);
    assert.equal(t.view.busy(), false);
  } finally { mock.timers.reset(); }
});

test("a permission error and a reply that does not match the request leave the state alone", async () => {
  const s = contract(), t = await boot(s);
  s.deny = true;
  button(card(t), "긴급 정지").click();
  await settle();
  assert.ok(said(t).includes("운영자만 정지·재개할 수 있습니다") && said(t).includes("동작 중"), said(t));
  s.deny = false;
  const real = s.handler;
  s.handler = (c) => (c.method === "POST" ? { status: 200, body: { ...s.rows[0], paused: false } } : real(c)); // 정지 요청에 «동작 중» 행을 돌려주는 잘못된 서버
  button(card(t), "긴급 정지").click();
  await settle();
  assert.ok(said(t).includes("서버 응답이 요청과 맞지 않아"), said(t));
  assert.ok(said(t).includes("동작 중") && !said(t).includes("정지됨"));
});

test("resume needs a second press, ignores a press right after the first, and sends the values pinned by the first press", async () => {
  mock.timers.enable({ apis: ["setTimeout", "Date"] });
  try {
    const s = contract([row({ paused: true, epoch: 5 })]), t = await boot(s);
    button(card(t), "재개").click();
    await settle();
    assert.equal(posts(t).length, 0, "첫 누름은 요청을 보내지 않는다");
    assert.ok(said(t).includes("한 번 더 눌러 재개") && said(t).includes("사람의 카드 없이 외부 쓰기를 다시 실행합니다") && said(t).includes("오늘 남은 허가 21회"), said(t));
    assert.ok(button(card(t), "취소"));
    button(card(t), "한 번 더 눌러 재개").click(); // 더블클릭·키 반복
    await settle();
    assert.equal(posts(t).length, 0, "첫 누름 직후의 두 번째 누름은 무시한다");
    mock.timers.tick(700);
    button(card(t), "한 번 더 눌러 재개").click();
    await settle();
    assert.equal(posts(t).length, 1);
    assert.equal(posts(t)[0].path, "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/resume");
    assert.deepEqual(posts(t)[0].body, { grant_id: "grant-0001-aaaa", epoch: 5 });
    assert.ok(said(t).includes("동작 중") && said(t).includes("재개했습니다 · notify") && button(card(t), "긴급 정지"), said(t));
  } finally { mock.timers.reset(); }
});

test("an unconfirmed resume lapses after twelve seconds, on Escape, on cancel and when the tab is left", async () => {
  mock.timers.enable({ apis: ["setTimeout", "Date"] });
  try {
    const t = await boot(contract([row({ paused: true, epoch: 5 })]));
    const press = () => button(card(t), "재개").click();
    const idle = () => assert.ok(button(card(t), "재개") && !button(card(t), "한 번 더 눌러 재개"), said(t));
    press(); await settle(); assert.equal(t.view.busy(), true);
    mock.timers.tick(12000); await settle(); idle();
    press(); await settle(); button(card(t), "한 번 더 눌러 재개").key("Escape"); await settle(); idle();
    press(); await settle(); button(card(t), "취소").click(); await settle(); idle();
    press(); await settle(); t.view.hide(); idle();
    assert.equal(posts(t).length, 0);
    assert.equal(t.view.busy(), false);
    button(card(t), "재개").key("Escape"); // 확인 중이 아니면 Esc는 아무 일도 아니다
    assert.equal(posts(t).length, 0);
  } finally { mock.timers.reset(); }
});

test("a refresh that finds the approval changed under a pending resume lifts the confirmation, and the next resume uses the new values", async () => {
  mock.timers.enable({ apis: ["setTimeout", "Date"] });
  try {
    const s = contract([row({ paused: true, epoch: 5 })]), t = await boot(s);
    const second = async () => { mock.timers.tick(700); button(card(t), "한 번 더 눌러 재개").click(); await settle(); };
    button(card(t), "재개").click(); await settle();
    s.rows[0].epoch = 7; // 다른 운영자가 재개했다가 다시 정지했다(같은 «정지됨»이지만 세대가 다르다)
    await t.app.refresh(); await settle();
    assert.ok(said(t).includes("확인을 거두었으니") && said(t).includes("다른 곳에서 상태가 바뀌었습니다") && !said(t).includes("한 번 더 눌러 재개"), said(t));
    assert.equal(posts(t).length, 0);
    button(card(t), "재개").click(); await settle(); await second();
    assert.equal(posts(t).length, 1);
    assert.deepEqual(posts(t)[0].body, { grant_id: "grant-0001-aaaa", epoch: 7 }, "옛 세대(5)는 한 번도 보내지 않는다");
    // 승인이 새로 발급되면(활성화가 바뀜) 확인을 거둔다
    s.rows[0].paused = true; s.rows[0].grant_id = "grant-0002-bbbb";
    await t.app.refresh(); await settle();
    button(card(t), "재개").click(); await settle();
    s.rows[0].grant_id = "grant-0003-cccc";
    await t.app.refresh(); await settle();
    assert.ok(said(t).includes("사전 승인이 새로 발급됐습니다"), said(t));
    // 선언이 사라지면 확인과 카드가 함께 사라진다
    s.rows[0].grant_id = "grant-0003-cccc"; s.rows[0].paused = true;
    button(card(t), "재개").click(); await settle();
    s.rows.length = 0;
    await t.app.refresh(); await settle();
    assert.equal(cardsOf(t.box).length, 0);
    assert.ok(textOf(t.box).includes("사전 승인한 트리거가 없습니다"));
    assert.equal(t.view.busy(), false, "사라진 행의 확인은 남지 않는다");
  } finally { mock.timers.reset(); }
});

test("the profile filter is a screen filter: the list request has no query and rows of other profiles are never shown", async () => {
  const s = contract([row(), row({ profile: "other", trigger: "daily" })]);
  const t = await boot(s);
  assert.equal(lists(t)[0].path, "/v1/admin/preapprovals", "쿼리가 없다");
  assert.ok(said(t).includes("프로필 personal") && said(t, "daily").includes("프로필 other"), "전체 보기에서는 프로필을 함께 보인다");
  t.setProfile("other");
  t.view.reset();
  await t.app.refresh(); await settle();
  assert.deepEqual(cardsOf(t.box).map((c) => textOf(c.childNodes[0])).map((x) => /daily|notify/.exec(x)[0]), ["daily"]);
  assert.ok(!textOf(t.box).includes("프로필 other"), "한 프로필만 고르면 meta에 프로필을 반복하지 않는다");
  assert.ok(lists(t).length >= 2 && lists(t).every((c) => c.path === "/v1/admin/preapprovals"), "프로필을 골라도 목록 요청에는 쿼리가 없다(서버는 모르는 쿼리를 400으로 거절한다)");
  t.setProfile("nothing");
  t.view.reset();
  await t.app.refresh(); await settle();
  assert.ok(textOf(t.box).includes("이 프로필에는 사전 승인한 트리거가 없습니다"));
});

test("a row with a wrong field gets no button, the rest keep working and the screen says how many were skipped", async () => {
  const s = contract([row(), row({ trigger: "broken" })]);
  s.rows[1].epoch = "4";
  const t = await boot(s);
  assert.equal(cardsOf(t.box).length, 1);
  assert.ok(button(card(t), "긴급 정지"));
  assert.ok(textOf(t.box).includes("읽을 수 없는 행 1개는 버튼 없이 건너뛰었습니다"));
});

test("a list with the wrong shape or a failed read keeps what was shown and says the state may be old", async () => {
  const s = contract(), t = await boot(s), good = s.handler;
  s.handler = (c) => (c.method === "GET" && c.path === "/v1/admin/preapprovals" ? { status: 200, body: { rows: [] } } : good(c));
  await t.app.refresh(); await settle();
  assert.equal(cardsOf(t.box).length, 1, "이전 행을 지우지 않는다");
  assert.ok(textOf(t.box).includes("목록 응답의 모양이 올바르지 않아"));
  s.handler = (c) => (c.method === "GET" && c.path === "/v1/admin/preapprovals" ? { status: 503, body: { error: "unavailable" } } : good(c));
  await t.app.refresh(); await settle();
  assert.equal(cardsOf(t.box).length, 1);
  assert.ok(textOf(t.box).includes("목록을 새로 읽지 못했습니다") && t.notices.length >= 1);
  s.handler = good;
  await t.app.refresh(); await settle();
  assert.ok(!textOf(t.box).includes("목록을 새로 읽지 못했습니다"), "다시 읽으면 경고가 사라진다");
});

test("a list answer that arrives after the profile was switched is ignored", async () => {
  const s = contract(), t = await boot(s);
  s.getGate = deferred();
  const late = t.view.load({ profile: "", signal: new AbortController().signal, current: () => true });
  t.view.reset();
  s.getGate.resolve();
  await late;
  await settle();
  assert.equal(cardsOf(t.box).length, 0, "옛 선택으로 보낸 요청의 응답은 새 선택 밑에 그리지 않는다");
});

test("a rotated CSRF token is refreshed once and the pause is applied exactly once", async () => {
  const s = contract(), t = await boot(s);
  s.csrf = "csrf-2"; // 서버가 토큰을 바꿨다: 화면이 가진 csrf-1은 이제 틀렸다
  button(card(t), "긴급 정지").click();
  await settle();
  assert.deepEqual(posts(t).map((c) => c.headers["X-Bot-Csrf"]), ["csrf-1", "csrf-2"]);
  assert.equal(s.audits, 1, "첫 요청은 CSRF에서 막혀 실행되지 않았다");
  assert.ok(said(t).includes("정지됨"));
});

test("the tab exists only for operators: it is declared operator-only, built inside the operator block and loaded after its rules", async () => {
  const fs = await import("node:fs"), here = new URL("../../ui/", import.meta.url);
  const app = fs.readFileSync(new URL("app.js", here), "utf8"), html = fs.readFileSync(new URL("index.html", here), "utf8");
  assert.match(app, /\{ id: "preapprovals", label: "사전 승인", op: true \}/);
  const block = /if \(op\) \{([\s\S]*?)\n    \}/.exec(app.slice(app.indexOf("function buildViews")));
  assert.ok(block && block[1].includes("views.preapprovals = window.BotPreapprovals.create(app, $(\"view-preapprovals\"))"), "운영자 블록 안에서만 만든다");
  assert.ok(!/views\.preapprovals/.test(app.replace(block[0], "")), "운영자 블록 밖에서는 만들지 않는다");
  assert.ok(html.includes('id="view-preapprovals"'));
  assert.ok(html.indexOf("/ui/preapproval.js") > 0 && html.indexOf("/ui/preapproval.js") < html.indexOf("/ui/preapprovals.js"), "규칙이 화면보다 먼저 불린다");
});

test("the audit tab names the pre-approval pause and resume actions", () => {
  load("labels.js");
  const names = W.BotLabels.AUDIT_ACTION;
  assert.equal(names["preapproval.pause"], "사전 승인 정지");
  assert.equal(names["preapproval.resume"], "사전 승인 재개");
  assert.equal(names["approval.decide"], "승인 판정", "기존 이름은 그대로다");
});
