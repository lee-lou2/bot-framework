// Interactive structure map: renders BotLayout geometry, camera pan/zoom, hover/selection highlighting, search, and filters.
(function (root) {
  "use strict";
  const D = root.BotDom, L = root.BotLayout, P = root.BotPanel, CAM = root.BotCamera, N = root.BotNeeds;
  const { h, svg, icon, clear } = D;
  const PANEL_W = 380;

  function create(o) {
    const els = o.els;
    const st = {
      nodes: [], idx: null, layout: null, cam: { x: 0, y: 0, k: 1 }, vp: { w: 0, h: 0 }, hidden: {}, hideOff: false, query: "", matches: {},
      selected: null, hover: null, byKey: {}, cardEl: {}, labelEl: {}, edgeEls: [], popEls: [], common: {}, moved: false, entered: false, visible: false, dirty: true,
      pointers: {}, drag: null, pinch: null, suppressClick: false, animT: 0, swapT: 0, searchT: 0, needsRaf: 0,
    };
    const panel = P.createPanel({ el: els.detail, canGo: (k) => canGo(k), onNavigate: (k) => select(k), onClose: () => deselect() });
    const dock = P.createDock({
      el: els.dock,
      makeChip: (it) => { const b = P.chip(it); st.popEls.push({ key: it.key, el: b }); return b; },
      onOpen: () => { paint(); redrawNeeds(); },
      onClose: () => { st.popEls = []; redrawNeeds(); },
    });
    const needs = N.create({ host: els.view, idx: () => st.idx, rect: (k) => needsRect(k) });

    function measure() {
      st.vp = { w: els.map.clientWidth || 0, h: els.map.clientHeight || 0 };
      return st.vp.w > 0 && st.vp.h > 0;
    }
    const narrow = () => st.vp.w <= 720;
    function insets(withPanel) {
      const bar = els.bar.offsetHeight || 40, dk = els.dock.offsetHeight || 0;
      const side = narrow() ? 8 : 24;
      const ins = { top: 12 + bar + 16, bottom: 12 + (dk || 0) + 16, left: side, right: side };
      if (withPanel) {
        if (narrow()) ins.bottom += Math.min(els.detail.offsetHeight || 280, st.vp.h * 0.62) + 8;
        else ins.right += PANEL_W + 4;
      }
      return ins;
    }
    const panelInset = () => (st.selected && !narrow() ? PANEL_W + 16 : 0);

    function applyCam(animate) {
      const s = els.stage;
      if (animate && !D.reduced()) {
        s.classList.add("anim");
        clearTimeout(st.animT);
        st.animT = setTimeout(() => s.classList.remove("anim"), 460);
      } else s.classList.remove("anim");
      s.style.transform = "translate(" + st.cam.x + "px, " + st.cam.y + "px) scale(" + st.cam.k + ")";
      trackNeeds(animate);
    }
    function liveCam() {
      const m = els.stage.classList.contains("anim") && root.getComputedStyle ? /matrix\(([^)]+)\)/.exec(root.getComputedStyle(els.stage).transform || "") : null;
      if (!m) return st.cam;
      const v = m[1].split(",").map(Number);
      return { k: v[0], x: v[4], y: v[5] };
    }
    function freeze() {
      if (!els.stage.classList.contains("anim")) return;
      const c = liveCam();
      els.stage.classList.remove("anim");
      st.cam = c;
      applyCam(false);
    }
    const limit = (cam) => (st.layout ? CAM.limitCamera(cam, st.layout.bounds, st.vp) : cam);
    function fit(animate) {
      if (!st.layout || !measure()) return;
      st.cam = CAM.defaultCamera(st.layout, st.vp, insets(false));
      st.moved = false;
      applyCam(animate);
    }
    function moveTo(cam, animate) { st.cam = cam; st.moved = true; applyCam(animate); }
    function zoomBy(factor, cx, cy) {
      freeze();
      const c = cx === undefined ? { x: st.vp.w / 2, y: st.vp.h / 2 } : { x: cx, y: cy };
      moveTo(limit(CAM.zoomAt(st.cam, factor, c.x, c.y)), false);
    }
    function reveal(el) {
      const vr = els.map.getBoundingClientRect(), r = el.getBoundingClientRect();
      const next = CAM.revealCamera(st.cam, { x: r.left - vr.left, y: r.top - vr.top, w: r.width, h: r.height }, st.vp, insets(!!st.selected), 16);
      if (next) moveTo(next, true);
    }

    const viewRect = (el) => { const r = el.getBoundingClientRect(), v = els.view.getBoundingClientRect(); return { x: r.left - v.left, y: r.top - v.top, w: r.width, h: r.height }; };
    function needsRect(key) {
      const r = st.layout && st.layout.rects[key];
      if (r) { const m = viewRect(els.map), c = liveCam(); return { x: m.x + c.x + r.x * c.k, y: m.y + c.y + r.y * c.k, w: r.w * c.k, h: r.h * c.k, group: key }; }
      const pop = st.popEls.filter((p) => p.key === key && p.el.parentNode)[0];
      if (pop) return Object.assign(viewRect(pop.el), { group: key });
      const c = st.idx && st.idx.byKey[key], pill = c && dock.pill(c.kind);
      return pill ? Object.assign(viewRect(pill), { group: "pill:" + c.kind }) : null;
    }
    const redrawNeeds = () => needs.draw(st.selected);
    // Redraw overlay lines across the 460ms camera/dock transition so curves stay attached.
    function trackNeeds(animate) {
      if (st.needsRaf && root.cancelAnimationFrame) root.cancelAnimationFrame(st.needsRaf);
      st.needsRaf = 0;
      if (!st.selected) { needs.clear(); return; }
      if (!animate || D.reduced() || !root.requestAnimationFrame) { redrawNeeds(); return; }
      const until = root.performance.now() + 480;
      const step = () => { redrawNeeds(); st.needsRaf = root.performance.now() < until ? root.requestAnimationFrame(step) : 0; };
      step();
    }

    function register(key, el) { (st.byKey[key] = st.byKey[key] || []).push(el); }
    function buildCard(c, i, entrance) {
      const el = h("div", { class: "card" + (c.root ? " root" : "") + (entrance ? " enter" : ""), "data-key": c.key, role: "group", "aria-label": "Agent " + c.name });
      el.style.left = c.x + "px"; el.style.top = c.y + "px"; el.style.width = c.w + "px"; el.style.height = c.h + "px";
      if (entrance) D.stagger(el, i);
      const counts = c.counts.map((x) => L.KIND_LABEL[x.kind] + " " + x.n).join(" · ");
      el.appendChild(h("h3", { class: "card-title" }, [h("button", { type: "button", class: "card-btn", "data-key": c.key, title: c.name }, [h("span", { class: "tile" }, [icon("agent", { size: 16 })]), h("span", { class: "card-name", text: c.name }), counts ? h("span", { class: "card-counts", text: counts }) : null])]));
      c.dividers.forEach((y) => { const d = h("div", { class: "divider", "aria-hidden": "true" }); d.style.top = y + "px"; el.appendChild(d); });
      c.groups.forEach((g) => g.chips.forEach((ch) => {
        const b = P.chip(ch);
        b.style.left = ch.x + "px"; b.style.top = ch.y + "px"; b.style.width = ch.w + "px"; b.style.height = ch.h + "px";
        register(ch.key, b);
        el.appendChild(b);
      }));
      if (c.empty) el.appendChild(h("p", { class: "card-empty", text: "No items to display" }));
      st.cardEl[c.key] = el;
      return el;
    }
    function renderStage(entrance) {
      const lay = st.layout, stage = els.stage;
      clear(stage);
      st.byKey = {}; st.cardEl = {}; st.labelEl = {}; st.edgeEls = []; st.common = {};
      stage.style.width = lay.width + "px"; stage.style.height = lay.height + "px";
      const sv = svg("svg", { class: "edges", width: lay.width, height: lay.height, viewBox: "0 0 " + lay.width + " " + lay.height, "aria-hidden": "true", focusable: "false" });
      lay.edges.forEach((e) => {
        const path = svg("path", { class: "edge", d: e.d }), dots = [svg("circle", { class: "edge-dot", cx: e.start[0], cy: e.start[1], r: 3.5 }), svg("circle", { class: "edge-dot", cx: e.end[0], cy: e.end[1], r: 3.5 })];
        sv.appendChild(path); dots.forEach((d) => sv.appendChild(d));
        st.edgeEls.push({ from: e.from, to: e.to, els: [path].concat(dots) });
      });
      stage.appendChild(sv);
      lay.profiles.forEach((p) => {
        const inner = [icon("profile", { size: 14 }), h("span", { text: "Profile " + p.name })];
        const tag = h("div", { class: "plabel" }, [p.key ? h("button", { type: "button", class: "plabel-btn", "data-key": p.key, title: "Profile " + p.name + " — click for details" }, inner) : h("span", { class: "plabel-btn" }, inner)]);
        tag.style.left = p.x + "px"; tag.style.top = p.y + 4 + "px"; tag.style.width = p.w + "px";
        if (p.key) st.labelEl[p.key] = tag;
        stage.appendChild(tag);
      });
      lay.cards.forEach((c, i) => stage.appendChild(buildCard(c, i, entrance)));
      lay.commons.forEach((g) => g.items.forEach((c) => { st.common[L.key(c)] = true; }));
      if (lay.empty) stage.appendChild(h("p", { class: "card-empty", text: "No structure to display" }));
    }
    const canGo = (k) => !!(st.layout && (st.layout.rects[k] || st.common[k]));

    function relayout(o2) {
      o2 = o2 || {};
      if (!st.idx) return;
      if (!measure()) { st.dirty = true; return; }
      st.dirty = false;
      st.layout = L.build(st.nodes, { viewport: st.vp, insets: insets(false), hidden: st.hidden, hideOff: st.hideOff });
      renderStage(!!o2.entrance);
      dock.update(st.layout.commons, !!o2.entrance);
      if (st.selected && !canGo(st.selected)) { st.selected = null; panel.hide(); els.view.style.setProperty("--inset", "0px"); }
      refreshMatches();
      paint();
      if (o2.fit) fit(false);
      else applyCam(false);
      st.entered = true;
    }
    function swap() {
      clearTimeout(st.swapT);
      if (D.reduced()) { relayout({ fit: !st.moved && !st.selected }); return; }
      els.stage.classList.add("swap");
      st.swapT = setTimeout(() => { relayout({ fit: !st.moved && !st.selected }); els.stage.classList.remove("swap"); }, 160);
    }

    function visibleNode(c) { return !st.hidden[c.kind] && !(st.hideOff && L.isOff(c)); }
    function refreshMatches() {
      st.matches = {};
      if (!st.query.trim()) { hitsText(); return; }
      const m = L.matches(st.nodes.filter(visibleNode), st.query);
      Object.keys(m).forEach((k) => { if (canGo(k)) st.matches[k] = true; });
      hitsText();
    }
    function hitsText() {
      const n = Object.keys(st.matches).length;
      els.hits.textContent = st.query.trim() ? (n ? n + (n === 1 ? " match" : " matches") : "No matches") : "";
    }
    // Selection takes precedence over hover so passing pointer events do not flicker the active focus set.
    function focusSet() {
      const k = st.selected || st.hover;
      if (k) return L.related(st.idx, k);
      const keys = Object.keys(st.matches);
      if (!keys.length) return null;
      const s = {};
      keys.forEach((m) => { s[m] = true; const c = st.idx.byKey[m], o = c && L.ownerKey(c); if (o) s[o] = true; if (c) s[c.profile + "|profile:" + c.profile] = true; });
      return s;
    }
    function paint() {
      if (!st.layout) return;
      const set = focusSet(), sel = st.selected;
      st.layout.cards.forEach((c) => {
        const el = st.cardEl[c.key];
        const hot = !!set && (!!set[c.key] || c.groups.some((g) => g.chips.some((ch) => set[ch.key])));
        el.classList.toggle("is-dim", !!set && !hot);
        el.classList.toggle("is-hot", hot);
        el.classList.toggle("is-selected", sel === c.key);
      });
      Object.keys(st.labelEl).forEach((k) => {
        const el = st.labelEl[k];
        el.classList.toggle("is-dim", !!set && !set[k]);
        el.classList.toggle("is-selected", sel === k);
        el.classList.toggle("is-match", !!st.matches[k]);
      });
      const chips = [];
      Object.keys(st.byKey).forEach((k) => st.byKey[k].forEach((el) => chips.push({ key: k, el })));
      st.popEls.forEach((p) => chips.push(p));
      chips.forEach((p) => {
        p.el.classList.toggle("is-dim", !!set && !set[p.key]);
        p.el.classList.toggle("is-selected", sel === p.key);
        p.el.classList.toggle("is-match", !!st.matches[p.key]);
        if (sel === p.key) p.el.setAttribute("aria-current", "true"); else p.el.removeAttribute("aria-current");
      });
      st.edgeEls.forEach((e) => {
        const hot = !!set && !!set[e.from] && !!set[e.to];
        e.els.forEach((x) => { x.classList.toggle("is-hot", hot); x.classList.toggle("is-dim", !!set && !hot); });
      });
      dock.mark(sel ? set : Object.keys(st.matches).length ? st.matches : null);
    }
    function keyOf(node) {
      for (let n = node; n && n !== els.view; n = n.parentNode) {
        const k = n.getAttribute && n.getAttribute("data-key");
        if (k) return k;
      }
      return null;
    }

    function focusSelected(animate) {
      const k = st.selected;
      if (!k || !st.layout || !measure()) return;
      let box = null, o = { kMax: 1.3 };
      const r = st.layout.rects[k];
      // On narrow viewports the bottom sheet covers part of the map, so focus the chip itself to keep text readable.
      if (r) { box = narrow() ? r : st.layout.rects[r.card]; if (narrow()) o = { kMax: 1.2, kMin: CAM.Z.READABLE, margin: 24 }; }
      else {
        const set = L.related(st.idx, k);
        box = CAM.union(Object.keys(set).filter((x) => st.layout.rects[x] && st.layout.rects[x].card === x && st.idx.byKey[x].kind === "agent").map((x) => st.layout.rects[x]));
        o = { kMax: 1.1 };
      }
      if (box) moveTo(CAM.focusCamera(box, st.vp, insets(true), o), animate);
    }
    function select(key, opts) {
      opts = opts || {};
      if (!st.idx || !st.idx.byKey[key] || !canGo(key)) return;
      st.selected = key;
      panel.show(st.idx, key);
      els.view.style.setProperty("--inset", panelInset() + "px");
      if (!opts.noCamera) focusSelected(true);
      hideHint();
      paint();
      trackNeeds(true);
    }
    function deselect() {
      if (!st.selected) return;
      st.selected = null;
      panel.hide();
      els.view.style.setProperty("--inset", "0px");
      needs.clear();
      paint();
      fit(true);
    }
    function hideHint() { els.hint.classList.add("gone"); }

    function setQuery(q) {
      st.query = q;
      refreshMatches();
      paint();
      clearTimeout(st.searchT);
      st.searchT = setTimeout(() => {
        if (!st.query.trim()) { if (!st.selected) fit(true); return; }
        const box = CAM.union(Object.keys(st.matches).map((k) => st.layout.rects[k] && st.layout.rects[st.layout.rects[k].card]).filter(Boolean));
        if (box) moveTo(CAM.focusCamera(box, st.vp, insets(!!st.selected), { kMax: 1.15, margin: 32 }), true);
      }, 220);
    }
    function buildLegend() {
      clear(els.kinds);
      const items = L.legend(st.nodes);
      items.forEach((e) => {
        els.kinds.appendChild(h("button", { type: "button", class: "lg", "data-kind": e.kind, "aria-pressed": String(!st.hidden[e.kind]), title: e.label + " (" + e.total + ") — click to toggle", onclick: () => { st.hidden[e.kind] = !st.hidden[e.kind]; buildLegend(); swap(); } }, [icon(e.kind, { size: 14 }), h("span", { class: "nm", text: e.label }), h("span", { class: "n", text: String(e.total) })]));
      });
      const off = items.reduce((n, e) => n + e.off, 0);
      if (off) {
        els.kinds.appendChild(h("span", { class: "lg-sep", "aria-hidden": "true" }));
        els.kinds.appendChild(h("button", { type: "button", class: "lg", "data-kind": "off", "aria-pressed": String(!st.hideOff), title: off + " disallowed scope(s) — click to toggle", onclick: () => { st.hideOff = !st.hideOff; buildLegend(); swap(); } }, [icon("power", { size: 14 }), h("span", { class: "nm", text: "Disallowed" }), h("span", { class: "n", text: String(off) })]));
      }
    }

    const rel = (p) => { const r = els.map.getBoundingClientRect(); return { x: p.x - r.left, y: p.y - r.top }; };
    const pt = (e) => rel({ x: e.clientX, y: e.clientY });
    function onDown(e) {
      if (e.pointerType === "mouse" && e.button !== 0) return;
      freeze();
      st.pointers[e.pointerId] = { x: e.clientX, y: e.clientY };
      const ids = Object.keys(st.pointers);
      if (ids.length === 1) st.drag = { x: e.clientX, y: e.clientY, cam: Object.assign({}, st.cam), moved: false };
      else if (ids.length === 2) {
        const a = rel(st.pointers[ids[0]]), b = rel(st.pointers[ids[1]]);
        st.pinch = { dist: Math.hypot(a.x - b.x, a.y - b.y) || 1, mid: { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 }, cam: Object.assign({}, st.cam) };
        st.drag = null; st.suppressClick = true;
      }
      hideHint();
    }
    function onMove(e) {
      if (!st.pointers[e.pointerId]) return;
      st.pointers[e.pointerId] = { x: e.clientX, y: e.clientY };
      if (st.pinch) {
        const ids = Object.keys(st.pointers);
        if (ids.length >= 2) moveTo(limit(CAM.pinchCamera(st.pinch, { a: rel(st.pointers[ids[0]]), b: rel(st.pointers[ids[1]]) })), false);
        return;
      }
      const d = st.drag;
      if (!d) return;
      const dx = e.clientX - d.x, dy = e.clientY - d.y;
      if (!d.moved && !CAM.isDrag(dx, dy)) return;
      if (!d.moved) { d.moved = true; st.suppressClick = true; els.map.classList.add("dragging"); if (els.map.setPointerCapture) { try { els.map.setPointerCapture(e.pointerId); } catch (x) { /* pointer already ended */ } } }
      moveTo(limit({ k: d.cam.k, x: d.cam.x + dx, y: d.cam.y + dy }), false);
    }
    function onUp(e) {
      delete st.pointers[e.pointerId];
      if (!Object.keys(st.pointers).length) { st.drag = null; st.pinch = null; els.map.classList.remove("dragging"); }
      else if (Object.keys(st.pointers).length === 1) { st.pinch = null; }
      // Suppress the synthetic click that immediately follows a drag gesture.
      if (st.suppressClick) setTimeout(() => { st.suppressClick = false; }, 0);
    }
    function onWheel(e) {
      e.preventDefault();
      freeze();
      moveTo(limit(CAM.wheelCamera(st.cam, { dx: e.deltaX, dy: e.deltaY, ctrl: e.ctrlKey, meta: e.metaKey, shift: e.shiftKey, mode: e.deltaMode }, pt(e))), false);
      hideHint();
    }
    function onKey(e) {
      if (e.key === "Escape") {
        if (dock.openKind) { dock.close(); paint(); } else if (st.selected) deselect(); else if (st.query) { els.query.value = ""; setQuery(""); }
        return;
      }
      if (e.target !== els.map) return;
      freeze();
      const next = CAM.keyCamera(st.cam, e.key, st.vp);
      if (!next) return;
      e.preventDefault();
      if (next === "fit") fit(true); else moveTo(limit(next), e.key.indexOf("Arrow") === 0);
    }

    function bind() {
      const m = els.map;
      m.addEventListener("pointerdown", onDown);
      m.addEventListener("pointermove", onMove);
      m.addEventListener("pointerup", onUp);
      m.addEventListener("pointercancel", onUp);
      m.addEventListener("wheel", onWheel, { passive: false });
      m.addEventListener("dblclick", (e) => { if (!keyOf(e.target)) { if (st.selected) deselect(); else fit(true); } });
      m.addEventListener("scroll", () => { m.scrollLeft = 0; m.scrollTop = 0; });
      els.view.addEventListener("keydown", onKey);
      const over = (e) => { if (e.pointerType === "touch") return; const k = keyOf(e.target); if (k !== st.hover) { st.hover = k; paint(); } };
      const leave = () => { if (st.hover) { st.hover = null; paint(); } };
      els.stage.addEventListener("pointerover", over); els.stage.addEventListener("pointerleave", leave);
      els.dock.addEventListener("pointerover", over); els.dock.addEventListener("pointerleave", leave);
      els.view.addEventListener("click", (e) => {
        if (st.suppressClick) { st.suppressClick = false; return; }
        const k = keyOf(e.target);
        if (k) { if (canGo(k)) select(k); return; }
        for (let n = e.target; n; n = n.parentNode) { if (n === els.map) { deselect(); return; } }
      });
      els.view.addEventListener("pointerdown", (e) => { if (dock.openKind && !(els.dock.contains && els.dock.contains(e.target))) { dock.close(); paint(); } });
      els.stage.addEventListener("focusin", (e) => { if (e.target.matches && e.target.matches(":focus-visible")) reveal(e.target); });
      els.query.addEventListener("input", (e) => setQuery(e.target.value));
      els.query.addEventListener("keydown", (e) => {
        if (e.key === "Enter") { const first = Object.keys(st.matches)[0]; if (first) select(first); }
      });
      els.fit.addEventListener("click", () => { if (st.selected) { st.selected = null; panel.hide(); els.view.style.setProperty("--inset", "0px"); paint(); } fit(true); });
      els.zin.addEventListener("click", () => zoomBy(CAM.Z.BTN_ZOOM));
      els.zout.addEventListener("click", () => zoomBy(1 / CAM.Z.BTN_ZOOM));
      const settle = () => {
        if (!st.visible || !st.idx) return;
        const before = st.vp.w + "x" + st.vp.h;
        if (!measure() || before === st.vp.w + "x" + st.vp.h) return;
        const next = L.build(st.nodes, { viewport: st.vp, insets: insets(false), hidden: st.hidden, hideOff: st.hideOff });
        if (!st.layout || next.cols !== st.layout.cols) relayout({ fit: !st.moved && !st.selected });
        else if (!st.moved && !st.selected) fit(false);
      };
      // Wait one frame after resize settles so wrapped toolbar/dock heights are measured accurately.
      const resize = D.debounce(() => { if (root.requestAnimationFrame) root.requestAnimationFrame(settle); else settle(); }, 90);
      if (root.ResizeObserver) new root.ResizeObserver(resize).observe(els.map); else root.addEventListener("resize", resize);
    }

    function setData(nodes, opts) {
      opts = opts || {};
      st.nodes = nodes; st.idx = L.index(nodes);
      if (opts.reset) { st.selected = null; st.hover = null; st.hidden = {}; st.hideOff = false; st.query = ""; els.query.value = ""; panel.hide(); els.view.style.setProperty("--inset", "0px"); }
      buildLegend();
      if (st.visible) relayout({ entrance: !st.entered || !!opts.reset, fit: !st.entered || !!opts.reset || (!st.moved && !st.selected) });
      else st.dirty = true;
      if (st.selected && st.idx.byKey[st.selected]) panel.show(st.idx, st.selected);
    }
    function show() {
      st.visible = true;
      if (!st.idx) return;
      if (st.dirty || !st.layout) relayout({ entrance: !st.entered, fit: true });
      else { const before = st.vp.w + "x" + st.vp.h; if (measure() && before !== st.vp.w + "x" + st.vp.h && !st.moved) fit(false); }
    }
    function hide() { st.visible = false; }
    function clearAll() {
      st.nodes = []; st.idx = null; st.layout = null; st.selected = null; st.hover = null; st.hidden = {}; st.hideOff = false; st.query = ""; st.matches = {}; st.entered = false; st.labelEl = {};
      els.query.value = ""; els.hits.textContent = "";
      panel.hide();
      els.view.style.setProperty("--inset", "0px");
      clear(els.stage); clear(els.kinds);
      dock.update([], false);
      needs.clear();
      st.dirty = true;
    }

    bind();
    return { setData, show, hide, select, deselect, fit, setQuery, clearAll, relayout, state: st, panel, dock };
  }

  const api = { create };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotMap = api;
})(typeof window !== "undefined" ? window : globalThis);
