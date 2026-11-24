// 구조 지도의 순수 계산: 조회 API의 카드(graph.nodes)에서 카드·칩 좌표, 위임 연결선, 연결 관계, 카메라 값을 만든다.
// DOM·네트워크를 쓰지 않아 node에서도 그대로 불러 시험할 수 있다. 같은 입력은 같은 결과다.
(function (root) {
  "use strict";

  // 배율 기준은 카메라(camera.js)가 정한다: 칩 글자 13px이 지도 배율 READABLE 이상에서 12px 아래로 내려가지 않는다.
  const CAM = typeof module !== "undefined" && module.exports ? require("./camera.js") : root.BotCamera;

  // 단위는 px.
  const C = {
    CHIP_W: 172, CHIP_H: 34, CHIP_GX: 8, CHIP_GY: 6, CARD_PX: 16, HEAD: 52, CARD_PB: 16, DIV: 14, EMPTY_H: 34,
    GAP_X: 32, GAP_Y: 32, BUS: 64, PAD: 40, PROFILE_GAP: 72, LABEL_H: 44, MAX_COLS: 4, R: 14,
    READABLE: CAM.Z.READABLE, K_MAX: CAM.Z.FIT_MAX,
  };
  C.CARD_W = 2 * C.CHIP_W + C.CHIP_GX + 2 * C.CARD_PX;

  // 에이전트 카드 안에 놓는 항목(위에서 아래로 입구 → 트리거 → 스킬 → 도구)과, 아래 독에 놓는 공통 자원.
  const ITEM_KINDS = ["binding", "trigger", "skill", "tool"];
  const COMMON_KINDS = ["scope", "secret"];
  const KINDS = ITEM_KINDS.concat(COMMON_KINDS);
  const KIND_LABEL = { binding: "입구", trigger: "트리거", skill: "스킬", tool: "도구", scope: "스코프", secret: "비밀", agent: "에이전트", profile: "프로필" };
  const REL_LABEL = { delegates: "위임", uses: "사용", runs: "소유", handles: "처리", triggers: "깨움", needs: "필요" };

  const key = (c) => c.profile + "|" + c.id;
  const compare = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
  const mainFirst = (a, b) => (a.name === "main" ? -1 : b.name === "main" ? 1 : compare(a.name, b.name));

  function fact(c, name) {
    for (let i = 0; i < c.facts.length; i++) if (c.facts[i].key === name) return c.facts[i].val;
    return null;
  }
  // 정책이 허용하지 않은 스코프만 «꺼짐»(사선·흐림)으로 보인다.
  const isOff = (c) => c.kind === "scope" && fact(c, "granted") === "false";
  const agentName = (to) => to.replace(/^agent:/, "");
  // 카드의 주인 에이전트. 입구(binding)는 프로필 수준 카드라 «처리»로 이어진 에이전트의 카드 안에 놓는다.
  function ownerOf(c) {
    if (c.agent) return c.agent;
    if (c.kind !== "binding") return null;
    const l = c.links.filter((x) => x.rel === "handles")[0];
    return l ? agentName(l.to) : null;
  }
  const ownerKey = (c) => (ownerOf(c) ? c.profile + "|agent:" + ownerOf(c) : null);

  // 같은 에이전트 안 항목: 종류 → (트리거는 검사 순서) → 이름
  function itemOrder(a, b) {
    const k = ITEM_KINDS.indexOf(a.kind) - ITEM_KINDS.indexOf(b.kind);
    if (k) return k;
    if (a.kind === "trigger") {
      const o = (parseInt(fact(a, "order"), 10) || 0) - (parseInt(fact(b, "order"), 10) || 0);
      if (o) return o;
    }
    return compare(a.name, b.name);
  }

  // 위임 깊이: 들어오는 위임이 없는 에이전트가 0(main 우선), 그 아래로 BFS. 순환·미도달은 0으로 둔다.
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

  // ---------- 관계 ----------
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

  // 어떤 노드를 가리키면 함께 밝힐 키: 자기 자신, 소속 에이전트, 직접 이어진 노드(와 그 소속 에이전트), 위임 사슬(프로필까지), 에이전트면 그 항목들.
  function related(idx, k) {
    const set = {}, add = (x) => { if (x && idx.byKey[x]) set[x] = true; };
    const c = idx.byKey[k];
    if (!c) return set;
    add(k); add(ownerKey(c));
    // 프로필 카드: 그 프로필의 모든 카드와 항목을 함께 밝힌다.
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

  // 범례(= 필터): 있는 종류와 개수, 꺼진(정책이 허용하지 않은) 개수.
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

  // ---------- 배치 ----------
  // 한 에이전트 카드의 안쪽: 종류별 묶음(입구·트리거·스킬·도구 순)을 2열 칩 격자로, 묶음 사이에 가는 구분선 자리.
  // 부모에게서 물려받은 스킬(ref)은 같은 키의 칩으로 한 번 더 놓는다(눌러서 이동하면 정의한 쪽 카드로 간다).
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

  // 곡선: 위·아래로 수직 접선을 가진 3차 곡선(S자). 위임선과 «필요» 선이 함께 쓴다. 아래로도 위로도 이어진다.
  function curve(x0, y0, x1, y1) {
    const dir = y1 >= y0 ? 1 : -1, dy = Math.max(24, Math.abs(y1 - y0) / 2);
    return "M" + x0 + " " + y0 + "C" + x0 + " " + (y0 + dir * dy) + " " + x1 + " " + (y1 - dir * dy) + " " + x1 + " " + y1;
  }
  // 위임선의 경로: 첫 행 자식은 부모 아래 중앙 → 자식 위 중앙의 곡선, 아래 행 자식은 곡선으로 왼쪽 골목에 내려앉아
  // 골목을 따라 내려간 뒤 둥글게 꺾여 카드 왼쪽 옆으로 들어간다(카드를 가로지르지 않는다). pts는 [부모 아래, 모선, 골목(모선), 골목(입구), 입구].
  function delegationPath(kind, pts) {
    const S = pts[0], E = pts[pts.length - 1];
    if (kind !== "left") return curve(S[0], S[1], E[0], E[1]);
    const g = pts[2], r = Math.min(C.R, Math.abs(E[1] - pts[3][1]) + C.R, Math.abs(E[0] - g[0]));
    return curve(S[0], S[1], g[0], g[1]) + "L" + g[0] + " " + (E[1] - r) + "Q" + g[0] + " " + E[1] + " " + (g[0] + r) + " " + E[1] + "L" + E[0] + " " + E[1];
  }

  // 한 프로필을 cols 열로 배치한다(좌표는 프로필 상자 왼쪽 위 기준). 위임 깊이마다 한 층, 층 안은 cols열 격자.
  function layoutProfile(agents, itemsOf, cols) {
    const depth = layers(agents), levels = {}, geo = {};
    agents.forEach((a) => { (levels[depth[a.name]] = levels[depth[a.name]] || []).push(a); geo[a.name] = inner(itemsOf(a)); });
    const plan = Object.keys(levels).map(Number).sort((a, b) => a - b).map((d) => {
      const list = levels[d].slice().sort(mainFirst), c = Math.min(cols, list.length), rows = [];
      for (let i = 0; i < list.length; i += c) rows.push(list.slice(i, i + c));
      // 같은 행은 높이를 맞춘다.
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
        const x0 = p.d === 0 ? (w - rowW) / 2 : (w - levelW(p)) / 2; // 맨 위 층은 가운데, 아래 층은 격자 열에 맞춘다
        row.forEach((a, c) => { cards[a.name] = { agent: a, x: x0 + c * (C.CARD_W + C.GAP_X), y, w: C.CARD_W, h: p.rowH[r], depth: p.d, row: r, col: c, lastRow: r === p.rows.length - 1, geo: geo[a.name] }; });
        y += p.rowH[r] + (r < p.rows.length - 1 ? C.GAP_Y : 0);
      });
      bottoms[p.d] = y;
    });
    // 위임 선: 부모 아래 중앙에서 모선(bus)까지 내려와 → 첫 행 자식은 위로 들어가고, 아래 행 자식은 왼쪽 골목으로 내려가 왼쪽 옆으로 들어간다.
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

  /**
   * nodes: 조회 API `graph.nodes`. opts: { viewport: {w,h}, insets: {top,right,bottom,left}, hidden: {종류: true}, hideOff, cols }.
   * 반환: { width, height, cols, profiles, cards, edges, commons, rects, bounds, empty } — 좌표는 모두 스테이지 기준 절대 좌표.
   * 프로필 이름표는 프로필이 하나여도 늘 그린다(이름표가 프로필 카드라 눌러서 사실을 볼 수 있다).
   */
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
      // 공통 종류와 주인 없는 항목은 아래 독(공통 자원)에서 보인다.
      const commons = KINDS.map((kind) => ({ profile: name, kind, items: cards.filter((c) => c.kind === kind && visible(c) && (COMMON_KINDS.indexOf(kind) >= 0 || !known[ownerOf(c)])).sort((a, b) => compare(a.name, b.name)) })).filter((g) => g.items.length);
      const card = cards.filter((c) => c.kind === "profile")[0];
      return { name, agents, itemsOf, commons, pkey: card ? key(card) : null };
    });
    const headH = C.LABEL_H;

    // 프로필 상자들을 위에서 아래로 쌓는다.
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
      // 열 수 고르기: 기본 배율이 읽을 수 있는 선(READABLE) 이상인 배치 중 한눈에 가장 크게 보이는 것, 같으면 화면 비율에 가까운 것.
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
      // 프로필 카드의 사각형은 프로필 상자 전체다(눌러서 다가가면 그 프로필 전체가 보인다).
      const box = { name: prep[i].name, key: prep[i].pkey, x: C.PAD + p.x, y: C.PAD + p.top, w: p.b.w, h: p.b.h + headH };
      profiles.push(box);
      if (box.key) rects[box.key] = { x: box.x, y: box.y, w: box.w, h: box.h, card: box.key };
      Object.keys(p.b.cards).map((n) => p.b.cards[n]).sort((a, b) => a.depth - b.depth || a.row - b.row || a.col - b.col).forEach((c) => {
        const a = c.agent;
        const card = { key: key(a), id: a.id, profile: a.profile, name: a.name, summary: a.summary, x: ox + c.x, y: oy + c.y, w: c.w, h: c.h, depth: c.depth, row: c.row, col: c.col, root: c.depth === 0, empty: c.geo.empty, dividers: c.geo.dividers, groups: c.geo.groups, counts: c.geo.groups.map((g) => ({ kind: g.kind, n: g.chips.length })) };
        cards.push(card);
        rects[card.key] = { x: card.x, y: card.y, w: card.w, h: card.h, card: card.key };
        // 물려받은 칩은 정의한 쪽(먼저 놓인 칩)의 자리를 덮어쓰지 않는다.
        card.groups.forEach((g) => g.chips.forEach((ch) => { if (!ch.ref || !rects[ch.key]) rects[ch.key] = { x: card.x + ch.x, y: card.y + ch.y, w: ch.w, h: ch.h, card: card.key }; }));
      });
      p.b.edges.forEach((e) => {
        const pts = e.pts.map((q) => [q[0] + ox, q[1] + oy]);
        edges.push({ id: e.id, from: e.from, to: e.to, cross: e.cross, d: delegationPath(e.cross ? "top" : e.end[2], pts), start: [e.start[0] + ox, e.start[1] + oy], end: [e.end[0] + ox, e.end[1] + oy, e.end[2]] });
      });
    });
    // 화면에 담을 상자: 에이전트 카드들과 프로필 이름 줄까지.
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
