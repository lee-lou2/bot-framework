// 실행 탭과 주소 조각(#runs/프로필/실행ID) 동기화 시험: 실제 app.js·runs.js를 DOM 모조품 위에서 돌린다.
// 확인하는 것: (1) 다른 탭을 다녀와 같은 (profile, run)을 다시 열 때 #runs/{profile}/{run} 유지,
// (2) 인자 없이 #runs 탭으로 돌아오면 상세 선택을 비워 해시와 일치, (3) hashchange로 #runs/p/r → #runs 뒤로 이동 시 상세가 닫힘.
import test from "node:test";
import assert from "node:assert/strict";
import { install, load, fakeFetch, settle, all, textOf } from "./support/fakedom.mjs";

const { document, body } = install();

// app.js가 기대하는 최소 DOM 뼈대와 브라우저 전역(location·history·hashchange)을 준비한다.
const mk = (tag, id) => {
  const el = document.createElement(tag);
  if (id) el.setAttribute("id", id);
  return el;
};
const profileSel = mk("select", "profile");
profileSel.appendChild(Object.assign(mk("option"), { value: "", textContent: "전체" }));
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

// 다른 탭은 빈 대역으로 두고 app 객체를 붙잡는다(runs.js와 app.js는 실제 코드).
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

  // 1) 실행 상세를 열고 다른 탭으로 갔다가 같은 (profile, run)을 다시 열면 #runs/personal/r-1이 유지된다
  runRow("personal|r-1").click();
  await settle();
  assert.equal(location.hash, "#runs/personal/r-1");
  assert.equal(splitEl().classList.contains("has-detail"), true);

  document.getElementById("tab-decide").click();
  await settle();
  assert.equal(location.hash, "#decide");

  capturedApp.go("runs", { profile: "personal", run: "r-1" });
  await settle();
  assert.equal(location.hash, "#runs/personal/r-1", "같은 실행을 다시 열 때 해시가 #runs로 덮어씌워지면 안 된다");
  assert.equal(splitEl().classList.contains("has-detail"), true);

  // 2) 다른 탭에서 인자 없이 실행 탭(#runs)으로 돌아오면 상세 선택이 비워져 해시와 일치한다
  document.getElementById("tab-decide").click();
  await settle();
  document.getElementById("tab-runs").click();
  await settle();
  assert.equal(location.hash, "#runs");
  assert.equal(splitEl().classList.contains("has-detail"), false, "인자 없는 #runs 전환은 상세 선택을 비워야 한다");
  assert.ok(textOf(viewRuns()).includes("실행을 고르세요"));

  // 상태 필터만으로 넘어온 경우(run 없음)도 상세 선택을 비우고 #runs를 유지한다
  runRow("personal|r-1").click();
  await settle();
  assert.equal(location.hash, "#runs/personal/r-1");
  capturedApp.go("runs", { profile: "personal", state: "dead" });
  await settle();
  assert.equal(location.hash, "#runs");
  assert.equal(splitEl().classList.contains("has-detail"), false);

  // 3) #runs/personal/r-1에서 브라우저 뒤로가기(hashchange → #runs)하면 상세 화면이 실제로 닫힌다
  capturedApp.go("runs", { profile: "personal", run: "r-1" });
  await settle();
  assert.equal(location.hash, "#runs/personal/r-1");
  assert.equal(splitEl().classList.contains("has-detail"), true);

  fireHash("#runs");
  await settle();
  assert.equal(location.hash, "#runs");
  assert.equal(splitEl().classList.contains("has-detail"), false, "hashchange로 #runs로 돌아오면 상세가 닫혀야 한다");
  assert.ok(textOf(viewRuns()).includes("실행을 고르세요"));
});
