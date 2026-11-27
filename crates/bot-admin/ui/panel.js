// Structure map companions: node chip builder, slide-out detail panel, and shared-resource dock.
(function (root) {
  "use strict";
  const D = root.BotDom, L = root.BotLayout, B = root.BotLabels;
  const { h, icon } = D;

  const REL_ORDER = ["delegates", "uses", "runs", "handles", "triggers", "needs"];
  const IN_LABEL = { delegates: "Delegated from", uses: "Used by", runs: "Owned by", handles: "Handled bindings", triggers: "Triggered by", needs: "Needed by" };
  const label = (kind) => L.KIND_LABEL[kind] || kind;

  function stateOf(c) {
    if (c.kind !== "scope") return null;
    const on = L.fact(c, "granted") === "true";
    return { on, text: on ? "Allowed by policy" : "Not allowed by policy" };
  }

  function chip(it) {
    const off = !!it.off, notes = [];
    if (it.ref) notes.push("Inherited from parent");
    if (it.lock) notes.push("Approval required");
    if (off) notes.push("Not allowed by policy");
    const tail = notes.length ? " — " + notes.join(", ") : "";
    return h("button", { type: "button", class: "chip" + (off ? " is-off" : "") + (it.ref ? " is-ref" : ""), "data-kind": it.kind, "data-key": it.key, title: it.name + tail, "aria-label": it.name + " (" + label(it.kind) + (notes.length ? ", " + notes.join(", ") : "") + ")" }, [icon(it.kind, { size: 14, off }), h("span", { class: "nm", text: it.name }), it.ref ? h("span", { class: "chip-tag", text: "Inherited" }) : null, it.lock ? icon("lock", { size: 12, cls: "chip-lock" }) : null]);
  }

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
          h("div", { class: "pn-kind" }, [h("b", { text: label(c.kind) }), h("span", { text: c.agent ? "Agent " + c.agent : "Profile " + c.profile })]),
          h("button", { type: "button", class: "pn-close", "aria-label": "Close details", onclick: () => o.onClose() }, [icon("close", { size: 16 })]),
        ]),
        h("h2", { class: "pn-title", text: c.name }),
        pills.length ? h("div", { class: "pn-state" }, pills) : null,
        c.summary ? h("p", { class: "pn-sum", text: c.summary }) : null,
        facts.length ? h("div", { class: "pn-sec" }, [h("h3", { text: "Facts" }), h("dl", { class: "facts" }, [].concat.apply([], facts.map((f) => [h("dt", { text: B.factLabel(c.kind, f.key) }), h("dd", null, [B.factNode(c, f)])])))]) : null,
        rel.length ? h("div", { class: "pn-sec" }, [h("h3", { text: "Connections" })].concat(rel)) : null,
        h("p", { class: "pn-foot" }, [D.text("Profile " + c.profile + " · "), h("code", { text: c.id })]),
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

  function createDock(o) {
    const el = o.el;
    let groups = [], openKind = null, pop = null, badges = {};

    function close() {
      const kind = openKind, inside = !!(pop && pop.contains && pop.contains(document.activeElement));
      openKind = null;
      if (pop && pop.parentNode) pop.parentNode.removeChild(pop);
      pop = null;
      Array.prototype.forEach.call(el.children || [], (b) => { if (b.setAttribute && b.getAttribute("data-kind")) b.setAttribute("aria-expanded", "false"); });
      // Return focus to the dock button so keyboard shortcuts (e.g. Escape) keep reaching the map.
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
      const body = [h("div", { class: "pop-head" }, [h("h3", { text: label(kind) }), h("span", { text: total + (off ? " · " + off + " disallowed" : "") })])];
      list.forEach((g) => {
        if (multi) body.push(h("p", { class: "pop-sub", text: "Profile " + g.profile }));
        body.push(h("div", { class: "pop-grid" }, g.items.map((c) => o.makeChip({ key: L.key(c), id: c.id, kind: c.kind, name: c.name, off: L.isOff(c) }))));
      });
      pop = h("div", { class: "pop enter", id: "pop-" + kind, role: "group", "aria-label": label(kind) + " list" }, body);
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
    function mark(set) {
      Array.prototype.forEach.call(el.children, (b) => {
        if (!b.getAttribute || !b.getAttribute("data-kind")) return;
        const kind = b.getAttribute("data-kind");
        const n = set ? groups.filter((g) => g.kind === kind).reduce((s, g) => s + g.items.filter((c) => set[L.key(c)]).length, 0) : 0;
        const old = badges[kind];
        if (old && old.parentNode) old.parentNode.removeChild(old);
        delete badges[kind];
        if (n) { badges[kind] = h("span", { class: "badge", title: n + " connected " + label(kind).toLowerCase() + (n === 1 ? "" : "s"), text: String(n) }); b.appendChild(badges[kind]); }
      });
    }
    const pill = (kind) => Array.prototype.filter.call(el.children || [], (b) => b.getAttribute && b.getAttribute("data-kind") === kind)[0] || null;
    return { update, open, close, mark, pill, get openKind() { return openKind; }, get el() { return el; } };
  }

  const api = { chip, createPanel, createDock, stateOf, label };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotPanel = api;
})(typeof window !== "undefined" ? window : globalThis);
