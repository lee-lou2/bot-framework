// Integration tests for the Pre-approvals view (dom.js, api.js, preapproval.js, preapprovals.js) on fakedom + contract server.
import test, { mock } from "node:test";
import assert from "node:assert/strict";
import { install, load, fakeFetch, settle, deferred, button, cardFor, cardsOf, textOf, all } from "./support/fakedom.mjs";
import { contract, row } from "./support/contract.mjs";

const { body } = install();
load("dom.js", "api.js", "preapproval.js", "preapprovals.js");
const W = globalThis.window;

// Hooks server.gate (hold POST), server.fail ("before"|"after"), and server.getGate (hold GET) for deterministic failure testing.
async function boot(server, { profile = "" } = {}) {
  let current = profile, view;
  const fetch = fakeFetch(async (c) => {
    if (c.method === "GET" && c.path === "/v1/admin/preapprovals" && server.getGate) await server.getGate.promise;
    if (c.method === "POST" && server.gate) await server.gate.promise;
    if (c.method === "POST" && server.fail === "before") throw new Error("server unreachable");
    const res = server.handler(c);
    if (c.method === "POST" && server.fail === "after") throw new Error("response lost");
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
  assert.ok(said(t).includes("Active") && said(t).includes("Today 3 / 24") && said(t).includes("Grant ID grant-0001-a"), said(t));
  assert.ok(button(card(t), "Emergency pause") && !button(card(t), "Resume"));
  assert.ok(said(t, "weekly").includes("Paused") && said(t, "weekly").includes("Daily limit reached"), said(t, "weekly"));
  assert.ok(button(card(t, "weekly"), "Resume") && !button(card(t, "weekly"), "Emergency pause"));
  assert.equal(all(card(t), (x) => x.localName === "i")[0].style.props["--pct"], "12", "meter reflects usage percentage");
  assert.ok(textOf(t.box).includes("2 pre-approved · 1 paused"));
});

test("pause sends exactly the approval id and epoch the screen saw, and the card keeps its state until the server answers", async () => {
  const s = contract(), t = await boot(s);
  s.gate = deferred();
  button(card(t), "Emergency pause").click();
  await settle();
  assert.equal(posts(t).length, 1);
  assert.equal(posts(t)[0].path, "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/pause");
  assert.deepEqual(posts(t)[0].body, { grant_id: "grant-0001-aaaa", epoch: 4 });
  assert.equal(posts(t)[0].headers["X-Bot-Csrf"], "csrf-1");
  assert.ok(said(t).includes("Active") && !said(t).includes("Paused"), "state must not change before the server replies");
  assert.ok(button(card(t), "Pausing…").disabled, "button stays disabled while in flight");
  s.gate.resolve();
  await settle();
  assert.ok(said(t).includes("Paused") && said(t).includes("Paused · notify") && said(t).includes("Pause epoch 5"), said(t));
  assert.deepEqual(t.toasts.at(-1), { m: "Paused · notify", tone: "ok" });
  assert.equal(lists(t).length, 2, "reloads the list after success to reflect server state");
  assert.equal(s.audits, 1);
});

test("a second click while the first request is in flight sends nothing, even on a button that was drawn before", async () => {
  const s = contract(), t = await boot(s), old = button(card(t), "Emergency pause");
  s.gate = deferred();
  old.click();
  old.click();
  old.click();
  await settle();
  button(card(t), "Pausing…").click();
  assert.equal(posts(t).length, 1, "only one request is sent");
  s.gate.resolve();
  await settle();
  assert.equal(posts(t).length, 1);
  assert.equal(s.audits, 1);
});

test("a stale answer (409) shows the stale sentence and re-reads the list: the card ends up in the server's state", async () => {
  const s = contract(), t = await boot(s);
  Object.assign(s.rows[0], { epoch: 6, paused: true });
  button(card(t), "Emergency pause").click();
  await settle();
  assert.equal(posts(t).length, 1);
  assert.ok(said(t).includes("Changed elsewhere or replaced"), said(t));
  assert.equal(t.toasts.at(-1).tone, "bad");
  assert.equal(lists(t).length, 2);
  assert.ok(said(t).includes("Paused") && button(card(t), "Resume"), "shows the server's current state");
  assert.equal(s.audits, 0);
});

test("when the request never reaches the server the screen says it does not know, then shows what the server really has", async () => {
  const s = contract(), t = await boot(s);
  s.fail = "before";
  button(card(t), "Emergency pause").click();
  await settle();
  assert.ok(said(t).includes("could not confirm whether the change was applied"), said(t));
  assert.ok(said(t).includes("Active") && button(card(t), "Emergency pause") && !button(card(t), "Emergency pause").disabled, "state stays unchanged and retryable");
  assert.equal(lists(t).length, 2);
  s.fail = null;
  button(card(t), "Emergency pause").click();
  await settle();
  assert.ok(said(t).includes("Paused"));
});

test("a lost reply: the server applied the pause but the answer was lost; the re-read list shows it and a resend would be the same command", async () => {
  const s = contract(), t = await boot(s);
  s.fail = "after";
  button(card(t), "Emergency pause").click();
  await settle();
  assert.equal(s.audits, 1);
  assert.ok(said(t).includes("could not confirm whether the change was applied"), "reports uncertain outcome when reply is lost");
  assert.ok(said(t).includes("Paused"), "list reload shows the committed server state: " + said(t));
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
    button(card(t), "Emergency pause").click();
    await settle();
    assert.ok(button(card(t), "Pausing…"));
    mock.timers.tick(15000);
    await settle();
    assert.ok(said(t).includes("No response received in time, so we could not confirm whether the change was applied"), said(t));
    assert.ok(!button(card(t), "Pausing…") && !button(card(t), "Emergency pause").disabled, "no longer stuck waiting");
    assert.equal(lists(t).length, 2);
    assert.equal(t.view.busy(), false);
  } finally { mock.timers.reset(); }
});

test("a permission error and a reply that does not match the request leave the state alone", async () => {
  const s = contract(), t = await boot(s);
  s.deny = true;
  button(card(t), "Emergency pause").click();
  await settle();
  assert.ok(said(t).includes("Only operators can pause or resume pre-approvals") && said(t).includes("Active"), said(t));
  s.deny = false;
  const real = s.handler;
  s.handler = (c) => (c.method === "POST" ? { status: 200, body: { ...s.rows[0], paused: false } } : real(c));
  button(card(t), "Emergency pause").click();
  await settle();
  assert.ok(said(t).includes("The server response did not match the request"), said(t));
  assert.ok(said(t).includes("Active") && !said(t).includes("Paused"));
});

test("resume needs a second press, ignores a press right after the first, and sends the values pinned by the first press", async () => {
  mock.timers.enable({ apis: ["setTimeout", "Date"] });
  try {
    const s = contract([row({ paused: true, epoch: 5 })]), t = await boot(s);
    button(card(t), "Resume").click();
    await settle();
    assert.equal(posts(t).length, 0, "first press only arms confirmation");
    assert.ok(said(t).includes("Press again to resume") && said(t).includes("re-enable external writes without human approval cards") && said(t).includes("21 remaining today"), said(t));
    assert.ok(button(card(t), "Cancel"));
    button(card(t), "Press again to resume").click();
    await settle();
    assert.equal(posts(t).length, 0, "double-click immediately after arming is ignored");
    mock.timers.tick(700);
    button(card(t), "Press again to resume").click();
    await settle();
    assert.equal(posts(t).length, 1);
    assert.equal(posts(t)[0].path, "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/resume");
    assert.deepEqual(posts(t)[0].body, { grant_id: "grant-0001-aaaa", epoch: 5 });
    assert.ok(said(t).includes("Active") && said(t).includes("Resumed · notify") && button(card(t), "Emergency pause"), said(t));
  } finally { mock.timers.reset(); }
});

test("an unconfirmed resume lapses after twelve seconds, on Escape, on cancel and when the tab is left", async () => {
  mock.timers.enable({ apis: ["setTimeout", "Date"] });
  try {
    const t = await boot(contract([row({ paused: true, epoch: 5 })]));
    const press = () => button(card(t), "Resume").click();
    const idle = () => assert.ok(button(card(t), "Resume") && !button(card(t), "Press again to resume"), said(t));
    press(); await settle(); assert.equal(t.view.busy(), true);
    mock.timers.tick(12000); await settle(); idle();
    press(); await settle(); button(card(t), "Press again to resume").key("Escape"); await settle(); idle();
    press(); await settle(); button(card(t), "Cancel").click(); await settle(); idle();
    press(); await settle(); t.view.hide(); idle();
    assert.equal(posts(t).length, 0);
    assert.equal(t.view.busy(), false);
    button(card(t), "Resume").key("Escape");
    assert.equal(posts(t).length, 0);
  } finally { mock.timers.reset(); }
});

test("a refresh that finds the approval changed under a pending resume lifts the confirmation, and the next resume uses the new values", async () => {
  mock.timers.enable({ apis: ["setTimeout", "Date"] });
  try {
    const s = contract([row({ paused: true, epoch: 5 })]), t = await boot(s);
    const second = async () => { mock.timers.tick(700); button(card(t), "Press again to resume").click(); await settle(); };
    button(card(t), "Resume").click(); await settle();
    s.rows[0].epoch = 7;
    await t.app.refresh(); await settle();
    assert.ok(said(t).includes("Confirmation cleared") && said(t).includes("State changed elsewhere") && !said(t).includes("Press again to resume"), said(t));
    assert.equal(posts(t).length, 0);
    button(card(t), "Resume").click(); await settle(); await second();
    assert.equal(posts(t).length, 1);
    assert.deepEqual(posts(t)[0].body, { grant_id: "grant-0001-aaaa", epoch: 7 }, "stale epoch 5 is never sent");
    s.rows[0].paused = true; s.rows[0].grant_id = "grant-0002-bbbb";
    await t.app.refresh(); await settle();
    button(card(t), "Resume").click(); await settle();
    s.rows[0].grant_id = "grant-0003-cccc";
    await t.app.refresh(); await settle();
    assert.ok(said(t).includes("A new pre-approval was issued"), said(t));
    s.rows[0].grant_id = "grant-0003-cccc"; s.rows[0].paused = true;
    button(card(t), "Resume").click(); await settle();
    s.rows.length = 0;
    await t.app.refresh(); await settle();
    assert.equal(cardsOf(t.box).length, 0);
    assert.ok(textOf(t.box).includes("No pre-approved triggers"));
    assert.equal(t.view.busy(), false, "confirmation for removed row is cleared");
  } finally { mock.timers.reset(); }
});

test("the profile filter is a screen filter: the list request has no query and rows of other profiles are never shown", async () => {
  const s = contract([row(), row({ profile: "other", trigger: "daily" })]);
  const t = await boot(s);
  assert.equal(lists(t)[0].path, "/v1/admin/preapprovals", "list request carries no query");
  assert.ok(said(t).includes("Profile personal") && said(t, "daily").includes("Profile other"), "all-profiles view shows profile label");
  t.setProfile("other");
  t.view.reset();
  await t.app.refresh(); await settle();
  assert.deepEqual(cardsOf(t.box).map((c) => textOf(c.childNodes[0])).map((x) => /daily|notify/.exec(x)[0]), ["daily"]);
  assert.ok(!textOf(t.box).includes("Profile other"), "single-profile filter omits redundant profile label");
  assert.ok(lists(t).length >= 2 && lists(t).every((c) => c.path === "/v1/admin/preapprovals"), "profile filter never appends query params rejected by server");
  t.setProfile("nothing");
  t.view.reset();
  await t.app.refresh(); await settle();
  assert.ok(textOf(t.box).includes("No pre-approved triggers in this profile"));
});

test("a row with a wrong field gets no button, the rest keep working and the screen says how many were skipped", async () => {
  const s = contract([row(), row({ trigger: "broken" })]);
  s.rows[1].epoch = "4";
  const t = await boot(s);
  assert.equal(cardsOf(t.box).length, 1);
  assert.ok(button(card(t), "Emergency pause"));
  assert.ok(textOf(t.box).includes("Skipped 1 unreadable row(s) without action buttons"));
});

test("a list with the wrong shape or a failed read keeps what was shown and says the state may be old", async () => {
  const s = contract(), t = await boot(s), good = s.handler;
  s.handler = (c) => (c.method === "GET" && c.path === "/v1/admin/preapprovals" ? { status: 200, body: { rows: [] } } : good(c));
  await t.app.refresh(); await settle();
  assert.equal(cardsOf(t.box).length, 1, "keeps previously loaded rows");
  assert.ok(textOf(t.box).includes("Skipped list response due to unexpected format"));
  s.handler = (c) => (c.method === "GET" && c.path === "/v1/admin/preapprovals" ? { status: 503, body: { error: "unavailable" } } : good(c));
  await t.app.refresh(); await settle();
  assert.equal(cardsOf(t.box).length, 1);
  assert.ok(textOf(t.box).includes("Failed to reload list") && t.notices.length >= 1);
  s.handler = good;
  await t.app.refresh(); await settle();
  assert.ok(!textOf(t.box).includes("Failed to reload list"), "warning clears after successful reload");
});

test("a list answer that arrives after the profile was switched is ignored", async () => {
  const s = contract(), t = await boot(s);
  s.getGate = deferred();
  const late = t.view.load({ profile: "", signal: new AbortController().signal, current: () => true });
  t.view.reset();
  s.getGate.resolve();
  await late;
  await settle();
  assert.equal(cardsOf(t.box).length, 0, "stale response from prior profile selection is ignored");
});

test("a rotated CSRF token is refreshed once and the pause is applied exactly once", async () => {
  const s = contract(), t = await boot(s);
  s.csrf = "csrf-2";
  button(card(t), "Emergency pause").click();
  await settle();
  assert.deepEqual(posts(t).map((c) => c.headers["X-Bot-Csrf"]), ["csrf-1", "csrf-2"]);
  assert.equal(s.audits, 1, "first attempt was blocked by CSRF check before execution");
  assert.ok(said(t).includes("Paused"));
});

test("the tab exists only for operators: it is declared operator-only, built inside the operator block and loaded after its rules", async () => {
  const fs = await import("node:fs"), here = new URL("../../ui/", import.meta.url);
  const app = fs.readFileSync(new URL("app.js", here), "utf8"), html = fs.readFileSync(new URL("index.html", here), "utf8");
  assert.match(app, /\{ id: "preapprovals", label: "Pre-approvals", op: true \}/);
  const block = /if \(op\) \{([\s\S]*?)\n    \}/.exec(app.slice(app.indexOf("function buildViews")));
  assert.ok(block && block[1].includes("views.preapprovals = window.BotPreapprovals.create(app, $(\"view-preapprovals\"))"), "must be constructed inside the operator block");
  assert.ok(!/views\.preapprovals/.test(app.replace(block[0], "")), "must not be constructed outside the operator block");
  assert.ok(html.includes('id="view-preapprovals"'));
  assert.ok(html.indexOf("/ui/preapproval.js") > 0 && html.indexOf("/ui/preapproval.js") < html.indexOf("/ui/preapprovals.js"), "rules script must load before view script");
});

test("the audit tab names the pre-approval pause and resume actions", () => {
  load("labels.js");
  const names = W.BotLabels.AUDIT_ACTION;
  assert.equal(names["preapproval.pause"], "Pause pre-approval");
  assert.equal(names["preapproval.resume"], "Resume pre-approval");
  assert.equal(names["approval.decide"], "Approval decision", "existing action labels remain intact");
});
