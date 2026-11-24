// 화면 공통 도우미: DOM 만들기(HTML 문자열은 쓰지 않는다), 선 아이콘(createElementNS), 상태 점, 움직임·서식·표·목록 갱신 도우미.
(function (root) {
  "use strict";

  // SVG 이름공간은 식별자일 뿐 요청이 아니다. 줄 단위 검사(외부 주소 0건)에 걸리지 않게 조각으로 이어 붙인다.
  const NS = ["http:", "", "www.w3.org", "2000", "svg"].join("/");

  const $ = (id) => document.getElementById(id);
  const text = (s) => document.createTextNode(String(s));
  function clear(el) { while (el.firstChild) el.removeChild(el.firstChild); }
  function append(el, kids) {
    (kids || []).forEach((c) => {
      if (c === null || c === undefined || c === false) return;
      if (Array.isArray(c)) append(el, c);
      else el.appendChild(typeof c === "string" || typeof c === "number" ? text(c) : c);
    });
    return el;
  }
  function props(el, p) {
    if (p) for (const k in p) {
      const v = p[k];
      if (v === null || v === undefined || v === false) continue;
      if (k === "class") el.setAttribute("class", v);
      else if (k === "text") el.textContent = v;
      else if (k.slice(0, 2) === "on") el.addEventListener(k.slice(2), v);
      else el.setAttribute(k, v === true ? "" : v);
    }
    return el;
  }
  const h = (tag, p, kids) => append(props(document.createElement(tag), p), kids);
  const svg = (tag, p, kids) => append(props(document.createElementNS(NS, tag), p), kids);
  const code = (s, title) => h("code", { text: String(s), title: title || null });

  // ---------- 선 아이콘(24 격자, currentColor) ----------
  const P = (d) => ["path", { d }];
  const ICONS = {
    agent: [["circle", { cx: 12, cy: 8, r: 3.6 }], P("M5 20c.9-3.7 3.6-5.6 7-5.6s6.1 1.9 7 5.6")],
    skill: [P("M12 3.5l2.3 6.2 6.2 2.3-6.2 2.3L12 20.5l-2.3-6.2L3.5 12l6.2-2.3z")],
    tool: [["rect", { x: 3.5, y: 5, width: 17, height: 14, rx: 3.2 }], P("M8 10l3 2-3 2"), P("M13 15h3.5")],
    trigger: [P("M13 3.5L6 13.2h5.3l-.6 7.3L18 10.6h-5.2z")],
    binding: [P("M14 4.5h3.5A2.5 2.5 0 0 1 20 7v10a2.5 2.5 0 0 1-2.5 2.5H14"), P("M4 12h10"), P("M10.5 8.5L14 12l-3.5 3.5")],
    scope: [P("M12 3.5l7 2.8v5.4c0 4.4-2.9 7.6-7 9-4.1-1.4-7-4.6-7-9V6.3z"), P("M9 12l2.2 2.2L15.2 10")],
    secret: [["circle", { cx: 8, cy: 15, r: 3.8 }], P("M10.8 12.2L20 3.5"), P("M16.2 7.3l2.6 2.6")],
    profile: [P("M12 3.5l8.5 4.5-8.5 4.5L3.5 8z"), P("M3.5 12l8.5 4.5 8.5-4.5"), P("M3.5 16l8.5 4.5 8.5-4.5")],
    run: [["circle", { cx: 12, cy: 12, r: 8.5 }], P("M10.3 8.8l4.9 3.2-4.9 3.2z")],
    logo: [["circle", { cx: 12, cy: 5.5, r: 2.3 }], ["circle", { cx: 5.5, cy: 18, r: 2.3 }], ["circle", { cx: 18.5, cy: 18, r: 2.3 }], P("M11 7.6L6.6 15.9"), P("M13 7.6l4.4 8.3")],
    search: [["circle", { cx: 11, cy: 11, r: 6.5 }], P("M16 16l4.5 4.5")],
    close: [P("M6.5 6.5l11 11M17.5 6.5l-11 11")],
    plus: [P("M12 5.5v13M5.5 12h13")],
    minus: [P("M5.5 12h13")],
    fit: [P("M4 9V6a2 2 0 0 1 2-2h3M15 4h3a2 2 0 0 1 2 2v3M20 15v3a2 2 0 0 1-2 2h-3M9 20H6a2 2 0 0 1-2-2v-3")],
    chevron: [P("M9 6l6 6-6 6")],
    down: [P("M6 9l6 6 6-6")],
    back: [P("M15 6l-6 6 6 6")],
    check: [P("M5 12.5l4.5 4.5L19 7.5")],
    alert: [P("M12 4l9 15.5H3z"), P("M12 10v4"), P("M12 17.2v.1")],
    pause: [P("M9 6.5v11M15 6.5v11")],
    arrow: [P("M5 12h14M13 6l6 6-6 6")],
    lock: [["rect", { x: 5, y: 10.5, width: 14, height: 9.5, rx: 2.6 }], P("M8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5")],
    inbox: [P("M4 13l2.5-7h11L20 13v5a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 4 18z"), P("M4 13h4.5l1 2h5l1-2H20")],
    power: [P("M12 3.5v8"), P("M7.2 6.6a7.5 7.5 0 1 0 9.6 0")],
    eye: [P("M2.5 12s3.4-6.5 9.5-6.5 9.5 6.5 9.5 6.5-3.4 6.5-9.5 6.5S2.5 12 2.5 12z"), ["circle", { cx: 12, cy: 12, r: 2.8 }]],
    external: [P("M14 4h6v6"), P("M20 4l-9 9"), P("M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4")],
  };
  // opts: { size, cls, off } — off면 사선을 긋는다(꺼짐을 색이 아니라 모양으로도 알린다).
  function icon(name, opts) {
    opts = opts || {};
    const s = opts.size || 16;
    const el = svg("svg", { viewBox: "0 0 24 24", width: s, height: s, fill: "none", stroke: "currentColor", "stroke-width": 1.75, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true", focusable: "false", class: "ic" + (opts.cls ? " " + opts.cls : "") });
    (ICONS[name] || ICONS.agent).forEach((n) => el.appendChild(svg(n[0], n[1])));
    if (opts.off) el.appendChild(svg("path", { d: "M5 19L19 5", class: "slash" }));
    return el;
  }

  // 상태 점(작은 모양 + 글자와 함께 쓴다): on 채운 원 · off 빈 원 · held 두 줄 · error 삼각형
  function dot(state) {
    const el = svg("svg", { viewBox: "0 0 10 10", width: 10, height: 10, "aria-hidden": "true", focusable: "false", class: "dot dot-" + state });
    if (state === "on") el.appendChild(svg("circle", { cx: 5, cy: 5, r: 4, fill: "currentColor" }));
    else if (state === "off") el.appendChild(svg("circle", { cx: 5, cy: 5, r: 3.1, fill: "none", stroke: "currentColor", "stroke-width": 1.5 }));
    else if (state === "held") { el.appendChild(svg("rect", { x: 1.6, y: 1.6, width: 2.4, height: 6.8, rx: 1, fill: "currentColor" })); el.appendChild(svg("rect", { x: 6, y: 1.6, width: 2.4, height: 6.8, rx: 1, fill: "currentColor" })); }
    else el.appendChild(svg("path", { d: "M5 1l4.4 8H.6z", fill: "currentColor", stroke: "currentColor", "stroke-width": 1, "stroke-linejoin": "round" }));
    return el;
  }
  // 알약: 상태 점 + 글자(한 줄). tone: ok | off | warn | bad | info
  const pill = (label, tone, state) => h("span", { class: "pill pill-" + (tone || "info") }, [state ? dot(state) : null, h("span", { text: label })]);

  // ---------- 움직임 ----------
  function reduced() { return !!(root.matchMedia && root.matchMedia("(prefers-reduced-motion: reduce)").matches); }
  const easeOut = (t) => 1 - Math.pow(1 - t, 3);
  // 숫자 카운트업(첫 등장에만). 움직임 줄이기·같은 값이면 바로 바꾼다.
  function countTo(el, to, o) {
    o = o || {};
    const from = typeof el._n === "number" ? el._n : o.from === undefined ? 0 : o.from;
    el._n = to;
    const fmt = o.format || String;
    if (!isFinite(to) || reduced() || from === to || !root.requestAnimationFrame || to - from > 1e6) { el.textContent = fmt(to); return; }
    const t0 = root.performance.now(), dur = o.duration || 480;
    el.textContent = fmt(from);
    const step = (t) => {
      const p = Math.min(1, (t - t0) / dur);
      el.textContent = fmt(Math.round(from + (to - from) * easeOut(p)));
      if (p < 1 && el._n === to) root.requestAnimationFrame(step);
      else el.textContent = fmt(el._n);
    };
    root.requestAnimationFrame(step);
  }
  function debounce(fn, ms) { let t = null; return function () { const a = arguments, self = this; if (t) clearTimeout(t); t = setTimeout(() => { t = null; fn.apply(self, a); }, ms); }; }
  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
  // 스태거 순번: 앞 몇 개만 차례로, 나머지는 같은 시각에(CSS가 --i로 min(i, 6) × 40ms 지연을 준다).
  function stagger(el, i) { if (el.style && el.style.setProperty) el.style.setProperty("--i", String(Math.min(i, 8))); return el; }

  // ---------- 서식·시계 ----------
  // 시계: 서버 시각과의 차이를 api.js가 알려 준다(승인 만료·경과 시간이 내 시계가 틀려도 맞게).
  let skew = 0;
  const now = () => Date.now() + skew;
  const setSkew = (ms) => { skew = ms; };
  // 시각은 보는 사람의 시간대로 보인다(시간대 이름을 함께).
  const STAMP = new Intl.DateTimeFormat("ko-KR", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false, timeZoneName: "short" });
  const when = (ms) => (ms ? STAMP.format(new Date(ms)) : "-");
  // 좁은 머리글용 짧은 시각(시:분:초)
  const CLOCK = new Intl.DateTimeFormat("ko-KR", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  const clock = (ms) => (ms ? CLOCK.format(new Date(ms)) : "-");
  function span(secs) {
    const a = Math.abs(secs);
    return a < 90 ? a + "초" : a < 5400 ? Math.round(a / 60) + "분" : a < 172800 ? Math.round(a / 3600) + "시간" : Math.round(a / 86400) + "일";
  }
  // 서버 시각과의 차이를 초 단위(Date 머리글)로만 알 수 있어, 몇 초 안쪽은 «방금»으로 뭉뚱그린다.
  function ago(ms, at) {
    if (!ms) return "-";
    const s = Math.round((ms - (at === undefined ? now() : at)) / 1000);
    if (Math.abs(s) < 5) return "방금";
    return s >= 0 ? span(s) + " 뒤" : span(s) + " 전";
  }
  const uptime = (secs) => span(secs);
  // 남은 시간: 지났으면 «만료됨».
  function remain(ms, at) {
    const s = Math.round((ms - (at === undefined ? now() : at)) / 1000);
    return s <= 0 ? "만료됨" : span(s) + " 남음";
  }
  // 흐르는 시간 글자: 같은 요소를 tick()이 1초마다 다시 쓴다(데이터가 그대로여도 «5분 전»이 낡지 않게).
  function timeEl(ms, o) {
    o = o || {};
    const el = h("time", { class: "ago" + (o.cls ? " " + o.cls : ""), "data-ms": String(ms || 0), title: when(ms) });
    el.textContent = ms ? ago(ms) : "-";
    return el;
  }
  function untilEl(ms, o) {
    o = o || {};
    const el = h("span", { class: "until" + (o.cls ? " " + o.cls : ""), "data-until": String(ms || 0), title: when(ms) });
    el.textContent = remain(ms);
    return el;
  }
  function tick() {
    const at = now();
    Array.prototype.forEach.call(document.querySelectorAll("time.ago[data-ms]"), (el) => {
      const ms = Number(el.getAttribute("data-ms")), t = ms ? ago(ms, at) : "-";
      if (el.textContent !== t) el.textContent = t;
    });
    Array.prototype.forEach.call(document.querySelectorAll("[data-until]"), (el) => {
      const t = remain(Number(el.getAttribute("data-until")), at);
      if (el.textContent !== t) el.textContent = t;
      el.classList.toggle("is-over", t === "만료됨");
    });
  }

  // ---------- 표·빈 상태·구역 갱신 ----------
  const isNode = (v) => typeof Node !== "undefined" && v instanceof Node;
  const cellOf = (tag, v, cls) => { const el = h(tag, cls ? { class: cls } : null); el.appendChild(isNode(v) ? v : text(v === null || v === undefined || v === "" ? "-" : v)); return el; };
  // 빈 상태 안내: 동그란 아이콘 + 제목 + 보조 설명
  function empty(title, hint, ic) {
    return h("div", { class: "empty" }, [h("span", { class: "empty-ic" }, [icon(ic || "inbox", { size: 18 })]), h("div", { class: "empty-tx" }, [h("p", { class: "empty-t", text: title }), hint ? h("p", { class: "empty-h", text: hint }) : null])]);
  }
  // 표: head(머리글 글자), rows(각 행은 칸 배열, {cells, cls}면 행에 클래스, {group, n}이면 묶음 머리 행),
  // o: { num: [오른쪽 맞춤 열], narrow: [줄이는 열], emptyTitle, emptyHint, emptyIcon, cls, label }.
  function table(head, rows, o) {
    o = o || {};
    if (!rows.length) return empty(o.emptyTitle || "없음", o.emptyHint, o.emptyIcon);
    const num = o.num || [], shr = o.narrow || [];
    const cls = (i) => (num.indexOf(i) >= 0 ? "num" : "") + (shr.indexOf(i) >= 0 ? " shr" : "");
    const thead = h("thead", null, [h("tr", null, head.map((t, i) => h("th", { scope: "col", class: cls(i), text: t })))]);
    const body = h("tbody", null, rows.map((r) => {
      if (Array.isArray(r)) return h("tr", null, r.map((v, i) => cellOf("td", v, cls(i))));
      if (r.cells) return h("tr", r.cls ? { class: r.cls } : null, r.cells.map((v, i) => cellOf("td", v, cls(i))));
      return h("tr", { class: "grp" }, [h("th", { colspan: head.length, scope: "colgroup" }, [h("span", { text: r.group }), r.n !== undefined ? h("em", { text: String(r.n) }) : null])]);
    }));
    // 좁은 화면에서 가로로 밀리는 표는 키보드로도 닿아야 하므로, 이름을 준 표는 포커스를 받는 영역으로 둔다.
    return h("div", { class: "tbl" + (o.cls ? " " + o.cls : ""), role: o.label ? "region" : null, tabindex: o.label ? "0" : null, "aria-label": o.label || null }, [h("table", null, [thead, body])]);
  }
  // 구역 갱신: list의 각 항목 { key, sig, build(isNew) → { node, cells } }. 같은 sig면 그 노드를 그대로 두고, 바뀌면 새로 만든 노드로 바꾸되
  // cells(키 → 요소)에서 글자가 바뀐 것만 .flash로 잠깐 강조한다. 첫 등장(스태거)과 갱신 강조를 구분하려고 memo에 이전 상태를 둔다.
  function patch(host, list, memo, o) {
    o = o || {};
    const keep = {};
    let prev = null;
    list.forEach((it, i) => {
      keep[it.key] = true;
      const old = memo[it.key];
      if (old && old.sig === it.sig) { prev = old.node; return; }
      const built = it.build(!old);
      built.node.setAttribute("data-sec", it.key);
      if (old) {
        Object.keys(built.cells || {}).forEach((k) => {
          const a = old.cells && old.cells[k], b = built.cells[k];
          if (a && b && a.textContent !== b.textContent && b.classList) b.classList.add("flash");
        });
        host.replaceChild(built.node, old.node);
      } else {
        if (o.stagger && built.node.classList) { built.node.classList.add("rise"); stagger(built.node, i); }
        const ref = prev ? prev.nextSibling : host.firstChild;
        if (ref) host.insertBefore(built.node, ref); else host.appendChild(built.node);
      }
      memo[it.key] = { sig: it.sig, node: built.node, cells: built.cells || {} };
      prev = built.node;
    });
    Object.keys(memo).forEach((k) => { if (!keep[k]) { if (memo[k].node.parentNode) memo[k].node.parentNode.removeChild(memo[k].node); delete memo[k]; } });
  }
  // 순서가 있는 목록 갱신: patch와 같은 항목 모양이지만 서버가 준 순서를 그대로 지킨다(바뀐 항목은 새 노드로, 새 항목은 rise로).
  function sync(host, list, memo, o) {
    o = o || {};
    const keep = {};
    list.forEach((it, i) => {
      keep[it.key] = true;
      const old = memo[it.key];
      let node;
      if (old && old.sig === it.sig) node = old.node;
      else {
        const built = it.build(!old);
        node = built.node;
        if (!old && o.stagger && node.classList) { node.classList.add("rise"); stagger(node, i); }
        if (old && old.node.parentNode === host) host.removeChild(old.node);
        memo[it.key] = { sig: it.sig, node, cells: built.cells || {} };
      }
      const at = host.children[i] || null;
      if (at !== node) host.insertBefore(node, at);
    });
    Object.keys(memo).forEach((k) => { if (!keep[k]) { if (memo[k].node.parentNode) memo[k].node.parentNode.removeChild(memo[k].node); delete memo[k]; } });
  }

  // 불러오는 동안의 뼈대(막대 몇 줄). 글자는 화면 읽기용으로만 둔다.
  function skeleton(n, cls) {
    const rows = [];
    for (let i = 0; i < (n || 3); i++) rows.push(stagger(h("div", { class: "sk-bar" + (i % 3 === 2 ? " short" : "") }), i));
    return h("div", { class: "skeleton " + (cls || ""), role: "status", "aria-busy": "true" }, [h("p", { class: "sr", text: "불러오는 중…" })].concat(rows));
  }

  const api = { $, h, svg, text, code, clear, append, icon, dot, pill, reduced, countTo, debounce, clamp, stagger, now, setSkew, when, clock, ago, remain, uptime, timeEl, untilEl, tick, skeleton, empty, table, patch, sync, cellOf, NS };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotDom = api;
})(typeof window !== "undefined" ? window : globalThis);
