// Access view: searchable, section-filterable roles, members, policy scopes, tool approvals, secret names, and external egress.
(function (root) {
  "use strict";
  const D = root.BotDom, L = root.BotLayout, B = root.BotLabels, S = root.BotStore, { h, icon, code } = D;
  const fact = L.fact;

  const section = (title, count, body, hint) => h("section", { class: "sec" }, [h("h3", { class: "sec-t" }, [h("span", { text: title }), count !== null ? h("em", { text: String(count) }) : null]), hint ? h("p", { class: "sec-h", text: hint }) : null, body]);
  const names = (obj) => Object.keys(obj || {}).sort();
  const secretsOf = (c) => c.links.filter((l) => l.rel === "needs" && /^secret:/.test(l.to)).map((l) => l.to.replace(/^secret:/, ""));
  const external = (c) => fact(c, "external") === "true" || !!fact(c, "destination") || (fact(c, "egress") || "-") !== "-";
  const has = (hay, q) => !q || String(hay || "").toLowerCase().indexOf(q) >= 0;

  function grouped(cards, row) {
    const out = [], by = {};
    cards.forEach((c) => { const k = c.agent || "(unassigned)"; (by[k] = by[k] || []).push(c); });
    Object.keys(by).sort().forEach((k) => { out.push({ group: "Agent " + k, n: by[k].length }); by[k].forEach((c) => out.push(row(c))); });
    return out;
  }
  const policyRow = (title, hint, scopes) => [h("dt", { text: title }), h("dd", null, [scopes.length ? B.tags(scopes, true) : "None", hint && scopes.length ? h("p", { class: "sec-h", text: hint }) : null])];

  function profileSections(p, acc, cards, q, tab) {
    const k = (s) => p + ":" + s, out = [];
    const members = acc.members || [], roles = acc.roles || {}, pol = acc.policy || { scopes: [], approve: [], agents: {} };
    const tools = cards.filter((c) => c.kind === "tool");
    const roleKeys = names(roles).filter((r) => has(r + " " + roles[r].join(" "), q));
    const memList = members.filter((m) => has(m.identity + " " + m.roles.join(" "), q));
    const toolList = tools.filter((c) => has([c.name, c.agent, fact(c, "class"), fact(c, "scopes"), secretsOf(c).join(" ")].join(" "), q));
    const ext = toolList.filter(external);

    out.push({ key: k("head"), sig: JSON.stringify([p, acc.revision, acc.policy_revision, acc.timezone, acc.model, acc.root]), build: () => ({ node: h("header", { class: "prof-h" }, [h("span", { class: "tile" }, [icon("profile", { size: 18 })]), h("div", null, [h("p", { class: "ovl", text: "Profile" }), h("h3", { text: p })]), h("p", { class: "prof-facts" }, ["Revision ", code(acc.revision), " · Policy rev ", code(acc.policy_revision), " · Timezone ", acc.timezone, " · Model ", code(acc.model), " · Root ", code(acc.root)])]) }) });

    if (tab === "all" || tab === "people") {
      const rolesTbl = section("Roles & Scopes", roleKeys.length, D.table(["Role", "Scopes", "Members"], roleKeys.map((r) => [code(r), B.tags(roles[r], true), String(members.filter((m) => m.roles.indexOf(r) >= 0).length)]), { num: [2], narrow: [2], cls: "t-id", label: "Roles and scopes table", emptyTitle: q ? "No matching roles" : "No roles defined", emptyIcon: "agent" }));
      const memTbl = section("Members", memList.length, D.table(["Member", "Roles", "Scopes from roles"], memList.map((m) => {
        const scopes = {};
        m.roles.forEach((r) => (roles[r] || []).forEach((s) => { scopes[s] = true; }));
        return [code(m.identity), B.tags(m.roles, true), B.tags(names(scopes), true)];
      }), { narrow: [1], cls: "t-id", label: "Members table", emptyTitle: q ? "No matching members" : "No members registered", emptyIcon: "agent" }), "Policy allowed scopes and approval-required scopes also apply.");
      out.push({ key: k("people"), sig: JSON.stringify([roleKeys, memList, roles, q]), build: () => ({ node: h("div", { class: "grid-2" }, [rolesTbl, memTbl]) }) });
    }
    if (tab === "all" || tab === "policy") {
      const agKeys = names(pol.agents).filter((a) => has(a + " " + pol.agents[a].join(" "), q));
      out.push({ key: k("policy"), sig: JSON.stringify([pol, agKeys, q]), build: () => ({ node: section("Policy", null, h("div", { class: "pol" }, [
        h("dl", { class: "facts pol-facts" }, [].concat(policyRow("Allowed scopes", "", pol.scopes), policyRow("Scopes requiring approval", "Write tools using these scopes require human approval.", pol.approve))),
        h("h4", { class: "pol-sub", text: "Per-agent ceilings" }),
        D.table(["Agent", "Ceiling scopes"], agKeys.map((a) => [code(a), B.tags(pol.agents[a], true)]), { narrow: [0], label: "Per-agent ceilings table", emptyTitle: q ? "No matching ceilings" : "No per-agent ceilings", emptyHint: "Agents may use scopes allowed by policy within their own declared scopes.", emptyIcon: "scope" }),
      ])) }) });
    }
    if (tab === "all" || tab === "tools") {
      out.push({ key: k("tools"), sig: JSON.stringify([toolList, q]), build: () => ({ node: section("Tools & Approvals", toolList.length, D.table(["Tool", "Risk", "Approval", "Scopes", "Secrets (names)"], grouped(toolList, (c) => [code(c.name), B.riskPill(fact(c, "class")), B.approvalReq(fact(c, "approval")), B.tags(B.list(fact(c, "scopes")), true), B.tags(secretsOf(c), true)]), { narrow: [1, 2], label: "Tools and approvals table", emptyTitle: q ? "No matching tools" : "No tools registered", emptyHint: "Tool risk levels and approval requirements appear here once agents declare tools.", emptyIcon: "tool" }), "Only secret names are shown; values are never displayed.") }) });
    }
    if (tab === "all" || tab === "egress") {
      out.push({ key: k("egress"), sig: JSON.stringify([ext, q]), build: () => ({ node: section("External Egress Tools", ext.length, D.table(["Tool", "Destination", "Allowed egress hosts", "Approval"], ext.map((c) => [code(c.name), fact(c, "destination") ? B.destination(fact(c, "destination")) : "Unspecified", B.tags(B.list(fact(c, "egress")), true), B.approvalReq(fact(c, "approval"))]), { narrow: [3], label: "External egress tools table", emptyTitle: q ? "No matching egress tools" : "No external egress tools", emptyIcon: "lock" }), "Tools that send input to third parties or accept recipient and content as arguments.") }) });
    }
    return out;
  }

  function create(app, box) {
    let memo = {}, wrap = null, body = null, chipsEl = null;
    const st = { query: "", tab: "all" };
    function reset() { memo = {}; wrap = null; body = null; chipsEl = null; D.clear(box); box.appendChild(h("div", { class: "wrap" }, [D.skeleton(4)])); }
    function renderChips() {
      if (!chipsEl) return;
      D.clear(chipsEl);
      [["all", "All sections"], ["people", "Roles & Members"], ["policy", "Policy"], ["tools", "Tools & Approvals"], ["egress", "External Egress"]].forEach(([v, label]) => {
        chipsEl.appendChild(h("button", { type: "button", class: "fchip", "aria-pressed": String(st.tab === v), text: label, onclick: () => { st.tab = v; renderChips(); render(); } }));
      });
    }
    function render() {
      const m = S.map;
      if (!m.access || !m.nodes) { if (wrap || !box.firstChild) reset(); return; }
      if (!wrap) {
        D.clear(box);
        body = h("div", { class: "stack" });
        chipsEl = h("div", { class: "chips", role: "group", "aria-label": "Access section filter" });
        const searchIn = h("input", { type: "search", placeholder: "Filter roles, members, scopes, or tools…", "aria-label": "Filter access settings", value: st.query, oninput: (e) => { st.query = e.target.value; render(); } });
        const toolbar = h("div", { class: "toolbar" }, [h("label", { class: "searchbox" }, [icon("search", { size: 15 }), searchIn]), chipsEl]);
        wrap = h("div", { class: "wrap" }, [h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "Access" }), h("p", { class: "sub", text: "Who can do what — roles, policy ceilings, tool approval rules, and external egress" })])]), toolbar, body]);
        box.appendChild(wrap);
        renderChips();
      }
      const q = st.query.trim().toLowerCase();
      let list = [];
      names(m.access).forEach((p) => { list = list.concat(profileSections(p, m.access[p], m.nodes.filter((c) => c.profile === p), q, st.tab)); });
      D.patch(body, list.map((s) => ({ key: s.key, sig: s.sig, build: () => s.build() })), memo, { stagger: true });
    }
    render();
    return {
      load(ctx) { return S.load(ctx).then((r) => { if (r.stale) return; app.noteProfiles(names(S.map.access)); app.stamp(S.map.at); render(); }); },
      reset, busy() { return false; }, show() { render(); }, hide() {},
    };
  }

  root.BotAccess = { create };
})(window);
