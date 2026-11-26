// 실행 탭: 실행 목록(프로필·상태 필터, 더 보기)과 상세(계보 빵부스러기·대기 사유·자식·도구 호출·효과).
// 목록은 서버가 준 순서를 지키고, 자동 새로고침은 지금까지 불러온 만큼을 다시 받는다(더 보기로 늘린 목록이 줄지 않는다).
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, { h, icon, code } = D;
  const PAGE = 30;

  function create(app, box) {
    const st = { items: [], next: null, state: "", sel: null, detail: null, detailErr: null, loaded: false, busyMore: false, epoch: 0, dkey: null };
    const controls = root.BotRunControl.create(app);
    let memo = {}, dmemo = {}, body = null;
    const selKey = () => (st.sel ? st.sel.profile + "|" + st.sel.run : null);

    const stateSel = h("select", { id: "run-state", "aria-label": "상태로 거르기", onchange: (e) => { st.state = e.target.value; clearList(); app.refresh(true); } }, [h("option", { value: "", text: "전체" })].concat(B.RUN_ORDER.map((s) => h("option", { value: s, text: B.RUN_STATE[s][0] }))));
    const count = h("p", { class: "rl-count", "aria-live": "polite" });
    const list = h("ul", { class: "rl", "aria-label": "실행 목록" });
    const none = h("div", { class: "rl-none" });
    const moreBtn = h("button", { type: "button", class: "btn", text: "더 보기", onclick: () => more() });
    const moreBox = h("div", { class: "more" }, [moreBtn]);
    // 상세에는 매초 바뀌는 «n초 전» 글자가 있어 live 영역으로 두지 않는다. 대신 실행을 열면 포커스가 상세로 옮겨 간다.
    const detail = h("section", { class: "rd-pane", "aria-label": "실행 상세", tabindex: "-1" });
    const split = h("div", { class: "split" }, [h("section", { class: "rl-pane", "aria-label": "실행" }, [count, list, none, moreBox]), detail]);
    const head = h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "실행" }), h("p", { class: "sub", text: "대화·예약·위임으로 시작된 실행의 상태와 기록" })]), h("label", { class: "field ph-act" }, [h("span", { class: "lab", text: "상태" }), h("span", { class: "sel" }, [stateSel, h("span", { class: "sel-ic" }, [icon("down", { size: 14 })])])])]);
    box.appendChild(h("div", { class: "wrap" }, [head, split]));
    box.classList.add("runs");
    renderAll();

    function clearList() { st.items = []; st.next = null; st.loaded = false; st.epoch++; memo = {}; D.clear(list); renderList(); }

    // ---------- 불러오기 ----------
    function load(ctx) {
      const limit = D.clamp(st.items.length || PAGE, PAGE, 200), epoch = st.epoch;
      const jobs = [A.get("/v1/admin/runs" + A.q({ profile: ctx.profile, state: st.state, limit }), { signal: ctx.signal })];
      if (st.sel) {
        const want = selKey();
        jobs.push(A.get("/v1/admin/runs/" + A.seg(st.sel.profile) + "/" + A.seg(st.sel.run), { signal: ctx.signal }).then((d) => ({ d, want }), (e) => { if (e.name === "AbortError") throw e; return { e, want }; }));
      }
      return Promise.all(jobs).then((r) => {
        if (!ctx.current() || epoch !== st.epoch) return;
        st.items = r[0].items; st.next = r[0].next; st.loaded = true;
        app.noteProfiles(st.items.map((x) => x.profile));
        if (r[1] && r[1].want === selKey()) { st.detail = r[1].d || null; st.detailErr = r[1].e || null; }
        renderAll();
      });
    }
    function more() {
      if (!st.next || st.busyMore) return;
      const epoch = st.epoch, profile = app.profile();
      st.busyMore = true; renderMore();
      A.get("/v1/admin/runs" + A.q({ profile, state: st.state, limit: PAGE, after: st.next })).then((d) => {
        if (epoch !== st.epoch) return;
        const have = {};
        st.items.forEach((x) => { have[x.profile + "|" + x.run] = true; });
        st.items = st.items.concat(d.items.filter((x) => !have[x.profile + "|" + x.run]));
        st.next = d.next; st.busyMore = false; renderAll();
      }).catch((e) => { st.busyMore = false; renderMore(); app.notice(A.explain(e)); });
    }

    // ---------- 선택 ----------
    // quiet: 탭을 여는 쪽이 이어서 새로고침하므로 여기서는 다시 묻지 않는다.
    function open(profile, run, quiet) {
      if (st.sel && st.sel.profile === profile && st.sel.run === run) return;
      st.sel = { profile, run }; st.detail = null; st.detailErr = null;
      app.setHash("runs/" + A.seg(profile) + "/" + A.seg(run));
      renderAll();
      if (!quiet) { app.refresh(true); detail.focus({ preventScroll: true }); }
      // 좁은 화면에서는 상세가 목록을 대신하므로 맨 위에서 시작한다.
      if (root.matchMedia && root.matchMedia("(max-width: 960px)").matches) root.scrollTo(0, 0);
    }
    // 목록으로 돌아가면 방금 보던 행에 포커스를 돌려 준다.
    function close() {
      const was = selKey();
      st.sel = null; st.detail = null; st.detailErr = null;
      app.setHash("runs");
      renderAll();
      Array.prototype.forEach.call(list.children, (li) => { if (li.firstChild.getAttribute("data-run") === was) li.firstChild.focus(); });
    }

    // ---------- 목록 ----------
    function meta(r) {
      const parts = [r.principal, B.conversation(r.kind) + (r.trigger ? " " + r.trigger : ""), "세대 " + r.generation, "시도 " + r.attempts];
      if (r.waiting) parts.push(B.waitingShort(r.waiting));
      if (r.error) parts.push("오류 " + r.error);
      return parts.join(" · ");
    }
    function row(r, multi) {
      const k = r.profile + "|" + r.run;
      return h("li", null, [h("button", { type: "button", class: "ri", "data-run": k, onclick: () => open(r.profile, r.run) }, [
        h("span", { class: "ri-main" }, [code(r.run), h("span", { class: "ri-agent", text: r.agent }), multi ? h("span", { class: "ri-prof", text: r.profile }) : null]),
        h("span", { class: "ri-state" }, [B.runPill(r.state)]),
        h("span", { class: "ri-meta", text: meta(r) }),
        D.timeEl(r.updated_ms, { cls: "ri-time" }),
      ])]);
    }
    function renderList() {
      const multi = !app.profile();
      D.sync(list, st.items.map((r) => ({ key: r.profile + "|" + r.run, sig: JSON.stringify([r, multi]), build: () => ({ node: row(r, multi) }) })), memo);
      const sel = selKey();
      Array.prototype.forEach.call(list.children, (li) => {
        const b = li.firstChild, on = b.getAttribute("data-run") === sel;
        b.classList.toggle("is-selected", on);
        if (on) b.setAttribute("aria-current", "true"); else b.removeAttribute("aria-current");
      });
      count.textContent = st.loaded ? (st.items.length ? st.items.length + "건" + (st.next ? " 이상" : "") : "") : "";
      D.clear(none);
      if (!st.loaded) none.appendChild(D.skeleton(4));
      else if (!st.items.length) none.appendChild(D.empty(st.state ? "이 상태의 실행이 없습니다" : "실행이 없습니다", st.state ? "다른 상태를 고르거나 «전체»로 돌아가 보세요." : "대화나 예약이 실행되면 여기에 보입니다.", "run"));
      renderMore();
    }
    function renderMore() {
      moreBox.hidden = !st.next;
      moreBtn.disabled = st.busyMore;
      moreBtn.textContent = st.busyMore ? "불러오는 중…" : "더 보기";
    }

    // ---------- 상세 ----------
    const line = (k, v) => [h("dt", { text: k }), h("dd", null, [v])];
    function section(title, n, kid) { return h("section", { class: "sec" }, [h("h3", { class: "sec-t" }, [h("span", { text: title }), n !== null ? h("em", { text: String(n) }) : null]), kid]); }
    function crumbs(d) {
      const last = d.lineage.length - 1;
      const items = d.lineage.map((r, i) => h("li", null, [i === last ? h("span", { class: "crumb is-now", "aria-current": "page" }, [code(r.run), h("span", { text: r.agent })]) : h("button", { type: "button", class: "crumb", title: "이 실행으로 이동", onclick: () => open(r.profile, r.run) }, [code(r.run), h("span", { text: r.agent })])]));
      return h("nav", { class: "crumbs", "aria-label": "계보(루트에서 현재까지)" }, [h("ol", null, items)]);
    }
    // 추적: 서버가 링크 틀을 알면 `trace_link`가 온다(웹 주소일 때만 연다). 없으면 추적 ID 앞 8자만 보인다.
    function traceNode(r) {
      if (!r.trace_id) return "-";
      let href = null;
      try { const u = new URL(r.trace_link); href = u.protocol === "https:" || u.protocol === "http:" ? u.href : null; } catch (e) { href = null; }
      const id = code(r.trace_id.slice(0, 8), r.trace_id);
      if (!href) return h("span", null, [id, h("span", { class: "dim", text: " (추적 링크 설정 없음)" })]);
      return h("span", { class: "trace" }, [h("a", { class: "xlink", href, target: "_blank", rel: "noreferrer noopener" }, [h("span", { text: "추적 열기" }), icon("external", { size: 12 })]), id]);
    }
    function waitLine(r, rootHold) {
      const w = B.waiting(r.waiting);
      let kids = null;
      if (w) kids = [icon("pause", { size: 14 }), w];
      else if (r.state === "retry") kids = [icon("pause", { size: 14 }), h("span", { text: "오류가 난 뒤 다시 시도할 차례를 기다리는 중입니다." })];
      else if (r.state === "dead") kids = [icon("alert", { size: 14 }), h("span", null, ["더 시도하지 않고 멈췄습니다.", r.error ? [" 마지막 오류: ", code(r.error)] : null])];
      if (!kids) return null;
      const go = r.waiting && (r.waiting.on === "approval" || (r.waiting.on === "hold" && app.me().operator && !rootHold));
      if (go) kids.push(h("button", { type: "button", class: "btn sm", text: "승인·보류에서 보기", onclick: () => app.go("decide", { profile: r.profile }) }));
      return h("p", { class: "rd-wait" }, kids);
    }
    function childList(d) {
      if (!d.children.length) return h("p", { class: "sec-h", text: "자식 실행이 없습니다." });
      return h("ul", { class: "rl rl-flat", "aria-label": "자식 실행" }, d.children.map((c) => row(c, false)));
    }
    function callTable(d) {
      return D.table(["호출", "도구", "위험 등급", "상태", "원인", "인자 해시", "시작"], d.calls.map((c) => [code(c.call), code(c.tool), B.riskPill(c.risk), B.callPill(c.state), B.callError(c.error), B.hash8(c.args_sha256), D.timeEl(c.started_ms)]), { narrow: [2, 3, 4, 5], label: "도구 호출 표", emptyTitle: "도구 호출이 없습니다", emptyIcon: "tool" });
    }
    function effectTable(d) {
      return D.table(["호출", "단계", "상태", "영수증", "사유", "시각"], d.effects.map((e) => ({ cls: e.state === "held" ? "is-held" : "", cells: [code(e.call), e.step, B.effectPill(e.state), e.receipt ? code(e.receipt) : "-", e.state === "held" ? B.holdReason(e.reason) : "-", D.timeEl(e.at_ms)] })), { narrow: [2], label: "효과 표", emptyTitle: "기록된 효과가 없습니다", emptyHint: "외부에 쓰는 도구를 부르면 단계별 영수증이 여기에 남습니다.", emptyIcon: "check" });
    }
    function sections(d) {
      const r = d.run, k = (s) => s;
      const action = root.BotRunControl.offered(d, app.me().operator);
      return [
        { key: k("head"), sig: JSON.stringify([r.run, r.state]), build: () => ({ node: h("header", { class: "rd-head" }, [h("button", { type: "button", class: "btn back", onclick: close }, [icon("back", { size: 14 }), h("span", { text: "목록" })]), h("div", { class: "rd-title" }, [h("p", { class: "ovl", text: "실행" }), h("h3", null, [code(r.run)])]), B.runPill(r.state)]) }) },
        { key: k("crumbs"), sig: JSON.stringify(d.lineage.map((x) => [x.run, x.agent])), build: () => ({ node: crumbs(d) }) },
        { key: k("wait"), sig: JSON.stringify([r.state, r.waiting, r.error, app.me().operator, action === "held"]), build: () => ({ node: waitLine(r, action === "held") || h("p", { class: "rd-wait is-none", text: "기다리는 중이 아닙니다." }) }) },
        ...(action ? [{ key: k("control"), sig: JSON.stringify([r.profile, r.run, r.state, r.generation, d.control]), build: () => ({ node: controls.mount(d, action) }) }] : []),
        { key: k("facts"), sig: JSON.stringify(r), build: () => ({ node: h("dl", { class: "facts rd-facts" }, [].concat(
          line("프로필", r.profile), line("에이전트", code(r.agent)), line("요청 주체", code(r.principal)),
          line("시작 방식", h("span", null, [B.conversation(r.kind), r.trigger ? [" · 트리거 ", code(r.trigger)] : null])),
          line("세대·시도", "세대 " + r.generation + " · 시도 " + r.attempts + "회"), line("위임 단계", r.step ? code(r.step) : "-"),
          line("만든 때", D.when(r.created_ms)), line("바뀐 때", h("span", null, [D.when(r.updated_ms), " · ", D.timeEl(r.updated_ms)])),
          line("비공개 기억 읽음", r.private_memory_read ? "예" : "아니오"), line("마지막 오류", r.error ? code(r.error) : "-"), line("추적", traceNode(r)))) }) },
        { key: k("children"), sig: JSON.stringify(d.children), build: () => ({ node: section("자식 실행", d.children.length, childList(d)) }) },
        { key: k("calls"), sig: JSON.stringify(d.calls), build: () => ({ node: section("도구 호출", d.calls.length, callTable(d)) }) },
        { key: k("effects"), sig: JSON.stringify(d.effects), build: () => ({ node: section("효과", d.effects.length, effectTable(d)) }) },
      ];
    }
    function renderDetail() {
      const key = selKey();
      if (st.dkey !== key) { D.clear(detail); dmemo = {}; body = null; st.dkey = key; }
      if (!key) { D.clear(detail); detail.appendChild(D.empty("실행을 고르세요", "목록에서 실행을 누르면 계보·도구 호출·효과가 보입니다.", "run")); st.dkey = null; return; }
      if (!st.detail) {
        D.clear(detail); dmemo = {}; body = null;
        const back = h("button", { type: "button", class: "btn back", onclick: close }, [icon("back", { size: 14 }), h("span", { text: "목록" })]);
        detail.appendChild(st.detailErr ? h("div", { class: "rd" }, [back, h("p", { class: "warnbox", text: A.explain(st.detailErr, { not_found: "이 실행을 찾을 수 없습니다. 볼 수 없는 실행이거나 이미 지워졌습니다." }) })]) : h("div", { class: "rd" }, [back, D.skeleton(4)]));
        return;
      }
      if (!body) { D.clear(detail); body = h("div", { class: "rd" }); detail.appendChild(body); }
      D.patch(body, sections(st.detail).map((s) => ({ key: s.key, sig: s.sig, build: () => s.build() })), dmemo, {});
    }
    function renderAll() { renderList(); renderDetail(); split.classList.toggle("has-detail", !!st.sel); }

    return {
      load,
      reset() { st.sel = null; st.detail = null; st.detailErr = null; clearList(); renderAll(); },
      // 다른 탭에서 넘어온 요청: { profile, run, state }
      apply(arg) {
        if (arg && arg.run) open(arg.profile, arg.run, true);
        else if (arg && arg.state !== undefined) { st.state = arg.state || ""; stateSel.value = st.state; clearList(); }
      },
      busy() { return controls.busy(); },
      show() {}, hide() { controls.hide(); },
    };
  }

  root.BotRuns = { create };
})(window);
