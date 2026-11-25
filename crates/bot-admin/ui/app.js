// 관리 화면의 뼈대: 신원(`/v1/admin/me`)을 알아낸 뒤 탭·프로필·자동 새로고침·알림을 묶는다. 각 탭의 조회와 그리기는 탭 모듈이 맡는다.
// 모든 글자는 textContent로만 넣는다(HTML 문자열 삽입 없음). 운영자가 아니면 운영자 전용 탭은 만들지 않는다.
(function () {
  "use strict";
  const D = window.BotDom, A = window.BotApi, S = window.BotStore, { $, h, icon } = D;
  // op: 운영자 전용 탭. 서버도 같은 규칙으로 막고, 화면은 처음부터 보이지 않게 한다.
  const TABS = [
    { id: "structure", label: "구조", op: true },
    { id: "runs", label: "실행" },
    { id: "decide", label: "승인·보류" },
    { id: "preapprovals", label: "사전 승인", op: true },
    { id: "access", label: "권한", op: true },
    { id: "audit", label: "감사", op: true },
    { id: "status", label: "상태", op: true },
  ];
  const state = { me: null, tab: null, profile: "", profiles: [], auto: true, timer: null, busy: false, seq: 0, ctl: null, stamped: 0, toastT: 0 };
  const views = {};

  function notice(msg) { $("notice").textContent = msg || ""; }
  // 짧은 알림(성공·실패 한 줄). 화면 읽기 프로그램에는 «상태»로 알린다.
  function toast(msg, tone) {
    const el = $("toast");
    el.className = "toast show is-" + (tone || "ok");
    el.textContent = msg;
    clearTimeout(state.toastT);
    state.toastT = setTimeout(() => { el.classList.remove("show"); }, 5000);
  }
  // 기준 시각과 «새로고침이 성공할 때마다 시각 옆 점이 한 번 깜빡인다».
  function stamp(ms) {
    state.stamped = state.seq;
    const s = $("stamp"), t = ms || D.now();
    // 넓은 화면은 날짜·시간대까지, 좁은 화면은 시:분:초만 보인다(css가 둘 중 하나를 숨긴다).
    D.clear(s);
    s.title = D.when(t);
    s.appendChild(h("span", { class: "st-full", text: "기준 " + D.when(t) }));
    s.appendChild(h("span", { class: "st-short", text: "기준 " + D.clock(t) }));
    if (!s.classList || !state.auto) return;
    s.classList.remove("tick");
    if (s.offsetWidth !== undefined) void s.offsetWidth;
    s.classList.add("tick");
  }

  // ---------- 프로필 ----------
  // 화면이 알게 된 프로필 이름을 모아 둔다(한 번 알게 되면 지우지 않는다: 하나를 골라도 다른 프로필로 돌아갈 수 있게).
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
  // 프로필을 바꾸면 이전 프로필의 자료를 모두 지우고(다른 프로필 자료가 새 선택 밑에 남지 않게) 새 선택으로 다시 묻는다.
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
    if (!quiet) refresh(true);
  }

  // ---------- 조회·자동 새로고침 ----------
  // 요청마다 번호(seq)와 그때의 선택(profile·탭)을 붙들어, 선택이 바뀌었거나 더 새 요청이 생긴 뒤에 도착한 옛 응답·오류는 화면에 쓰지 않는다.
  // 자동 갱신(manual 없음)은 진행 중인 요청이 있으면 건너뛴다 — 느린 서버에서 응답이 계속 밀려 버려지는 일을 막는다.
  // 사용자가 프로필·탭을 바꾸거나 판정한 뒤에는 manual=true: 진행 중 요청을 끊고 가장 새 선택으로 반드시 다시 묻는다.
  function refresh(manual) {
    const id = state.tab, v = id && views[id];
    if (!v) return Promise.resolve();
    if (state.busy && !manual) return Promise.resolve();
    if (state.ctl) state.ctl.abort();
    const seq = ++state.seq, ctl = new AbortController();
    const ctx = { profile: state.profile, signal: ctl.signal, current: () => seq === state.seq && id === state.tab };
    state.ctl = ctl; state.busy = true;
    return v.load(ctx)
      .then(() => { if (ctx.current()) { notice(""); if (state.stamped !== seq) stamp(); } })
      .catch((e) => {
        if ((e && e.name === "AbortError") || !ctx.current()) return;
        // 인증이 끊기면 자동 새로고침을 멈춘다: 인증 없는 요청마다 서버의 실패 한도(전역)에 세어져 운영자까지 잠길 수 있다.
        if (e instanceof A.ApiError && e.word === "unauthorized") stopAuto();
        notice(A.explain(e, { not_found: "선택한 프로필이 없습니다." }));
      })
      .then(() => { if (seq === state.seq) { state.busy = false; state.ctl = null; } });
  }
  // 글자를 쓰는 입력칸에 포커스가 있으면 자동 새로고침이 입력을 방해하지 않게 건너뛴다.
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

  // ---------- 탭 ----------
  const visibleTabs = () => TABS.filter((t) => !t.op || (state.me && state.me.operator));
  const available = (id) => visibleTabs().some((t) => t.id === id);
  function moveInk() {
    const nav = $("tabs"), b = $("tab-" + state.tab);
    if (!b || !b.offsetWidth) return;
    nav.style.setProperty("--ink-x", b.offsetLeft + "px");
    nav.style.setProperty("--ink-w", String(b.offsetWidth / 100));
  }
  // 주소 조각(#실행/프로필/실행ID)을 바꾼다. 기록은 쌓지 않는다.
  function setHash(s) {
    if (window.history && history.replaceState) history.replaceState(null, "", "#" + s);
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
    setHash(id);
    const v = views[id];
    if (v.apply && opts.arg !== undefined) v.apply(opts.arg);
    if (v.show) v.show();
    refresh(true);
  }
  // 다른 탭으로 건너간다(arg: { profile, run, state }). 프로필이 다르면 선택도 그 프로필로 옮긴다.
  function go(id, arg) {
    if (!available(id)) return;
    // 실행 하나를 열 때는 «전체» 선택을 그대로 두고, 다른 프로필이 골라져 있거나 프로필만 가리키는 이동이면 선택을 옮긴다.
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

  // ---------- 시작 ----------
  const app = { me: () => state.me, profile: () => state.profile, go, refresh, notice, toast, stamp, noteProfiles, setHash };

  function parseHash() {
    const parts = location.hash.replace(/^#/, "").split("/").map((s) => { try { return decodeURIComponent(s); } catch (e) { return ""; } });
    return { id: parts[0], rest: parts.slice(1) };
  }
  // 주소 조각 → 탭(과 실행 하나). 시작할 때와 주소를 직접 고쳤을 때 같은 규칙을 쓴다.
  function routeFromHash(first) {
    const at = parseHash();
    if (!available(at.id)) { if (first) showTab(visibleTabs()[0].id); return; }
    const arg = at.id === "runs" && at.rest.length >= 2 ? { profile: at.rest[0], run: at.rest[1] } : undefined;
    if (!first && at.id === state.tab && !arg) return;
    if (arg && state.profile && state.profile !== arg.profile) setProfile(arg.profile, true);
    showTab(at.id, { arg });
  }
  function onMe(me) {
    state.me = me;
    $("boot").hidden = true;
    $("who").title = "로그인한 주체: " + me.principal;
    $("who").appendChild(h("code", { text: me.principal }));
    $("who").appendChild(h("span", { class: "role" + (me.operator ? " is-op" : ""), text: me.operator ? "운영자" : "일반" }));
    buildTabs();
    // 운영자가 아니면 운영자 전용 화면은 아예 두지 않는다(숨기기만 하면 탭 없는 패널이 남는다).
    TABS.forEach((t) => { const el = $("view-" + t.id); if (t.op && !me.operator && el) el.parentNode.removeChild(el); });
    buildViews(app);
    routeFromHash(true);
    window.addEventListener("hashchange", () => routeFromHash(false));
    schedule();
    // 운영자는 프로필 목록을 미리 알아 둔다(어느 탭으로 시작하든 고를 수 있게).
    if (me.operator) A.get("/v1/admin/status").then((d) => noteProfiles(Object.keys(d.profiles || {}))).catch(() => {});
  }
  function start() {
    A.me().then(onMe, (e) => {
      const boot = $("boot");
      D.clear(boot);
      boot.appendChild(D.empty("화면을 열지 못했습니다", A.explain(e), "alert"));
      notice(A.explain(e));
      // 인증 실패(401)는 다시 시도하지 않는다(시도마다 실패 한도에 세어진다). 한도에 막혔으면 한참 뒤에, 연결 문제면 5초 뒤에 다시 시도한다.
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
