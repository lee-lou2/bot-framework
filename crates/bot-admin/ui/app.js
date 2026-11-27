// Admin UI shell: resolves /v1/admin/me, mounts role-appropriate tabs, manages profile scope, preferences, badges, polling, and toasts.
(function () {
  "use strict";
  const D = window.BotDom, A = window.BotApi, S = window.BotStore, { $, h, icon } = D;
  const TABS = [
    { id: "overview", label: "Overview", ic: "home", sec: "Workspace" },
    { id: "decide", label: "Approvals & Holds", ic: "inbox" },
    { id: "runs", label: "Runs", ic: "run" },
    { id: "preapprovals", label: "Pre-approvals", op: true },
    { id: "structure", label: "Structure", ic: "grid", op: true, sec: "Configure & Inspect" },
    { id: "access", label: "Access", ic: "scope", op: true },
    { id: "audit", label: "Audit", ic: "audit", op: true },
    { id: "status", label: "Status", ic: "activity", op: true },
  ];
  const TAB_ICON = { preapprovals: "trigger" };
  const PREF_KEY = "bot_admin_prefs_v1";
  const CADENCES = [5000, 15000, 30000];
  const state = {
    me: null, tab: null, profile: "", profiles: [], auto: true, interval: 5000,
    theme: "system", density: "comfortable", structMode: "directory",
    timer: null, busy: false, seq: 0, ctl: null, stamped: 0, toastT: 0, badges: {},
  };
  const views = {};

  function readPrefs() {
    try {
      const raw = window.localStorage && window.localStorage.getItem(PREF_KEY);
      if (!raw) return;
      const p = JSON.parse(raw);
      if (p.theme === "light" || p.theme === "dark" || p.theme === "system") state.theme = p.theme;
      if (p.density === "compact" || p.density === "comfortable") state.density = p.density;
      if (p.structMode === "directory" || p.structMode === "graph") state.structMode = p.structMode;
      if (CADENCES.indexOf(p.interval) >= 0) state.interval = p.interval;
      if (typeof p.auto === "boolean") state.auto = p.auto;
    } catch (e) { /* storage unavailable in private mode */ }
  }
  function savePrefs() {
    try {
      if (window.localStorage) window.localStorage.setItem(PREF_KEY, JSON.stringify({
        theme: state.theme, density: state.density, structMode: state.structMode, interval: state.interval, auto: state.auto,
      }));
    } catch (e) { /* ignore quota or private mode */ }
  }
  function applyPrefs() {
    const rootEl = document.documentElement;
    if (rootEl && rootEl.setAttribute) {
      if (state.theme === "system") rootEl.removeAttribute("data-theme");
      else rootEl.setAttribute("data-theme", state.theme);
      if (state.density === "compact") rootEl.setAttribute("data-density", "compact");
      else rootEl.removeAttribute("data-density");
    }
    const tb = $("theme-btn");
    if (tb) {
      D.clear(tb);
      const ic = state.theme === "light" ? "sun" : state.theme === "dark" ? "moon" : "monitor";
      tb.appendChild(icon(ic, { size: 16 }));
      tb.title = "Theme: " + state.theme + " (click to cycle)";
    }
    if ($("auto")) $("auto").checked = state.auto;
    renderPrefGroups();
  }
  function segGroup(hostId, items, current, onPick) {
    const host = $(hostId);
    if (!host) return;
    D.clear(host);
    items.forEach((it) => {
      host.appendChild(h("button", { type: "button", "aria-pressed": String(it.v === current), text: it.t, onclick: () => { onPick(it.v); savePrefs(); applyPrefs(); } }));
    });
  }
  function renderPrefGroups() {
    segGroup("pref-cadence", [{ v: 5000, t: "5s" }, { v: 15000, t: "15s" }, { v: 30000, t: "30s" }], state.interval, (v) => { state.interval = v; schedule(); });
    segGroup("pref-theme", [{ v: "system", t: "Auto" }, { v: "light", t: "Light" }, { v: "dark", t: "Dark" }], state.theme, (v) => { state.theme = v; });
    segGroup("pref-density", [{ v: "comfortable", t: "Comfortable" }, { v: "compact", t: "Compact" }], state.density, (v) => { state.density = v; });
    segGroup("pref-struct", [{ v: "directory", t: "Directory" }, { v: "graph", t: "Graph" }], state.structMode, (v) => {
      state.structMode = v;
      if (views.structure && views.structure.setMode) views.structure.setMode(v);
    });
  }

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
    s.title = "Last updated " + D.when(t);
    s.appendChild(h("span", { text: "Updated " + D.clock(t) }));
    if (!s.classList || !state.auto) return;
    s.classList.remove("tick");
    if (s.offsetWidth !== undefined) void s.offsetWidth;
    s.classList.add("tick");
  }
  function setBuild(b) {
    const el = $("brand-ver");
    if (el && b && b.version) el.textContent = "v" + b.version;
  }
  function setBadges(next) {
    if (!next) return;
    Object.assign(state.badges, next);
    const paint = (id, n, tone) => {
      const el = $("badge-" + id);
      if (!el) return;
      el.textContent = n > 0 ? String(n) : "";
      el.className = "tab-badge" + (n > 0 && tone ? " is-" + tone : "");
    };
    paint("decide", state.badges.decide || 0, state.badges.decideTone || "warn");
    paint("runs", state.badges.dead || 0, "bad");
    paint("preapprovals", state.badges.paused || 0, "warn");
  }

  function renderProfileChip() {
    const chip = $("prof-chip");
    if (!chip) return;
    D.clear(chip);
    if (!state.profile) { chip.hidden = true; return; }
    chip.hidden = false;
    chip.appendChild(h("span", { text: "Profile: " + state.profile }));
    chip.appendChild(h("button", { type: "button", class: "prof-chip-x", title: "Show all profiles", "aria-label": "Clear profile filter", onclick: () => setProfile("") }, [icon("close", { size: 12 })]));
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
    renderProfileChip();
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
    const rbtn = $("refresh-now");
    if (rbtn && rbtn.classList) rbtn.classList.add("is-spin");
    return v.load(ctx)
      .then(() => { if (ctx.current()) { notice(""); if (state.stamped !== seq) stamp(); } })
      .catch((e) => {
        if ((e && e.name === "AbortError") || !ctx.current()) return;
        // Stop polling on 401 so repeated unauthenticated polls do not trigger the global rate limiter.
        if (e instanceof A.ApiError && e.word === "unauthorized") stopAuto();
        notice(A.explain(e, { not_found: "Selected profile does not exist." }));
      })
      .then(() => {
        if (seq === state.seq) {
          state.busy = false; state.ctl = null;
          if (rbtn && rbtn.classList) rbtn.classList.remove("is-spin");
        }
      });
  }
  function typing() {
    const a = document.activeElement;
    if (!a || a === document.body || !$("main").contains(a)) return false;
    return a.tagName === "TEXTAREA" || (a.tagName === "INPUT" && /^(text|search|email|url|tel|number|password)$/.test(a.type || "text"));
  }
  const confirming = () => !!(views[state.tab] && views[state.tab].busy && views[state.tab].busy());
  function auto() { if (state.auto && document.visibilityState === "visible" && !typing() && !confirming()) refresh(false); }
  function stopAuto() { if (!state.auto) return; state.auto = false; if ($("auto")) $("auto").checked = false; schedule(); }
  function schedule() {
    if (state.timer) clearInterval(state.timer);
    state.timer = state.auto ? setInterval(auto, state.interval) : null;
    if ($("stamp") && $("stamp").classList) $("stamp").classList.toggle("paused", !state.auto);
  }

  const visibleTabs = () => TABS.filter((t) => !t.op || (state.me && state.me.operator));
  const available = (id) => visibleTabs().some((t) => t.id === id);
  function moveInk() {
    const nav = $("tabs"), b = $("tab-" + state.tab);
    if (!nav || !nav.style || !b || !b.offsetHeight) return;
    nav.style.setProperty("--ink-y", (b.offsetTop + Math.round((b.offsetHeight - 20) / 2)) + "px");
    nav.style.setProperty("--ink-on", "1");
  }
  function setHash(s) {
    const next = "#" + s;
    if (window.history && history.replaceState && location.hash !== next) history.replaceState(null, "", next);
  }
  function closeNav() {
    if (document.body && document.body.classList) document.body.classList.remove("nav-open");
    if ($("rail-backdrop")) $("rail-backdrop").hidden = true;
    if ($("menu-btn")) $("menu-btn").setAttribute("aria-expanded", "false");
  }
  function openNav() {
    if (document.body && document.body.classList) document.body.classList.add("nav-open");
    if ($("rail-backdrop")) $("rail-backdrop").hidden = false;
    if ($("menu-btn")) $("menu-btn").setAttribute("aria-expanded", "true");
  }
  function setPrefsOpen(on) {
    const pop = $("prefs-pop"), btn = $("prefs-btn");
    if (!pop || !btn) return;
    pop.hidden = !on;
    btn.setAttribute("aria-expanded", String(on));
  }
  function updateBodyMap() {
    const isGraph = state.tab === "structure" && views.structure && views.structure.isGraph && views.structure.isGraph();
    if (document.body && document.body.classList) document.body.classList.toggle("is-map", !!isGraph);
  }
  function showTab(id, opts) {
    opts = opts || {};
    if (!available(id)) id = visibleTabs()[0].id;
    const prev = state.tab;
    state.tab = id;
    closeNav();
    Array.prototype.forEach.call(document.querySelectorAll("#tabs [role=tab]"), (b) => {
      const on = b.getAttribute("data-tab") === id;
      b.setAttribute("aria-selected", String(on));
      b.setAttribute("tabindex", on ? "0" : "-1");
      if (on && opts.focus && b.focus) b.focus();
      if (on && b.scrollIntoView) b.scrollIntoView({ block: "nearest", inline: "nearest" });
    });
    TABS.forEach((t) => { const el = $("view-" + t.id); if (el) el.hidden = t.id !== id; });
    const meta = TABS.filter((t) => t.id === id)[0];
    if ($("page-crumb") && meta) $("page-crumb").textContent = meta.label;
    updateBodyMap();
    moveInk();
    const panel = $("view-" + id);
    if (id !== "structure" && panel && panel.classList) {
      panel.classList.remove("enter");
      if (panel.offsetWidth !== undefined) void panel.offsetWidth;
      panel.classList.add("enter");
    }
    if (prev && prev !== id && views[prev] && views[prev].hide) views[prev].hide();
    const v = views[id];
    if (v && v.apply) v.apply(opts.arg);
    setHash(v && v.hash ? v.hash() : id);
    if (v && v.show) v.show();
    updateBodyMap();
    refresh(true);
  }
  function go(id, arg) {
    if (!available(id)) return;
    const p = arg && arg.profile;
    if (p && state.profile !== p && (state.profile || arg.run === undefined)) setProfile(p, true);
    showTab(id, { arg });
  }
  function tabKeys(e) {
    const ids = visibleTabs().map((t) => t.id), i = ids.indexOf(state.tab);
    const at = { ArrowDown: i + 1, ArrowRight: i + 1, ArrowUp: i - 1, ArrowLeft: i - 1, Home: 0, End: ids.length - 1 }[e.key];
    if (at === undefined) return;
    e.preventDefault();
    showTab(ids[(at + ids.length) % ids.length], { focus: true });
  }
  function buildTabs() {
    const nav = $("tabs"), ink = $("tab-ink");
    visibleTabs().forEach((t) => {
      if (t.sec) nav.insertBefore(h("span", { class: "nav-sec", "aria-hidden": "true", text: t.sec }), ink);
      const ic = t.ic || TAB_ICON[t.id] || "agent";
      const badge = (t.id === "decide" || t.id === "runs" || t.id === "preapprovals") ? h("span", { class: "tab-badge", id: "badge-" + t.id }) : null;
      nav.insertBefore(h("button", { type: "button", role: "tab", id: "tab-" + t.id, "aria-controls": "view-" + t.id, "aria-selected": "false", tabindex: "-1", "data-tab": t.id, onclick: () => showTab(t.id) }, [icon(ic, { size: 16 }), h("span", { class: "tab-lbl", text: t.label }), badge]), ink);
    });
  }
  function buildViews(app) {
    const op = state.me.operator;
    views.overview = window.BotOverview.create(app, $("view-overview"));
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

  const app = {
    me: () => state.me, profile: () => state.profile, structMode: () => state.structMode,
    setStructMode: (m) => { state.structMode = m; savePrefs(); renderPrefGroups(); updateBodyMap(); },
    go, refresh, notice, toast, stamp, noteProfiles, setHash, setBadges, setBuild,
  };

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
  function seedStatus(me) {
    if (!me.operator) return;
    A.get("/v1/admin/status").then((d) => {
      const profiles = d.profiles || {};
      noteProfiles(Object.keys(profiles));
      if (d.build) setBuild(d.build);
      let approvals = 0, holds = 0, dead = 0;
      Object.keys(profiles).forEach((p) => {
        approvals += profiles[p].approvals_pending || 0;
        holds += profiles[p].holds || 0;
        dead += (profiles[p].runs && profiles[p].runs.dead) || 0;
      });
      setBadges({ decide: approvals + holds, decideTone: holds > 0 ? "bad" : "warn", dead });
    }).catch(() => {});
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
    seedStatus(me);
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
    readPrefs();
    const put = (id, name, size) => { const el = $(id); if (el) el.appendChild(icon(name, { size })); };
    put("mark", "logo", 16);
    put("prof-ic", "profile", 14);
    put("sel-ic", "down", 14);
    put("search-ic", "search", 16);
    put("fit-ic", "fit", 16);
    put("zin", "plus", 16);
    put("zout", "minus", 16);
    put("menu-btn", "menu", 18);
    put("rail-close", "close", 16);
    put("refresh-now", "refresh", 15);
    put("prefs-ic", "sliders", 14);
    applyPrefs();
    renderProfileChip();
    $("tabs").addEventListener("keydown", tabKeys);
    $("profile").addEventListener("change", (e) => setProfile(e.target.value));
    $("auto").addEventListener("change", (e) => { state.auto = e.target.checked; savePrefs(); schedule(); });
    if ($("menu-btn")) $("menu-btn").addEventListener("click", openNav);
    if ($("rail-close")) $("rail-close").addEventListener("click", closeNav);
    if ($("rail-backdrop")) $("rail-backdrop").addEventListener("click", closeNav);
    if ($("refresh-now")) $("refresh-now").addEventListener("click", () => refresh(true));
    if ($("theme-btn")) $("theme-btn").addEventListener("click", () => {
      const order = ["system", "light", "dark"];
      state.theme = order[(order.indexOf(state.theme) + 1) % order.length];
      savePrefs(); applyPrefs();
    });
    if ($("prefs-btn")) $("prefs-btn").addEventListener("click", (e) => {
      e.stopPropagation();
      setPrefsOpen($("prefs-pop").hidden);
    });
    if ($("prefs-pop")) $("prefs-pop").addEventListener("click", (e) => e.stopPropagation());
    document.addEventListener("click", (e) => {
      const pop = $("prefs-pop"), btn = $("prefs-btn");
      if (pop && !pop.hidden && !pop.contains(e.target) && btn && !btn.contains(e.target)) setPrefsOpen(false);
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") { setPrefsOpen(false); closeNav(); return; }
      if (typing() || e.ctrlKey || e.metaKey || e.altKey) return;
      if (e.key === "r") { e.preventDefault(); refresh(true); }
    });
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
