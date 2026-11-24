// 지도의 곁가지 화면: 칩 한 개, 오른쪽 상세 패널, 아래 독(공통 자원)과 펼침 목록. 글자는 모두 textContent로만 넣는다.
(function (root) {
  "use strict";
  const D = root.BotDom, L = root.BotLayout, B = root.BotLabels;
  const { h, icon } = D;

  const REL_ORDER = ["delegates", "uses", "runs", "handles", "triggers", "needs"];
  // 들어오는 관계(이 노드를 가리키는 쪽)를 부르는 말
  const IN_LABEL = { delegates: "위임한 곳", uses: "사용하는 곳", runs: "소유한 에이전트", handles: "처리를 맡긴 입구", triggers: "깨우는 트리거", needs: "필요로 하는 곳" };
  const label = (kind) => L.KIND_LABEL[kind] || kind;

  // 정책 허용 여부(스코프만 갖는다) — 작은 모양 + 글자
  function stateOf(c) {
    if (c.kind !== "scope") return null;
    const on = L.fact(c, "granted") === "true";
    return { on, text: on ? "정책 허용" : "정책 미허용" };
  }

  // 칩: 종류 아이콘(정책 미허용은 사선) + 이름. 승인이 필요한 도구는 자물쇠, 물려받은 스킬은 «상속» 표시. 지도 카드 안·독 목록 안에서 같은 모양이다.
  function chip(it) {
    const off = !!it.off, notes = [];
    if (it.ref) notes.push("부모에게서 물려받음");
    if (it.lock) notes.push("승인 필요");
    if (off) notes.push("정책 미허용");
    const tail = notes.length ? " — " + notes.join(", ") : "";
    return h("button", { type: "button", class: "chip" + (off ? " is-off" : "") + (it.ref ? " is-ref" : ""), "data-key": it.key, title: it.name + tail, "aria-label": it.name + " (" + label(it.kind) + (notes.length ? ", " + notes.join(", ") : "") + ")" }, [icon(it.kind, { size: 14, off }), h("span", { class: "nm", text: it.name }), it.ref ? h("span", { class: "chip-tag", text: "상속" }) : null, it.lock ? icon("lock", { size: 12, cls: "chip-lock" }) : null]);
  }

  // ---------- 상세 패널 ----------
  function createPanel(o) {
    const el = o.el;
    let shown = null;

    function target(idx, key) {
      const c = idx.byKey[key];
      const body = [c ? icon(c.kind, { size: 14, off: L.isOff(c) }) : null, h("span", { class: "nm", text: c ? c.name : key.split("|").pop() })];
      if (c && o.canGo(key)) return h("button", { type: "button", class: "lk", "data-go": key, title: c.name, onclick: () => o.onNavigate(key) }, body);
      return h("span", { class: "lk", title: c ? c.name : key }, body);
    }
    function relations(idx, key) {
      const out = {}, inn = {}, sections = [];
      idx.out[key].forEach((e) => { (out[e.rel] = out[e.rel] || []).push(e.key); });
      idx.inc[key].forEach((e) => { (inn[e.rel] = inn[e.rel] || []).push(e.key); });
      const group = (title, keys) => sections.push(h("div", { class: "rel" }, [h("p", { text: title + " " + keys.length }), h("div", { class: "links" }, keys.map((k) => target(idx, k)))]));
      REL_ORDER.forEach((r) => { if (out[r]) group(L.REL_LABEL[r] || r, out[r]); });
      REL_ORDER.forEach((r) => { if (inn[r]) group(IN_LABEL[r] || r, inn[r]); });
      Object.keys(out).filter((r) => REL_ORDER.indexOf(r) < 0).forEach((r) => group(r, out[r]));
      return sections;
    }
    // 위험 등급·승인 요건은 표 위의 알약으로, 스코프의 정책 허용 여부도 알약으로 보인다(사실 표에서는 뺀다).
    const PILLED = { tool: ["class", "approval"], scope: ["granted"] };

    function show(idx, key) {
      const c = idx.byKey[key];
      if (!c) return hide();
      shown = key;
      const st = stateOf(c), skip = PILLED[c.kind] || [];
      const facts = c.facts.filter((f) => skip.indexOf(f.key) < 0);
      const pills = [];
      if (st) pills.push(D.pill(st.text, st.on ? "ok" : "off", st.on ? "on" : "off"));
      if (c.kind === "tool") pills.push(B.riskPill(L.fact(c, "class")), B.approvalReq(L.fact(c, "approval")));
      const rel = relations(idx, key);
      const kids = [
        h("div", { class: "pn-head" }, [
          h("span", { class: "pn-tile" }, [icon(c.kind, { size: 18, off: L.isOff(c) })]),
          h("div", { class: "pn-kind" }, [h("b", { text: label(c.kind) }), h("span", { text: c.agent ? "에이전트 " + c.agent : "프로필 " + c.profile })]),
          h("button", { type: "button", class: "pn-close", "aria-label": "상세 닫기", onclick: () => o.onClose() }, [icon("close", { size: 16 })]),
        ]),
        h("h2", { class: "pn-title", text: c.name }),
        pills.length ? h("div", { class: "pn-state" }, pills) : null,
        c.summary ? h("p", { class: "pn-sum", text: c.summary }) : null,
        facts.length ? h("div", { class: "pn-sec" }, [h("h3", { text: "사실" }), h("dl", { class: "facts" }, [].concat.apply([], facts.map((f) => [h("dt", { text: B.factLabel(c.kind, f.key) }), h("dd", null, [B.factNode(c, f)])])))]) : null,
        rel.length ? h("div", { class: "pn-sec" }, [h("h3", { text: "연결" })].concat(rel)) : null,
        h("p", { class: "pn-foot" }, [D.text("프로필 " + c.profile + " · "), h("code", { text: c.id })]),
      ];
      D.clear(el);
      el.appendChild(h("div", { class: "pn" }, kids));
      el.classList.add("open");
      el.setAttribute("aria-hidden", "false");
    }
    function hide() {
      shown = null;
      el.classList.remove("open");
      el.setAttribute("aria-hidden", "true");
    }
    return { show, hide, get key() { return shown; } };
  }

  // ---------- 독: 공통 자원(스코프·비밀)을 종류별 개수로 보이고, 누르면 목록이 펼쳐진다 ----------
  function createDock(o) {
    const el = o.el;
    let groups = [], openKind = null, pop = null, badges = {};

    function close() {
      const kind = openKind, inside = !!(pop && pop.contains && pop.contains(document.activeElement));
      openKind = null;
      if (pop && pop.parentNode) pop.parentNode.removeChild(pop);
      pop = null;
      Array.prototype.forEach.call(el.children || [], (b) => { if (b.setAttribute && b.getAttribute("data-kind")) b.setAttribute("aria-expanded", "false"); });
      // 목록 안에 있던 포커스는 목록을 연 단추로 돌려준다(포커스가 사라지면 Esc 같은 키가 지도에 닿지 않는다).
      const opener = inside && kind ? pill(kind) : null;
      if (opener && opener.focus) opener.focus();
      if (o.onClose) o.onClose();
    }
    function open(kind) {
      close();
      const list = groups.filter((g) => g.kind === kind);
      if (!list.length) return;
      openKind = kind;
      const multi = list.length > 1;
      const total = list.reduce((n, g) => n + g.items.length, 0), off = list.reduce((n, g) => n + g.items.filter((c) => L.isOff(c)).length, 0);
      const body = [h("div", { class: "pop-head" }, [h("h3", { text: label(kind) }), h("span", { text: total + "개" + (off ? " · 정책 미허용 " + off : "") })])];
      list.forEach((g) => {
        if (multi) body.push(h("p", { class: "pop-sub", text: "프로필 " + g.profile }));
        body.push(h("div", { class: "pop-grid" }, g.items.map((c) => o.makeChip({ key: L.key(c), id: c.id, kind: c.kind, name: c.name, off: L.isOff(c) }))));
      });
      pop = h("div", { class: "pop enter", id: "pop-" + kind, role: "group", "aria-label": label(kind) + " 목록" }, body);
      el.appendChild(pop);
      Array.prototype.forEach.call(el.children, (b) => { if (b.getAttribute && b.getAttribute("data-kind")) b.setAttribute("aria-expanded", String(b.getAttribute("data-kind") === kind)); });
      if (o.onOpen) o.onOpen(kind);
    }
    function update(next, first) {
      const keep = openKind;
      close();
      groups = next;
      const kinds = [];
      groups.forEach((g) => { if (kinds.indexOf(g.kind) < 0) kinds.push(g.kind); });
      D.clear(el);
      badges = {};
      kinds.forEach((kind) => {
        const n = groups.filter((g) => g.kind === kind).reduce((s, g) => s + g.items.length, 0);
        const num = h("span", { class: "n", text: first ? "0" : String(n) });
        if (first) D.countTo(num, n);
        el.appendChild(h("button", { type: "button", class: "dk", "data-kind": kind, "aria-expanded": "false", "aria-controls": "pop-" + kind, onclick: () => (openKind === kind ? close() : open(kind)) }, [icon(kind, { size: 16 }), h("span", { class: "nm", text: label(kind) }), num]));
      });
      if (keep && kinds.indexOf(keep) >= 0) open(keep);
    }
    // 이 키들 가운데 독 목록에 든 것의 수(종류별)를 알린다 — 연결된 공통 자원이 어디 있는지 단서
    function mark(set) {
      Array.prototype.forEach.call(el.children, (b) => {
        if (!b.getAttribute || !b.getAttribute("data-kind")) return;
        const kind = b.getAttribute("data-kind");
        const n = set ? groups.filter((g) => g.kind === kind).reduce((s, g) => s + g.items.filter((c) => set[L.key(c)]).length, 0) : 0;
        const old = badges[kind];
        if (old && old.parentNode) old.parentNode.removeChild(old);
        delete badges[kind];
        if (n) { badges[kind] = h("span", { class: "badge", title: "선택과 연결된 " + label(kind) + " " + n + "개", text: String(n) }); b.appendChild(badges[kind]); }
      });
    }
    // 종류의 알약 요소(«필요» 선이 닿는 곳)
    const pill = (kind) => Array.prototype.filter.call(el.children || [], (b) => b.getAttribute && b.getAttribute("data-kind") === kind)[0] || null;
    return { update, open, close, mark, pill, get openKind() { return openKind; }, get el() { return el; } };
  }

  const api = { chip, createPanel, createDock, stateOf, label };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotPanel = api;
})(typeof window !== "undefined" ? window : globalThis);
