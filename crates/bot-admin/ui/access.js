// 권한 탭: 프로필마다 역할→스코프, 멤버→역할, 정책(허용·승인 필요·에이전트별 상한), 도구별 위험 등급·승인 요건·스코프·비밀 이름,
// 외부 전송 도구의 전송 대상을 간결한 표로 보인다. 자료는 구조 지도(`/v1/admin/map`)의 access와 카드다. 비밀은 이름만 있고 값은 어디에도 없다.
(function (root) {
  "use strict";
  const D = root.BotDom, L = root.BotLayout, B = root.BotLabels, S = root.BotStore, { h, icon, code } = D;
  const fact = L.fact;

  const section = (title, count, body, hint) => h("section", { class: "sec" }, [h("h3", { class: "sec-t" }, [h("span", { text: title }), count !== null ? h("em", { text: String(count) }) : null]), hint ? h("p", { class: "sec-h", text: hint }) : null, body]);
  const names = (obj) => Object.keys(obj || {}).sort();
  const secretsOf = (c) => c.links.filter((l) => l.rel === "needs" && /^secret:/.test(l.to)).map((l) => l.to.replace(/^secret:/, ""));
  const external = (c) => fact(c, "external") === "true" || !!fact(c, "destination") || (fact(c, "egress") || "-") !== "-";

  // 같은 에이전트끼리 묶어 묶음 머리 행을 넣는다.
  function grouped(cards, row) {
    const out = [], by = {};
    cards.forEach((c) => { const k = c.agent || "(소속 없음)"; (by[k] = by[k] || []).push(c); });
    Object.keys(by).sort().forEach((k) => { out.push({ group: "에이전트 " + k, n: by[k].length }); by[k].forEach((c) => out.push(row(c))); });
    return out;
  }
  // 비어 있으면 «없음»만 보인다(설명은 해당하는 스코프가 있을 때만 붙는다).
  const policyRow = (title, hint, scopes) => [h("dt", { text: title }), h("dd", null, [scopes.length ? B.tags(scopes, true) : "없음", hint && scopes.length ? h("p", { class: "sec-h", text: hint }) : null])];

  function profileSections(p, acc, cards) {
    const k = (s) => p + ":" + s, out = [];
    const members = acc.members || [], roles = acc.roles || {}, pol = acc.policy || { scopes: [], approve: [], agents: {} };
    const tools = cards.filter((c) => c.kind === "tool");
    out.push({ key: k("head"), sig: JSON.stringify([p, acc.revision, acc.policy_revision, acc.timezone, acc.model, acc.root]), build: () => ({ node: h("header", { class: "prof-h" }, [h("span", { class: "tile" }, [icon("profile", { size: 18 })]), h("div", null, [h("p", { class: "ovl", text: "프로필" }), h("h3", { text: p })]), h("p", { class: "prof-facts" }, ["판본 ", code(acc.revision), " · 정책 판본 ", code(acc.policy_revision), " · 시간대 ", acc.timezone, " · 모델 ", code(acc.model), " · 루트 에이전트 ", code(acc.root)])]) }) });
    out.push({ key: k("roles"), sig: JSON.stringify([roles, members]), build: () => ({ node: section("역할과 스코프", names(roles).length, D.table(["역할", "스코프", "멤버 수"], names(roles).map((r) => [code(r), B.tags(roles[r], true), String(members.filter((m) => m.roles.indexOf(r) >= 0).length)]), { num: [2], narrow: [2], cls: "t-id", label: "역할과 스코프 표", emptyTitle: "정의된 역할이 없습니다", emptyIcon: "agent" })) }) });
    out.push({ key: k("members"), sig: JSON.stringify([roles, members]), build: () => ({ node: section("멤버", members.length, D.table(["멤버", "역할", "역할이 주는 스코프"], members.map((m) => {
      const scopes = {};
      m.roles.forEach((r) => (roles[r] || []).forEach((s) => { scopes[s] = true; }));
      return [code(m.identity), B.tags(m.roles, true), B.tags(names(scopes), true)];
    }), { narrow: [1], cls: "t-id", label: "멤버 표", emptyTitle: "등록된 멤버가 없습니다", emptyIcon: "agent" }), "정책의 허용 스코프와 승인 필요 스코프가 함께 적용됩니다.") }) });
    out.push({ key: k("policy"), sig: JSON.stringify(pol), build: () => ({ node: section("정책", null, h("div", { class: "pol" }, [
      h("dl", { class: "facts pol-facts" }, [].concat(policyRow("허용하는 스코프", "", pol.scopes), policyRow("승인이 필요한 스코프", "이 스코프를 쓰는 쓰기 도구는 승인을 받아야 합니다.", pol.approve))),
      h("h4", { class: "pol-sub", text: "에이전트별 상한" }),
      D.table(["에이전트", "허용 상한 스코프"], names(pol.agents).map((a) => [code(a), B.tags(pol.agents[a], true)]), { narrow: [0], label: "에이전트별 상한 표", emptyTitle: "에이전트별 상한이 없습니다", emptyHint: "에이전트는 자기 스코프 안에서 정책이 허용하는 만큼 쓸 수 있습니다.", emptyIcon: "scope" }),
    ])) }) });
    out.push({ key: k("tools"), sig: JSON.stringify(tools), build: () => ({ node: section("도구와 승인", tools.length, D.table(["도구", "위험 등급", "승인 요건", "스코프", "비밀(이름)"], grouped(tools, (c) => [code(c.name), B.riskPill(fact(c, "class")), B.approvalReq(fact(c, "approval")), B.tags(B.list(fact(c, "scopes")), true), B.tags(secretsOf(c), true)]), { narrow: [1, 2], label: "도구와 승인 표", emptyTitle: "등록된 도구가 없습니다", emptyHint: "에이전트가 쓰는 도구가 생기면 위험 등급·승인 요건이 여기에 보입니다.", emptyIcon: "tool" }), "비밀은 이름만 보입니다. 값은 어디에도 표시하지 않습니다.") }) });
    const ext = tools.filter(external);
    out.push({ key: k("egress"), sig: JSON.stringify(ext), build: () => ({ node: section("외부 전송 도구", ext.length, D.table(["도구", "전송 대상", "허용 외부 주소", "승인 요건"], ext.map((c) => [code(c.name), fact(c, "destination") ? B.destination(fact(c, "destination")) : "지정 없음", B.tags(B.list(fact(c, "egress")), true), B.approvalReq(fact(c, "approval"))]), { narrow: [3], label: "외부 전송 도구 표", emptyTitle: "외부로 보내는 도구가 없습니다", emptyIcon: "lock" }), "제3자에게 입력을 보내는 도구와, 받는 곳·내용이 인자로 정해지는 도구입니다.") }) });
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
        wrap = h("div", { class: "wrap" }, [h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "권한" }), h("p", { class: "sub", text: "누가 무엇을 할 수 있는지 — 역할·정책·도구 승인·외부 전송" })])]), body]);
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
