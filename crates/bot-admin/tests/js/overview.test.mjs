// Integration tests for the Overview dashboard (overview.js) covering operator aggregation and non-operator isolation.
import test from "node:test";
import assert from "node:assert/strict";
import { install, load, fakeFetch, settle, textOf } from "./support/fakedom.mjs";

const { body } = install();
load("dom.js", "api.js", "labels.js", "store.js", "preapproval.js", "overview.js");
const W = globalThis.window;

async function boot({ operator, profile = "" }) {
  const calls = [], badges = [], nav = [];
  globalThis.fetch = fakeFetch((c) => {
    calls.push(c.path);
    if (c.path === "/v1/admin/me") return { status: 200, body: { principal: operator ? "local:owner" : "local:guest", operator, csrf: "csrf-1" } };
    if (c.path.startsWith("/v1/admin/runs")) {
      return { status: 200, body: { items: [{ profile: "personal", run: "r-1", agent: "main", principal: "local:owner", kind: "cli", state: "dead", error: "tool_failed", updated_ms: 1700000000000 }], next: null } };
    }
    if (c.path.startsWith("/v1/admin/approvals")) {
      return { status: 200, body: { items: [{ nonce: "ap-1", profile: "personal", approver: operator ? "local:owner" : "local:guest", tool: "agenda.delete" }], next: null } };
    }
    if (c.path === "/v1/admin/status") {
      return { status: 200, body: { at_ms: 1700000000000, build: { name: "bot", version: "0.1.0" }, process: { uptime_secs: 90, logs: { enabled: false }, issues: { enabled: false } }, profiles: { personal: { runs: { ready: 0, leased: 1, waiting: 2, retry: 0, succeeded: 5, dead: 1, cancelled: 0 }, approvals_pending: 1, holds: 2, oldest_ready_secs: null, obs: { enabled: false } } } } };
    }
    if (c.path.startsWith("/v1/admin/holds")) return { status: 200, body: { items: [{ profile: "personal", run: "r-4", call: "c-9", kind: "tool" }] } };
    if (c.path === "/v1/admin/preapprovals") {
      return { status: 200, body: { items: [{ profile: "personal", agent: "main", trigger: "digest", grant_id: "g-1", epoch: 1, paused: true, used_today: 0, max_per_day: 8 }] } };
    }
    if (c.path.startsWith("/v1/admin/map")) {
      return { status: 200, body: { rev: "rev-1", at_ms: 1700000000000, access: { personal: { revision: "rev-1", timezone: "UTC", model: "assistant", root: "main" } }, graph: { nodes: [{ id: "agent:main", profile: "personal", kind: "agent", name: "main", facts: [], links: [] }] } } };
    }
    return { status: 404, body: { error: "not_found" } };
  });
  const box = document.createElement("section");
  body.appendChild(box);
  const app = {
    me: () => ({ operator, principal: operator ? "local:owner" : "local:guest" }),
    profile: () => profile,
    go: (...args) => nav.push(args),
    stamp() {},
    noteProfiles() {},
    setBuild() {},
    setBadges: (b) => badges.push(b),
  };
  W.BotStore.reset();
  const view = W.BotOverview.create(app, box);
  await W.BotApi.me(true);
  await view.load({ profile, signal: new AbortController().signal, current: () => true });
  await settle();
  return { box, calls, badges, nav };
}

test("operator overview aggregates action counts, profiles, and updates sidebar badges", async () => {
  const t = await boot({ operator: true });
  const text = textOf(t.box);
  assert.ok(text.includes("Action required"), text);
  assert.ok(text.includes("2 unresolved holds") && text.includes("1 dead run"), text);
  assert.ok(text.includes("Active bots & profiles") && text.includes("personal"), text);
  assert.ok(text.includes("Pre-approved automations") && text.includes("digest"), text);
  assert.deepEqual(t.badges.at(-1), { decide: 3, decideTone: "bad", dead: 1, paused: 1 });
});

test("member overview never calls operator-only endpoints and shows scoped approvals and runs", async () => {
  const t = await boot({ operator: false });
  for (const forbidden of ["/v1/admin/status", "/v1/admin/holds", "/v1/admin/preapprovals", "/v1/admin/map"]) {
    assert.ok(!t.calls.some((p) => p.startsWith(forbidden)), "member must not request " + forbidden);
  }
  const text = textOf(t.box);
  assert.ok(text.includes("1 approval waiting for you"), text);
  assert.ok(text.includes("Assigned approvals") && text.includes("Recent activity"), text);
  assert.ok(!text.includes("Active bots & profiles"), "operator profile catalog stays hidden from members");
  assert.deepEqual(t.badges.at(-1), { decide: 1, decideTone: "warn", dead: 1 });
});
