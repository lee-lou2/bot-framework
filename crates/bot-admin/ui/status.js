// 상태 탭: 프로필마다 실행 상태 띠(대기·실행·응답 대기…), 가장 오래 대기한 시간, 승인 대기·보류 건수(눌러서 해당 탭), 추적 내보내기.
// 새로고침 때는 값이 바뀐 구역만 바꾸고 바뀐 숫자만 잠깐 강조한다. 이 화면은 읽기 전용이다(눌러서 이동하는 버튼뿐).
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, { h, icon, pill } = D;

  const secTitle = (title, extra) => h("h3", { class: "sec-t" }, [h("span", { text: title }), extra ? h("em", { text: extra }) : null]);
  const section = (title, extra, kids) => h("section", { class: "sec" }, [secTitle(title, extra)].concat(kids));

  // 숫자 한 칸: 첫 등장에는 0에서 세어 올라가고, 갱신 때는 값이 바뀐 것만 강조한다(cells에 등록).
  function num(cells, key, value, isNew) {
    const b = h("b", { text: String(value) });
    cells[key] = b;
    if (isNew && typeof value === "number") D.countTo(b, value);
    return b;
  }
  // go가 있으면 눌러서 이동하는 칸(0건이면 그냥 글자).
  function mini(cells, key, label, value, tone, isNew, go) {
    const kids = [h("span", null, [tone === "bad" ? icon("alert", { size: 12 }) : null, D.text(label)]), num(cells, key, value, isNew)];
    const cls = "mini" + (tone ? " " + tone : "");
    if (go && Number(value) > 0) return h("button", { type: "button", class: cls + " is-link", title: label + " 실행 보기", onclick: go }, kids);
    return h("div", { class: cls }, kids);
  }
  const strip = (hero, minis) => h("div", { class: "strip" }, [hero, h("div", { class: "minis" }, minis)]);
  const link = (label, n, tone, go) => h("button", { type: "button", class: "todo" + (n > 0 ? " " + tone : ""), onclick: go }, [h("b", { text: String(n) }), h("span", { text: label }), icon("chevron", { size: 14 })]);

  function obsSection(o, isNew, title, offMsg) {
    const heading = title || "추적 내보내기";
    if (!o || !o.enabled) return section(heading, null, [D.empty("꺼져 있음", offMsg || "이 프로필은 추적을 내보내지 않습니다.", "power")]);
    const cells = {}, sent = o.sent || 0, queued = o.queued || 0, dropped = o.dropped || 0, failed = o.failed || 0;
    const hero = h("div", { class: "hero" }, [num(cells, "o.sent", sent, isNew), h("span", { text: "보냄" })]);
    const kids = [strip(hero, [mini(cells, "o.queued", "큐", queued, "", isNew), mini(cells, "o.dropped", "버림", dropped, dropped > 0 ? "warn" : "", isNew), mini(cells, "o.failed", "실패", failed, failed > 0 ? "bad" : "", isNew)])];
    if (o.circuit) kids.push(h("p", { class: "inline" }, [h("span", { class: "inl-t", text: "내보내기 회로" }), B.circuitPill(o.circuit)]));
    kids.push(D.table(["목적지", "종류", "origin", "상태"], (o.backends || []).map((b) => [b.name, b.kind, b.origin, B.circuitPill(b.state)]), { narrow: [3], label: heading + " 목적지 표", emptyTitle: "목적지가 없습니다", emptyIcon: "inbox" }));
    if (o.last_error) kids.push(h("p", { class: "inline" }, [h("span", { class: "inl-t", text: "마지막 오류 분류" }), pill(o.last_error, "bad", "error")]));
    return section(heading, null, kids);
  }

  // ---------- 프로필 하나의 구역들 ----------
  function profileSections(p, d, app) {
    const k = (s) => p + ":" + s, runs = d.runs || {}, out = [];
    const to = (state) => () => app.go("runs", { profile: p, state });
    out.push({ key: k("head"), sig: p, build: () => ({ cells: {}, node: h("header", { class: "prof-h" }, [h("span", { class: "tile" }, [icon("profile", { size: 18 })]), h("div", null, [h("p", { class: "ovl", text: "프로필" }), h("h3", { text: p })])]) }) });
    out.push({ key: k("queue"), sig: JSON.stringify([runs, d.oldest_ready_secs]), build: (isNew) => {
      const cells = {}, secs = d.oldest_ready_secs, st = (s) => runs[s] || 0;
      const hero = h("button", { type: "button", class: "hero" + (st("ready") > 0 ? " is-link" : ""), title: "대기 중인 실행 보기", onclick: to("ready"), disabled: !st("ready") }, [num(cells, "ready", st("ready"), isNew), h("span", { text: B.RUN_STATE.ready[0] })]);
      const minis = ["leased", "waiting", "retry", "succeeded", "dead", "cancelled"].map((s) => mini(cells, s, B.RUN_STATE[s][0], st(s), s === "dead" && st(s) > 0 ? "bad" : "", isNew, to(s)));
      minis.push(mini(cells, "oldest", "가장 오래 대기(초)", secs === null || secs === undefined ? "-" : secs, secs > 60 ? "warn" : "", isNew));
      return { node: section("실행 상태", null, [strip(hero, minis)]), cells };
    } });
    out.push({ key: k("todo"), sig: JSON.stringify([d.approvals_pending, d.holds]), build: () => ({ cells: {}, node: h("div", { class: "todos" }, [link("승인 대기", d.approvals_pending || 0, "warn", () => app.go("decide", { profile: p })), link("보류", d.holds || 0, "bad", () => app.go("decide", { profile: p }))]) }) });
    out.push({ key: k("obs"), sig: JSON.stringify(d.obs || null), build: (isNew) => ({ cells: {}, node: obsSection(d.obs, isNew) }) });
    return out;
  }

  function create(app, box) {
    let memo = {}, wrap = null, body = null, sub = null, live = null;
    function reset() { live = null; memo = {}; wrap = null; body = null; sub = null; D.clear(box); box.appendChild(D.skeleton(5)); }
    function render() {
      if (!live) { if (wrap || !box.firstChild) reset(); return; }
      if (!wrap) {
        D.clear(box);
        body = h("div", { class: "stack" }); sub = h("p", { class: "sub" });
        wrap = h("div", { class: "wrap" }, [h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "상태" }), sub])]), body]);
        box.appendChild(wrap);
      }
      const names = Object.keys(live.profiles || {}).sort(), shown = names.filter((p) => !app.profile() || p === app.profile());
      sub.textContent = "가동 " + D.uptime(live.process.uptime_secs) + " · 프로필 " + names.join(", ") + (live.build ? " · " + live.build.name + " " + live.build.version : "");
      let list = [];
      if (live.process && live.process.logs && live.process.logs.enabled) {
        list.push({ key: "host:logs", sig: JSON.stringify(live.process.logs), build: (isNew) => ({ cells: {}, node: obsSection(live.process.logs, isNew, "로그 내보내기(OTLP)", "로그를 내보내지 않습니다.") }) });
      }
      if (live.process && live.process.issues && live.process.issues.enabled) {
        list.push({ key: "host:issues", sig: JSON.stringify(live.process.issues), build: (isNew) => ({ cells: {}, node: obsSection(live.process.issues, isNew, "이슈 트래커", "이슈를 내보내지 않습니다.") }) });
      }
      shown.forEach((p) => { list = list.concat(profileSections(p, live.profiles[p], app)); });
      D.patch(body, list, memo, { stagger: true });
    }
    render();
    return {
      load(ctx) {
        return A.get("/v1/admin/status", { signal: ctx.signal }).then((d) => {
          if (!ctx.current()) return;
          live = d;
          app.noteProfiles(Object.keys(d.profiles || {}));
          app.stamp(d.at_ms);
          render();
        });
      },
      reset, busy() { return false; }, show() { render(); }, hide() {},
    };
  }

  root.BotStatus = { create };
})(window);
