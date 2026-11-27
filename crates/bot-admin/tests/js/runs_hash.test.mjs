// URL hash synchronization tests for app.js + runs.js (#runs/{profile}/{run}) on fakedom.
import test from "node:test";
import assert from "node:assert/strict";
import { install, load, fakeFetch, settle, all, textOf } from "./support/fakedom.mjs";

const { document, body } = install();

const mk = (tag, id) => {
  const el = document.createElement(tag);
  if (id) el.setAttribute("id", id);
  return el;
};
const profileSel = mk("select", "profile");
profileSel.appendChild(Object.assign(mk("option"), { value: "", textContent: "All" }));
Object.defineProperty(profileSel, "options", { get() { return this.children; } });
profileSel.remove = function (i) { if (this.children[i]) this.removeChild(this.children[i]); };
profileSel.value = "";

const tabs = mk("nav", "tabs");
tabs.appendChild(mk("span", "tab-ink"));
const auto = mk("input", "auto");
auto.checked = true;
const main = mk("main", "main");
for (const id of ["boot", "view-structure", "view-runs", "view-decide", "view-preapprovals", "view-access", "view-audit", "view-status"]) {
  main.appendChild(mk("section", id));
}
for (const el of [mk("span", "mark"), mk("span", "sel-ic"), mk("span", "search-ic"), mk("span", "fit-ic"), mk("button", "zin"), mk("button", "zout"), tabs, mk("span", "who"), profileSel, auto, mk("span", "stamp"), mk("div", "notice"), main, mk("div", "toast")]) {
  body.appendChild(el);
}

document.addEventListener = () => {};
document.querySelectorAll = (sel) => (sel === "#tabs [role=tab]" ? all(tabs, (x) => x.getAttribute("role") === "tab") : []);

const winListeners = {};
globalThis.addEventListener = (type, fn) => { (winListeners[type] = winListeners[type] || []).push(fn); };
globalThis.location = { hash: "#runs" };
globalThis.history = { replaceState(_s, _t, url) { globalThis.location.hash = String(url); } };
globalThis.setInterval = () => 1;
globalThis.clearInterval = () => {};

function fireHash(next) {
  globalThis.location.hash = next;
  (winListeners.hashchange || []).slice().forEach((fn) => fn({ type: "hashchange" }));
}

const sampleRun = {
  profile: "personal", run: "r-1", root: "r-1", parent: null, step: null, agent: "main",
  principal: "local:owner", kind: "cli", trigger: null, state: "succeeded", generation: 1,
  attempts: 1, waiting: null, private_memory_read: false, error: null,
  created_ms: 1700000000000, updated_ms: 1700000001000, trace_id: "0123456789abcdef",
};
const sampleDetail = { run: sampleRun, control: null, lineage: [sampleRun], children: [], calls: [], effects: [] };

globalThis.fetch = fakeFetch((c) => {
  if (c.path === "/v1/admin/me") return { status: 200, body: { principal: "local:owner", operator: true, csrf: "csrf-1" } };
  if (c.path === "/v1/admin/status") return { status: 200, body: { profiles: { personal: {} } } };
  if (c.path.startsWith("/v1/admin/runs/personal/r-1")) return { status: 200, body: sampleDetail };
  if (c.path.startsWith("/v1/admin/runs")) return { status: 200, body: { items: [sampleRun], next: null } };
  return { status: 200, body: { items: [] } };
});

// Stub unrelated tabs while capturing the real app object created by app.js.
let capturedApp = null;
const stub = { create(app) { capturedApp = app; return { load: () => Promise.resolve(), reset() {}, show() {}, hide() {} }; } };
Object.assign(globalThis.window, {
  BotStructure: stub, BotDecide: stub, BotPreapprovals: stub,
  BotAccess: stub, BotAudit: stub, BotStatus: stub,
});

load("dom.js", "api.js", "labels.js", "store.js", "runctl.js", "runs.js", "app.js");

const viewRuns = () => document.getElementById("view-runs");
const splitEl = () => all(viewRuns(), (x) => x.classList.contains("split"))[0];
const runRow = (key) => all(viewRuns(), (x) => x.getAttribute("data-run") === key)[0];

test("runs URL hash stays consistent across revisit, bare tab switch, and hashchange back", async () => {
  await settle();
  assert.equal(location.hash, "#runs");
  assert.equal(splitEl().classList.contains("has-detail"), false);

  runRow("personal|r-1").click();
  await settle();
  assert.equal(location.hash, "#runs/personal/r-1");
  assert.equal(splitEl().classList.contains("has-detail"), true);

  document.getElementById("tab-decide").click();
  await settle();
  assert.equal(location.hash, "#decide");

  capturedApp.go("runs", { profile: "personal", run: "r-1" });
  await settle();
  assert.equal(location.hash, "#runs/personal/r-1", "reopening the same run must keep #runs/{profile}/{run}");
  assert.equal(splitEl().classList.contains("has-detail"), true);

  document.getElementById("tab-decide").click();
  await settle();
  document.getElementById("tab-runs").click();
  await settle();
  assert.equal(location.hash, "#runs");
  assert.equal(splitEl().classList.contains("has-detail"), false, "bare #runs switch must clear detail selection");
  assert.ok(textOf(viewRuns()).includes("Select a run"));

  runRow("personal|r-1").click();
  await settle();
  assert.equal(location.hash, "#runs/personal/r-1");
  capturedApp.go("runs", { profile: "personal", state: "dead" });
  await settle();
  assert.equal(location.hash, "#runs");
  assert.equal(splitEl().classList.contains("has-detail"), false);

  capturedApp.go("runs", { profile: "personal", run: "r-1" });
  await settle();
  assert.equal(location.hash, "#runs/personal/r-1");
  assert.equal(splitEl().classList.contains("has-detail"), true);

  fireHash("#runs");
  await settle();
  assert.equal(location.hash, "#runs");
  assert.equal(splitEl().classList.contains("has-detail"), false, "navigating back to #runs via hashchange must close detail");
  assert.ok(textOf(viewRuns()).includes("Select a run"));
});
