// Pure structure-map layout: computes card/chip geometry, delegation curves, relations, and column plans from graph.nodes.
(function (root) {
  "use strict";

  const CAM = typeof module !== "undefined" && module.exports ? require("./camera.js") : root.BotCamera;

  const C = {
    CHIP_W: 172, CHIP_H: 34, CHIP_GX: 8, CHIP_GY: 6, CARD_PX: 16, HEAD: 52, CARD_PB: 16, DIV: 14, EMPTY_H: 34,
    GAP_X: 32, GAP_Y: 32, BUS: 64, PAD: 40, PROFILE_GAP: 72, LABEL_H: 44, MAX_COLS: 4, R: 14,
    READABLE: CAM.Z.READABLE, K_MAX: CAM.Z.FIT_MAX,
  };
  C.CARD_W = 2 * C.CHIP_W + C.CHIP_GX + 2 * C.CARD_PX;

  const ITEM_KINDS = ["binding", "trigger", "skill", "tool"];
  const COMMON_KINDS = ["scope", "secret"];
  const KINDS = ITEM_KINDS.concat(COMMON_KINDS);
  const KIND_LABEL = { binding: "Binding", trigger: "Trigger", skill: "Skill", tool: "Tool", scope: "Scope", secret: "Secret", agent: "Agent", profile: "Profile" };
  const REL_LABEL = { delegates: "Delegates", uses: "Uses", runs: "Owns", handles: "Handles", triggers: "Triggers", needs: "Needs" };

  const key = (c) => c.profile + "|" + c.id;
  const compare = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
  const mainFirst = (a, b) => (a.name === "main" ? -1 : b.name === "main" ? 1 : compare(a.name, b.name));

  function fact(c, name) {
    for (let i = 0; i < c.facts.length; i++) if (c.facts[i].key === name) return c.facts[i].val;
    return null;
  }
  const isOff = (c) => c.kind === "scope" && fact(c, "granted") === "false";
  const agentName = (to) => to.replace(/^agent:/, "");
  // Bindings are profile-scoped nodes placed inside the agent card linked via "handles".
  function ownerOf(c) {
    if (c.agent) return c.agent;
    if (c.kind !== "binding") return null;
    const l = c.links.filter((x) => x.rel === "handles")[0];
    return l ? agentName(l.to) : null;
  }
  const ownerKey = (c) => (ownerOf(c) ? c.profile + "|agent:" + ownerOf(c) : null);

  function itemOrder(a, b) {
    const k = ITEM_KINDS.indexOf(a.kind) - ITEM_KINDS.indexOf(b.kind);
    if (k) return k;
    if (a.kind === "trigger") {
      const o = (parseInt(fact(a, "order"), 10) || 0) - (parseInt(fact(b, "order"), 10) || 0);
      if (o) return o;
    }
    return compare(a.name, b.name);
  }

  // BFS delegation depth from zero-indegree roots (main first); cycles and unreachable nodes default to 0.
  function layers(agents) {
    const byName = {}, incoming = {}, out = {};
    agents.forEach((a) => { byName[a.name] = a; incoming[a.name] = 0; out[a.name] = []; });
    agents.forEach((a) => a.links.forEach((l) => {
      if (l.rel !== "delegates") return;
      const to = agentName(l.to);
      if (byName[to] && to !== a.name) { out[a.name].push(to); incoming[to]++; }
    }));
    const depth = {}, queue = [];
    const roots = agents.map((a) => a.name).filter((n) => incoming[n] === 0);
    roots.sort((a, b) => (a === "main" ? -1 : b === "main" ? 1 : compare(a, b)));
    roots.forEach((n) => { depth[n] = 0; queue.push(n); });
    while (queue.length) {
      const n = queue.shift();
      out[n].slice().sort().forEach((t) => { if (depth[t] === undefined) { depth[t] = depth[n] + 1; queue.push(t); } });
    }
    agents.forEach((a) => { if (depth[a.name] === undefined) depth[a.name] = 0; });
    return depth;
  }

  function index(nodes) {
    const byKey = {}, out = {}, inc = {};
    nodes.forEach((c) => { byKey[key(c)] = c; out[key(c)] = []; inc[key(c)] = []; });
    nodes.forEach((c) => c.links.forEach((l) => {
      const to = c.profile + "|" + l.to;
      if (!byKey[to]) return;
      out[key(c)].push({ rel: l.rel, key: to });
      inc[to].push({ rel: l.rel, key: key(c) });
    }));
    return { byKey, out, inc };
  }

  function related(idx, k) {
    const set = {}, add = (x) => { if (x && idx.byKey[x]) set[x] = true; };
    const c = idx.byKey[k];
    if (!c) return set;
    add(k); add(ownerKey(c));
    if (c.kind === "profile") { Object.keys(idx.byKey).forEach((x) => { if (idx.byKey[x].profile === c.profile) add(x); }); return set; }
    const near = (x) => { add(x); const n = idx.byKey[x]; if (n) add(ownerKey(n)); };
    idx.out[k].forEach((e) => near(e.key));
    idx.inc[k].forEach((e) => near(e.key));
    if (c.kind === "agent") Object.keys(idx.byKey).forEach((x) => { const n = idx.byKey[x]; if (n.profile === c.profile && ownerOf(n) === c.name) add(x); });
    const seen = {};
    let from = c.kind === "agent" ? k : ownerKey(c);
    while (from && !seen[from]) {
      seen[from] = true; add(from);
      const up = (idx.inc[from] || []).filter((e) => e.rel === "delegates").map((e) => e.key).sort()[0];
      from = up || null;
    }
    return set;
  }

  function matches(nodes, query) {
    const q = String(query || "").trim().toLowerCase();
    const hit = {};
    if (!q) return hit;
    nodes.forEach((c) => {
      if (c.name.toLowerCase().indexOf(q) >= 0 || String(c.summary || "").toLowerCase().indexOf(q) >= 0) hit[key(c)] = true;
    });
    return hit;
  }

  function legend(nodes) {
    const m = {};
    nodes.forEach((c) => {
      if (KINDS.indexOf(c.kind) < 0) return;
      const e = (m[c.kind] = m[c.kind] || { kind: c.kind, label: KIND_LABEL[c.kind], total: 0, off: 0 });
      e.total++;
      if (isOff(c)) e.off++;
    });
    return KINDS.filter((k) => m[k]).map((k) => m[k]);
  }

  function inner(items) {
    const groups = [];
    let y = C.HEAD;
    ITEM_KINDS.forEach((kind) => {
      const list = items.filter((c) => c.kind === kind);
      if (!list.length) return;
      if (groups.length) y += C.DIV;
      const g = { kind, y, chips: [] };
      list.forEach((c, i) => g.chips.push({ key: key(c), id: c.id, kind, name: c.name, off: isOff(c), ref: !!c.ref, lock: kind === "tool" && ["always", "policy"].indexOf(fact(c, "approval")) >= 0, x: C.CARD_PX + (i % 2) * (C.CHIP_W + C.CHIP_GX), y: y + Math.floor(i / 2) * (C.CHIP_H + C.CHIP_GY), w: C.CHIP_W, h: C.CHIP_H }));
      const rows = Math.ceil(list.length / 2);
      y += rows * C.CHIP_H + (rows - 1) * C.CHIP_GY;
      groups.push(g);
    });
    const empty = !groups.length;
    if (empty) y += C.EMPTY_H;
    return { groups, empty, h: y + C.CARD_PB, dividers: groups.slice(1).map((g) => g.y - C.DIV / 2) };
  }

  function curve(x0, y0, x1, y1) {
    const dir = y1 >= y0 ? 1 : -1, dy = Math.max(24, Math.abs(y1 - y0) / 2);
    return "M" + x0 + " " + y0 + "C" + x0 + " " + (y0 + dir * dy) + " " + x1 + " " + (y1 - dir * dy) + " " + x1 + " " + y1;
  }
  // Route lower-row children through the left gutter so delegation curves never cross card faces.
  function delegationPath(kind, pts) {
    const S = pts[0], E = pts[pts.length - 1];
    if (kind !== "left") return curve(S[0], S[1], E[0], E[1]);
    const g = pts[2], r = Math.min(C.R, Math.abs(E[1] - pts[3][1]) + C.R, Math.abs(E[0] - g[0]));
    return curve(S[0], S[1], g[0], g[1]) + "L" + g[0] + " " + (E[1] - r) + "Q" + g[0] + " " + E[1] + " " + (g[0] + r) + " " + E[1] + "L" + E[0] + " " + E[1];
  }

  function layoutProfile(agents, itemsOf, cols) {
    const depth = layers(agents), levels = {}, geo = {};
    agents.forEach((a) => { (levels[depth[a.name]] = levels[depth[a.name]] || []).push(a); geo[a.name] = inner(itemsOf(a)); });
    const plan = Object.keys(levels).map(Number).sort((a, b) => a - b).map((d) => {
      const list = levels[d].slice().sort(mainFirst), c = Math.min(cols, list.length), rows = [];
      for (let i = 0; i < list.length; i += c) rows.push(list.slice(i, i + c));
      return { d, c, rows, rowH: rows.map((r) => r.reduce((m, a) => Math.max(m, geo[a.name].h), 0)) };
    });
    const levelW = (p) => p.c * C.CARD_W + (p.c - 1) * C.GAP_X;
    const w = plan.reduce((m, p) => Math.max(m, levelW(p)), C.CARD_W);
    const cards = {}, bottoms = {};
    let y = 0;
    plan.forEach((p, li) => {
      if (li) y += C.BUS;
      p.rows.forEach((row, r) => {
        const rowW = row.length * C.CARD_W + (row.length - 1) * C.GAP_X;
        const x0 = p.d === 0 ? (w - rowW) / 2 : (w - levelW(p)) / 2;
        row.forEach((a, c) => { cards[a.name] = { agent: a, x: x0 + c * (C.CARD_W + C.GAP_X), y, w: C.CARD_W, h: p.rowH[r], depth: p.d, row: r, col: c, lastRow: r === p.rows.length - 1, geo: geo[a.name] }; });
        y += p.rowH[r] + (r < p.rows.length - 1 ? C.GAP_Y : 0);
      });
      bottoms[p.d] = y;
    });
    const edges = [];
    agents.forEach((u) => u.links.forEach((l) => {
      if (l.rel !== "delegates") return;
      const v = cards[agentName(l.to)], p = cards[u.name];
      if (!v || !p || v === p) return;
      const ux = p.x + p.w / 2, uy = p.y + p.h, vx = v.x + v.w / 2;
      const e = { id: u.profile + "|" + u.id + ">" + u.profile + "|" + l.to, from: u.profile + "|" + u.id, to: u.profile + "|" + l.to, cross: false, pts: null };
      if (v.depth === p.depth + 1 && p.lastRow) {
        const bus = bottoms[p.depth] + C.BUS / 2;
        if (v.row === 0) { e.pts = [[ux, uy], [ux, bus], [vx, bus], [vx, v.y]]; e.end = [vx, v.y, "top"]; }
        else { const gx = v.x - C.GAP_X / 2, hy = v.y + C.HEAD / 2; e.pts = [[ux, uy], [ux, bus], [gx, bus], [gx, hy], [v.x, hy]]; e.end = [v.x, hy, "left"]; }
      } else { e.cross = true; e.pts = [[ux, uy], [vx, v.y]]; e.end = [vx, v.y, "top"]; }
      e.start = [ux, uy];
      edges.push(e);
    }));
    return { w, h: y, cards, edges };
  }

  function build(nodes, opts) {
    opts = opts || {};
    const hidden = opts.hidden || {}, hideOff = !!opts.hideOff;
    const visible = (c) => !hidden[c.kind] && !(hideOff && isOff(c));
    const byProfile = {}, order = [];
    nodes.forEach((c) => { if (!byProfile[c.profile]) { byProfile[c.profile] = []; order.push(c.profile); } byProfile[c.profile].push(c); });
    order.sort();
    const prep = order.map((name) => {
      const cards = byProfile[name], byId = {};
      cards.forEach((c) => { byId[c.id] = c; });
      const agents = cards.filter((c) => c.kind === "agent").sort((a, b) => compare(a.name, b.name));
      const known = {};
      agents.forEach((a) => { known[a.name] = true; });
      const itemsOf = (a) => {
        const own = cards.filter((c) => ownerOf(c) === a.name && ITEM_KINDS.indexOf(c.kind) >= 0 && visible(c));
        const refs = a.links.filter((l) => l.rel === "uses").map((l) => byId[l.to]).filter((c) => c && c.kind === "skill" && ownerOf(c) !== a.name && visible(c)).map((c) => Object.assign({}, c, { ref: true }));
        return own.concat(refs).sort(itemOrder);
      };
      const commons = KINDS.map((kind) => ({ profile: name, kind, items: cards.filter((c) => c.kind === kind && visible(c) && (COMMON_KINDS.indexOf(kind) >= 0 || !known[ownerOf(c)])).sort((a, b) => compare(a.name, b.name)) })).filter((g) => g.items.length);
      const card = cards.filter((c) => c.kind === "profile")[0];
      return { name, agents, itemsOf, commons, pkey: card ? key(card) : null };
    });
    const headH = C.LABEL_H;

    const at = (cols) => {
      const blocks = prep.map((p) => (p.agents.length ? layoutProfile(p.agents, p.itemsOf, cols) : { w: C.CARD_W, h: 0, cards: {}, edges: [] }));
      const w = blocks.reduce((m, b) => Math.max(m, b.w), 0);
      let y = 0;
      const placed = blocks.map((b, i) => { if (i) y += C.PROFILE_GAP; const top = y; y += headH + b.h; return { b, top, x: (w - b.w) / 2 }; });
      return { placed, w, h: y };
    };
    const maxAgents = prep.reduce((m, p) => Math.max(m, p.agents.length), 1);
    const vp = opts.viewport, ins = Object.assign({ top: 0, right: 0, bottom: 0, left: 0 }, opts.insets);
    let cols = opts.cols || 0, laid = null;
    if (!cols && !vp) cols = Math.min(3, maxAgents);
    if (cols) laid = at(cols);
    else {
      // Pick the column count maximizing fit scale while staying >= READABLE, breaking ties by aspect ratio.
      const a = { w: vp.w - ins.left - ins.right, h: vp.h - ins.top - ins.bottom };
      let best = null;
      for (let c = 1; c <= Math.min(C.MAX_COLS, maxAgents); c++) {
        const l = at(c), kw = l.w ? a.w / l.w : 1, kall = l.w ? Math.min(kw, a.h / l.h) : 1;
        const score = (kw >= C.READABLE ? 10 : 0) + Math.min(C.K_MAX, kall) + (kw >= C.READABLE ? 0 : Math.min(C.K_MAX, kw) / 10);
        const tie = l.w ? -Math.abs(Math.log(l.w / l.h / (a.w / a.h))) : 0;
        if (!best || score > best.score + 0.01 || (Math.abs(score - best.score) <= 0.01 && tie > best.tie)) best = { c, score, tie, l };
      }
      cols = best.c; laid = best.l;
    }

    const cards = [], edges = [], rects = {}, profiles = [];
    laid.placed.forEach((p, i) => {
      const ox = C.PAD + p.x, oy = C.PAD + p.top + headH;
      const box = { name: prep[i].name, key: prep[i].pkey, x: C.PAD + p.x, y: C.PAD + p.top, w: p.b.w, h: p.b.h + headH };
      profiles.push(box);
      if (box.key) rects[box.key] = { x: box.x, y: box.y, w: box.w, h: box.h, card: box.key };
      Object.keys(p.b.cards).map((n) => p.b.cards[n]).sort((a, b) => a.depth - b.depth || a.row - b.row || a.col - b.col).forEach((c) => {
        const a = c.agent;
        const card = { key: key(a), id: a.id, profile: a.profile, name: a.name, summary: a.summary, x: ox + c.x, y: oy + c.y, w: c.w, h: c.h, depth: c.depth, row: c.row, col: c.col, root: c.depth === 0, empty: c.geo.empty, dividers: c.geo.dividers, groups: c.geo.groups, counts: c.geo.groups.map((g) => ({ kind: g.kind, n: g.chips.length })) };
        cards.push(card);
        rects[card.key] = { x: card.x, y: card.y, w: card.w, h: card.h, card: card.key };
        // Do not let inherited skill chips overwrite the defining agent's rect.
        card.groups.forEach((g) => g.chips.forEach((ch) => { if (!ch.ref || !rects[ch.key]) rects[ch.key] = { x: card.x + ch.x, y: card.y + ch.y, w: ch.w, h: ch.h, card: card.key }; }));
      });
      p.b.edges.forEach((e) => {
        const pts = e.pts.map((q) => [q[0] + ox, q[1] + oy]);
        edges.push({ id: e.id, from: e.from, to: e.to, cross: e.cross, d: delegationPath(e.cross ? "top" : e.end[2], pts), start: [e.start[0] + ox, e.start[1] + oy], end: [e.end[0] + ox, e.end[1] + oy, e.end[2]] });
      });
    });
    const boxes = cards.map((c) => c).concat(profiles);
    const b = boxes.length ? boxes.reduce((m, c) => ({ x1: Math.min(m.x1, c.x), y1: Math.min(m.y1, c.y), x2: Math.max(m.x2, c.x + c.w), y2: Math.max(m.y2, c.y + c.h) }), { x1: 1e9, y1: 1e9, x2: -1e9, y2: -1e9 }) : { x1: 0, y1: 0, x2: 0, y2: 0 };
    return {
      width: laid.w + 2 * C.PAD, height: laid.h + 2 * C.PAD, cols, profiles, cards, edges, rects,
      commons: prep.reduce((all, p) => all.concat(p.commons), []),
      bounds: { x: b.x1, y: b.y1, w: b.x2 - b.x1, h: b.y2 - b.y1 }, empty: !cards.length,
    };
  }

  const api = { C, KINDS, ITEM_KINDS, COMMON_KINDS, KIND_LABEL, REL_LABEL, key, fact, isOff, ownerOf, ownerKey, layers, index, related, matches, legend, build, curve };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotLayout = api;
})(typeof window !== "undefined" ? window : globalThis);
