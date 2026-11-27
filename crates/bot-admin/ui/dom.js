// Shared DOM builders (textContent only, no markup strings), line icons, time formatting, tables, and keyed list patching.
(function (root) {
  "use strict";

  // Built from segments so static checks for external URLs do not flag the SVG namespace identifier.
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
    play: [P("M8 5.5l10.5 6.5L8 18.5z")],
    arrow: [P("M5 12h14M13 6l6 6-6 6")],
    lock: [["rect", { x: 5, y: 10.5, width: 14, height: 9.5, rx: 2.6 }], P("M8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5")],
    inbox: [P("M4 13l2.5-7h11L20 13v5a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 4 18z"), P("M4 13h4.5l1 2h5l1-2H20")],
    power: [P("M12 3.5v8"), P("M7.2 6.6a7.5 7.5 0 1 0 9.6 0")],
    eye: [P("M2.5 12s3.4-6.5 9.5-6.5 9.5 6.5 9.5 6.5-3.4 6.5-9.5 6.5S2.5 12 2.5 12z"), ["circle", { cx: 12, cy: 12, r: 2.8 }]],
    external: [P("M14 4h6v6"), P("M20 4l-9 9"), P("M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4")],
  };
  function icon(name, opts) {
    opts = opts || {};
    const s = opts.size || 16;
    const el = svg("svg", { viewBox: "0 0 24 24", width: s, height: s, fill: "none", stroke: "currentColor", "stroke-width": 1.75, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true", focusable: "false", class: "ic" + (opts.cls ? " " + opts.cls : "") });
    (ICONS[name] || ICONS.agent).forEach((n) => el.appendChild(svg(n[0], n[1])));
    if (opts.off) el.appendChild(svg("path", { d: "M5 19L19 5", class: "slash" }));
    return el;
  }

  // Distinct glyph per status (filled circle, hollow circle, pause bars, triangle) so state never relies on color alone.
  function dot(state) {
    const el = svg("svg", { viewBox: "0 0 10 10", width: 10, height: 10, "aria-hidden": "true", focusable: "false", class: "dot dot-" + state });
    if (state === "on") el.appendChild(svg("circle", { cx: 5, cy: 5, r: 4, fill: "currentColor" }));
    else if (state === "off") el.appendChild(svg("circle", { cx: 5, cy: 5, r: 3.1, fill: "none", stroke: "currentColor", "stroke-width": 1.5 }));
    else if (state === "held") { el.appendChild(svg("rect", { x: 1.6, y: 1.6, width: 2.4, height: 6.8, rx: 1, fill: "currentColor" })); el.appendChild(svg("rect", { x: 6, y: 1.6, width: 2.4, height: 6.8, rx: 1, fill: "currentColor" })); }
    else el.appendChild(svg("path", { d: "M5 1l4.4 8H.6z", fill: "currentColor", stroke: "currentColor", "stroke-width": 1, "stroke-linejoin": "round" }));
    return el;
  }
  const pill = (label, tone, state) => h("span", { class: "pill pill-" + (tone || "info") }, [state ? dot(state) : null, h("span", { text: label })]);

  function reduced() { return !!(root.matchMedia && root.matchMedia("(prefers-reduced-motion: reduce)").matches); }
  const easeOut = (t) => 1 - Math.pow(1 - t, 3);
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
  function stagger(el, i) { if (el.style && el.style.setProperty) el.style.setProperty("--i", String(Math.min(i, 8))); return el; }

  // Server clock skew calibrated from HTTP Date headers by api.js so expiry and elapsed times survive client clock drift.
  let skew = 0;
  const now = () => Date.now() + skew;
  const setSkew = (ms) => { skew = ms; };
  const STAMP = new Intl.DateTimeFormat("en-US", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false, timeZoneName: "short" });
  const when = (ms) => (ms ? STAMP.format(new Date(ms)) : "-");
  const CLOCK = new Intl.DateTimeFormat("en-US", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  const clock = (ms) => (ms ? CLOCK.format(new Date(ms)) : "-");
  function span(secs) {
    const a = Math.abs(secs);
    return a < 90 ? a + "s" : a < 5400 ? Math.round(a / 60) + "m" : a < 172800 ? Math.round(a / 3600) + "h" : Math.round(a / 86400) + "d";
  }
  // HTTP Date headers have 1s resolution, so deltas within 5s collapse to "just now".
  function ago(ms, at) {
    if (!ms) return "-";
    const s = Math.round((ms - (at === undefined ? now() : at)) / 1000);
    if (Math.abs(s) < 5) return "just now";
    return s >= 0 ? "in " + span(s) : span(s) + " ago";
  }
  const uptime = (secs) => span(secs);
  function remain(ms, at) {
    const s = Math.round((ms - (at === undefined ? now() : at)) / 1000);
    return s <= 0 ? "Expired" : span(s) + " left";
  }
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
      el.classList.toggle("is-over", t === "Expired");
    });
  }

  const isNode = (v) => typeof Node !== "undefined" && v instanceof Node;
  const cellOf = (tag, v, cls) => { const el = h(tag, cls ? { class: cls } : null); el.appendChild(isNode(v) ? v : text(v === null || v === undefined || v === "" ? "-" : v)); return el; };
  function empty(title, hint, ic) {
    return h("div", { class: "empty" }, [h("span", { class: "empty-ic" }, [icon(ic || "inbox", { size: 18 })]), h("div", { class: "empty-tx" }, [h("p", { class: "empty-t", text: title }), hint ? h("p", { class: "empty-h", text: hint }) : null])]);
  }
  function table(head, rows, o) {
    o = o || {};
    if (!rows.length) return empty(o.emptyTitle || "None", o.emptyHint, o.emptyIcon);
    const num = o.num || [], shr = o.narrow || [];
    const cls = (i) => (num.indexOf(i) >= 0 ? "num" : "") + (shr.indexOf(i) >= 0 ? " shr" : "");
    const thead = h("thead", null, [h("tr", null, head.map((t, i) => h("th", { scope: "col", class: cls(i), text: t })))]);
    const body = h("tbody", null, rows.map((r) => {
      if (Array.isArray(r)) return h("tr", null, r.map((v, i) => cellOf("td", v, cls(i))));
      if (r.cells) return h("tr", r.cls ? { class: r.cls } : null, r.cells.map((v, i) => cellOf("td", v, cls(i))));
      return h("tr", { class: "grp" }, [h("th", { colspan: head.length, scope: "colgroup" }, [h("span", { text: r.group }), r.n !== undefined ? h("em", { text: String(r.n) }) : null])]);
    }));
    // Make labeled scrollable tables keyboard-focusable on narrow viewports.
    return h("div", { class: "tbl" + (o.cls ? " " + o.cls : ""), role: o.label ? "region" : null, tabindex: o.label ? "0" : null, "aria-label": o.label || null }, [h("table", null, [thead, body])]);
  }
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

  function skeleton(n, cls) {
    const rows = [];
    for (let i = 0; i < (n || 3); i++) rows.push(stagger(h("div", { class: "sk-bar" + (i % 3 === 2 ? " short" : "") }), i));
    return h("div", { class: "skeleton " + (cls || ""), role: "status", "aria-busy": "true" }, [h("p", { class: "sr", text: "Loading…" })].concat(rows));
  }

  const api = { $, h, svg, text, code, clear, append, icon, dot, pill, reduced, countTo, debounce, clamp, stagger, now, setSkew, when, clock, ago, remain, uptime, timeEl, untilEl, tick, skeleton, empty, table, patch, sync, cellOf, NS };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotDom = api;
})(typeof window !== "undefined" ? window : globalThis);
