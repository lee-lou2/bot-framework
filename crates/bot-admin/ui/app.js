// Admin UI shell: resolves /v1/admin/me, mounts role-appropriate tabs, manages profile filter, polling, and toasts.
(function () {
  "use strict";
  const D = window.BotDom, A = window.BotApi, S = window.BotStore, { $, h, icon } = D;
  const TABS = [
    { id: "structure", label: "Structure", op: true },
    { id: "runs", label: "Runs" },
    { id: "decide", label: "Approvals & Holds" },
    { id: "preapprovals", label: "Pre-approvals", op: true },
    { id: "access", label: "Access", op: true },
    { id: "audit", label: "Audit", op: true },
    { id: "status", label: "Status", op: true },
  ];
  const state = { me: null, tab: null, profile: "", profiles: [], auto: true, timer: null, busy: false, seq: 0, ctl: null, stamped: 0, toastT: 0 };
  const views = {};

  function notice(msg) { $("notice").textContent = msg || ""; }
  function toast(msg, tone) {
    const el = $("toast");
    el.className = "toast show is-" + (tone || "ok");
    el.textContent = msg;
    clearTimeout(state.toastT);
    state.toastT = setTimeout(() => { el.classList.remove("show"); }, 5000);
  }
  function stamp(ms) {
    state.stamped = state.seq;
    const s = $("stamp"), t = ms || D.now();
    D.clear(s);
    s.title = D.when(t);
    s.appendChild(h("span", { class: "st-full", text: "As of " + D.when(t) }));
    s.appendChild(h("span", { class: "st-short", text: "As of " + D.clock(t) }));
    if (!s.classList || !state.auto) return;
    s.classList.remove("tick");
    if (s.offsetWidth !== undefined) void s.offsetWidth;
    s.classList.add("tick");
  }

  function noteProfiles(list) {
    const have = state.profiles.slice();
    (list || []).forEach((p) => { if (p && have.indexOf(p) < 0) have.push(p); });
    have.sort();
    if (have.join(",") === state.profiles.join(",")) return;
    state.profiles = have;
    const sel = $("profile");
    while (sel.options.length > 1) sel.remove(1);
    have.forEach((p) => sel.appendChild(h("option", { value: p, text: p })));
    sel.value = state.profile;
  }
  function resetData() {
    S.reset();
    Object.keys(views).forEach((id) => { if (views[id].reset) views[id].reset(); });
    D.clear($("stamp"));
    notice("");
  }
  function setProfile(p, quiet) {
    state.profile = p;
    const sel = $("profile");
    if (p && state.profiles.indexOf(p) < 0) noteProfiles([p]);
    sel.value = p;
    resetData();
    if (!quiet && state.tab && views[state.tab] && views[state.tab].hash) setHash(views[state.tab].hash());
    if (!quiet) refresh(true);
  }

  // Sequence-guard requests so stale responses cannot overwrite newer selections; skip overlapping background polls.
  function refresh(manual) {
    const id = state.tab, v = id && views[id];
    if (!v) return Promise.resolve();
    if (state.busy && !manual) return Promise.resolve();
    if (state.ctl) state.ctl.abort();
    const seq = ++state.seq, ctl = new AbortController();
    const ctx = { profile: state.profile, signal: ctl.signal, manual, current: () => seq === state.seq && id === state.tab };
    state.ctl = ctl; state.busy = true;
    return v.load(ctx)
      .then(() => { if (ctx.current()) { notice(""); if (state.stamped !== seq) stamp(); } })
      .catch((e) => {
        if ((e && e.name === "AbortError") || !ctx.current()) return;
        // Stop polling on 401 so repeated unauthenticated polls do not trigger the global rate limiter.
        if (e instanceof A.ApiError && e.word === "unauthorized") stopAuto();
        notice(A.explain(e, { not_found: "Selected profile does not exist." }));
      })
      .then(() => { if (seq === state.seq) { state.busy = false; state.ctl = null; } });
  }
  function typing() {
    const a = document.activeElement;
    if (!a || a === document.body || !$("main").contains(a)) return false;
    return a.tagName === "TEXTAREA" || (a.tagName === "INPUT" && /^(text|search|email|url|tel|number|password)$/.test(a.type || "text"));
  }
  const confirming = () => !!(views[state.tab] && views[state.tab].busy && views[state.tab].busy());
  function auto() { if (state.auto && document.visibilityState === "visible" && !typing() && !confirming()) refresh(false); }
  function stopAuto() { if (!state.auto) return; state.auto = false; $("auto").checked = false; schedule(); }
  function schedule() {
    if (state.timer) clearInterval(state.timer);
    state.timer = state.auto ? setInterval(auto, 5000) : null;
    $("stamp").classList.toggle("paused", !state.auto);
  }

  const visibleTabs = () => TABS.filter((t) => !t.op || (state.me && state.me.operator));
  const available = (id) => visibleTabs().some((t) => t.id === id);
  function moveInk() {
    const nav = $("tabs"), b = $("tab-" + state.tab);
    if (!b || !b.offsetWidth) return;
    nav.style.setProperty("--ink-x", b.offsetLeft + "px");
    nav.style.setProperty("--ink-w", String(b.offsetWidth / 100));
  }
  function setHash(s) {
    const next = "#" + s;
    if (window.history && history.replaceState && location.hash !== next) history.replaceState(null, "", next);
  }
  function showTab(id, opts) {
    opts = opts || {};
    if (!available(id)) id = visibleTabs()[0].id;
    const prev = state.tab;
    state.tab = id;
    Array.prototype.forEach.call(document.querySelectorAll("#tabs [role=tab]"), (b) => {
      const on = b.getAttribute("data-tab") === id;
      b.setAttribute("aria-selected", String(on));
      b.setAttribute("tabindex", on ? "0" : "-1");
      if (on && opts.focus && b.focus) b.focus();
      if (on && b.scrollIntoView) b.scrollIntoView({ block: "nearest", inline: "nearest" });
    });
    TABS.forEach((t) => { const el = $("view-" + t.id); if (el) el.hidden = t.id !== id; });
    document.body.classList.toggle("is-map", id === "structure");
    moveInk();
    const panel = $("view-" + id);
    if (id !== "structure" && panel.classList) {
      panel.classList.remove("enter");
      if (panel.offsetWidth !== undefined) void panel.offsetWidth;
      panel.classList.add("enter");
    }
    if (prev && prev !== id && views[prev] && views[prev].hide) views[prev].hide();
    const v = views[id];
    if (v.apply) v.apply(opts.arg);
    setHash(v.hash ? v.hash() : id);
    if (v.show) v.show();
    refresh(true);
  }
  function go(id, arg) {
    if (!available(id)) return;
    const p = arg && arg.profile;
    if (p && state.profile !== p && (state.profile || arg.run === undefined)) setProfile(p, true);
    showTab(id, { arg });
  }
  function tabKeys(e) {
    const ids = visibleTabs().map((t) => t.id), i = ids.indexOf(state.tab), at = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: ids.length - 1 }[e.key];
    if (at === undefined) return;
    e.preventDefault();
    showTab(ids[(at + ids.length) % ids.length], { focus: true });
  }
  function buildTabs() {
    const nav = $("tabs"), ink = $("tab-ink");
    visibleTabs().forEach((t) => nav.insertBefore(h("button", { type: "button", role: "tab", id: "tab-" + t.id, "aria-controls": "view-" + t.id, "aria-selected": "false", tabindex: "-1", "data-tab": t.id, text: t.label, onclick: () => showTab(t.id) }), ink));
  }
  function buildViews(app) {
    const op = state.me.operator;
    if (op) views.structure = window.BotStructure.create(app);
    views.runs = window.BotRuns.create(app, $("view-runs"));
    views.decide = window.BotDecide.create(app, $("view-decide"));
    if (op) {
      views.preapprovals = window.BotPreapprovals.create(app, $("view-preapprovals"));
      views.access = window.BotAccess.create(app, $("view-access"));
      views.audit = window.BotAudit.create(app, $("view-audit"));
      views.status = window.BotStatus.create(app, $("view-status"));
    }
  }

  const app = { me: () => state.me, profile: () => state.profile, go, refresh, notice, toast, stamp, noteProfiles, setHash };

  function parseHash() {
    const parts = location.hash.replace(/^#/, "").split("/").map((s) => { try { return decodeURIComponent(s); } catch (e) { return ""; } });
    return { id: parts[0], rest: parts.slice(1) };
  }
  function routeFromHash(first) {
    const at = parseHash();
    if (!available(at.id)) { if (first) showTab(visibleTabs()[0].id); return; }
    const arg = at.id === "runs" && at.rest.length >= 2 && at.rest[0] && at.rest[1] ? { profile: at.rest[0], run: at.rest[1] } : undefined;
    const hasSel = !!(views[at.id] && views[at.id].selected && views[at.id].selected());
    if (!first && at.id === state.tab && !arg && !hasSel) return;
    if (arg && state.profile && state.profile !== arg.profile) setProfile(arg.profile, true);
    showTab(at.id, { arg });
  }
  function onMe(me) {
    state.me = me;
    $("boot").hidden = true;
    $("who").title = "Signed in as: " + me.principal;
    $("who").appendChild(h("code", { text: me.principal }));
    $("who").appendChild(h("span", { class: "role" + (me.operator ? " is-op" : ""), text: me.operator ? "Operator" : "Member" }));
    buildTabs();
    // Remove operator-only panels from the DOM rather than merely hiding them.
    TABS.forEach((t) => { const el = $("view-" + t.id); if (t.op && !me.operator && el) el.parentNode.removeChild(el); });
    buildViews(app);
    routeFromHash(true);
    window.addEventListener("hashchange", () => routeFromHash(false));
    schedule();
    if (me.operator) A.get("/v1/admin/status").then((d) => noteProfiles(Object.keys(d.profiles || {}))).catch(() => {});
  }
  function start() {
    A.me().then(onMe, (e) => {
      const boot = $("boot");
      D.clear(boot);
      boot.appendChild(D.empty("Could not load admin UI", A.explain(e), "alert"));
      notice(A.explain(e));
      // Do not retry 401 automatically because each failed attempt counts toward the auth rate limit.
      if (e.word !== "unauthorized") setTimeout(start, e.word === "rate_limited" ? 30000 : 5000);
    });
  }

  function init() {
    $("mark").appendChild(icon("logo", { size: 16 }));
    $("sel-ic").appendChild(icon("down", { size: 14 }));
    $("search-ic").appendChild(icon("search", { size: 16 }));
    $("fit-ic").appendChild(icon("fit", { size: 16 }));
    $("zin").appendChild(icon("plus", { size: 16 }));
    $("zout").appendChild(icon("minus", { size: 16 }));
    $("tabs").addEventListener("keydown", tabKeys);
    $("profile").addEventListener("change", (e) => setProfile(e.target.value));
    $("auto").addEventListener("change", (e) => { state.auto = e.target.checked; schedule(); });
    window.addEventListener("resize", D.debounce(moveInk, 60));
    document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible") auto(); });
    setInterval(() => { if (document.visibilityState === "visible") D.tick(); }, 1000);
    const nav = $("tabs");
    nav.classList.add("first");
    setTimeout(() => nav.classList.remove("first"), 0);
    start();
  }
  init();
})();
