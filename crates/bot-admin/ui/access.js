// Access view: roles, members, policy scopes, tool approvals, secret names, and external egress tables from /v1/admin/map.
(function (root) {
  "use strict";
  const D = root.BotDom, L = root.BotLayout, B = root.BotLabels, S = root.BotStore, { h, icon, code } = D;
  const fact = L.fact;

  const section = (title, count, body, hint) => h("section", { class: "sec" }, [h("h3", { class: "sec-t" }, [h("span", { text: title }), count !== null ? h("em", { text: String(count) }) : null]), hint ? h("p", { class: "sec-h", text: hint }) : null, body]);
  const names = (obj) => Object.keys(obj || {}).sort();
  const secretsOf = (c) => c.links.filter((l) => l.rel === "needs" && /^secret:/.test(l.to)).map((l) => l.to.replace(/^secret:/, ""));
  const external = (c) => fact(c, "external") === "true" || !!fact(c, "destination") || (fact(c, "egress") || "-") !== "-";

  function grouped(cards, row) {
    const out = [], by = {};
    cards.forEach((c) => { const k = c.agent || "(unassigned)"; (by[k] = by[k] || []).push(c); });
    Object.keys(by).sort().forEach((k) => { out.push({ group: "Agent " + k, n: by[k].length }); by[k].forEach((c) => out.push(row(c))); });
    return out;
  }
  const policyRow = (title, hint, scopes) => [h("dt", { text: title }), h("dd", null, [scopes.length ? B.tags(scopes, true) : "None", hint && scopes.length ? h("p", { class: "sec-h", text: hint }) : null])];

  function profileSections(p, acc, cards) {
    const k = (s) => p + ":" + s, out = [];
    const members = acc.members || [], roles = acc.roles || {}, pol = acc.policy || { scopes: [], approve: [], agents: {} };
    const tools = cards.filter((c) => c.kind === "tool");
    out.push({ key: k("head"), sig: JSON.stringify([p, acc.revision, acc.policy_revision, acc.timezone, acc.model, acc.root]), build: () => ({ node: h("header", { class: "prof-h" }, [h("span", { class: "tile" }, [icon("profile", { size: 18 })]), h("div", null, [h("p", { class: "ovl", text: "Profile" }), h("h3", { text: p })]), h("p", { class: "prof-facts" }, ["Revision ", code(acc.revision), " · Policy revision ", code(acc.policy_revision), " · Timezone ", acc.timezone, " · Model ", code(acc.model), " · Root agent ", code(acc.root)])]) }) });
    out.push({ key: k("roles"), sig: JSON.stringify([roles, members]), build: () => ({ node: section("Roles & Scopes", names(roles).length, D.table(["Role", "Scopes", "Members"], names(roles).map((r) => [code(r), B.tags(roles[r], true), String(members.filter((m) => m.roles.indexOf(r) >= 0).length)]), { num: [2], narrow: [2], cls: "t-id", label: "Roles and scopes table", emptyTitle: "No roles defined", emptyIcon: "agent" })) }) });
    out.push({ key: k("members"), sig: JSON.stringify([roles, members]), build: () => ({ node: section("Members", members.length, D.table(["Member", "Roles", "Scopes from roles"], members.map((m) => {
      const scopes = {};
      m.roles.forEach((r) => (roles[r] || []).forEach((s) => { scopes[s] = true; }));
      return [code(m.identity), B.tags(m.roles, true), B.tags(names(scopes), true)];
    }), { narrow: [1], cls: "t-id", label: "Members table", emptyTitle: "No members registered", emptyIcon: "agent" }), "Policy allowed scopes and approval-required scopes also apply.") }) });
    out.push({ key: k("policy"), sig: JSON.stringify(pol), build: () => ({ node: section("Policy", null, h("div", { class: "pol" }, [
      h("dl", { class: "facts pol-facts" }, [].concat(policyRow("Allowed scopes", "", pol.scopes), policyRow("Scopes requiring approval", "Write tools using these scopes require approval.", pol.approve))),
      h("h4", { class: "pol-sub", text: "Per-agent ceilings" }),
      D.table(["Agent", "Ceiling scopes"], names(pol.agents).map((a) => [code(a), B.tags(pol.agents[a], true)]), { narrow: [0], label: "Per-agent ceilings table", emptyTitle: "No per-agent ceilings", emptyHint: "Agents may use scopes allowed by policy within their own declared scopes.", emptyIcon: "scope" }),
    ])) }) });
    out.push({ key: k("tools"), sig: JSON.stringify(tools), build: () => ({ node: section("Tools & Approvals", tools.length, D.table(["Tool", "Risk", "Approval", "Scopes", "Secrets (names)"], grouped(tools, (c) => [code(c.name), B.riskPill(fact(c, "class")), B.approvalReq(fact(c, "approval")), B.tags(B.list(fact(c, "scopes")), true), B.tags(secretsOf(c), true)]), { narrow: [1, 2], label: "Tools and approvals table", emptyTitle: "No tools registered", emptyHint: "Tool risk levels and approval requirements will appear here once agents declare tools.", emptyIcon: "tool" }), "Only secret names are shown; values are never displayed.") }) });
    const ext = tools.filter(external);
    out.push({ key: k("egress"), sig: JSON.stringify(ext), build: () => ({ node: section("External Egress Tools", ext.length, D.table(["Tool", "Destination", "Allowed egress hosts", "Approval"], ext.map((c) => [code(c.name), fact(c, "destination") ? B.destination(fact(c, "destination")) : "Unspecified", B.tags(B.list(fact(c, "egress")), true), B.approvalReq(fact(c, "approval"))]), { narrow: [3], label: "External egress tools table", emptyTitle: "No external egress tools", emptyIcon: "lock" }), "Tools that send input to third parties or accept recipient and content as arguments.") }) });
    return out;
  }

  function create(app, box) {
    let memo = {}, wrap = null, body = null;
    function reset() { memo = {}; wrap = null; body = null; D.clear(box); box.appendChild(D.skeleton(4)); }
    function render() {
      const m = S.map;
      if (!m.access || !m.nodes) { if (wrap || !box.firstChild) reset(); return; }
      if (!wrap) {
        D.clear(box);
        body = h("div", { class: "stack" });
        wrap = h("div", { class: "wrap" }, [h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "Access" }), h("p", { class: "sub", text: "Who can do what — roles, policy, tool approvals, and external egress" })])]), body]);
        box.appendChild(wrap);
      }
      let list = [];
      names(m.access).forEach((p) => { list = list.concat(profileSections(p, m.access[p], m.nodes.filter((c) => c.profile === p))); });
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
