// 감사 탭: 해시로 이어진 감사 기록(최신순)과 «체인 검증». 기록은 읽기 전용이고 화면에서 고칠 수 없다.
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, { h, icon, code } = D;
  const PAGE = 50;

  function create(app, box) {
    const st = { items: [], next: null, loaded: false, busyMore: false, epoch: 0, check: null, checking: false, around: null };
    let memo = {}, amemo = {};

    // 검증 결과(live 영역)와 «n초 전»(매초 바뀌므로 live 밖)을 나눈다.
    const live = h("span", { class: "verify-live", role: "status" }), ago = h("span", { class: "verify-t" });
    const result = h("p", { class: "verify" }, [live, ago]);
    const verifyBtn = h("button", { type: "button", class: "btn", onclick: () => verify() }, [icon("check", { size: 14 }), h("span", { text: "체인 검증" })]);
    const tableBox = h("div", { class: "audit-table" }), aroundBox = h("div", { class: "audit-around" });
    const moreBtn = h("button", { type: "button", class: "btn", text: "더 보기", onclick: () => more() });
    const moreBox = h("div", { class: "more" }, [moreBtn]);
    box.appendChild(h("div", { class: "wrap" }, [
      h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "감사" }), h("p", { class: "sub", text: "해시로 이어진 기록을 최신순으로 봅니다. 중간이 바뀌면 검증이 어긋난 순번을 알려 줍니다." })]), h("div", { class: "ph-act verify-box" }, [verifyBtn, result])]),
      h("div", { class: "stack" }, [aroundBox, tableBox, moreBox]),
    ]));

    // ---------- 체인 검증 ----------
    function verify() {
      if (st.checking) return;
      st.checking = true; renderVerify();
      A.get("/v1/admin/audit/verify").then((d) => {
        st.check = { ok: !!d.ok, entries: d.entries, head: d.head, broken_at: d.broken_at, at: D.now() }; st.checking = false; st.around = null;
        renderVerify(); render();
        // 어긋났다면 그 순번 둘레(앞뒤 몇 줄)를 따로 가져와 보인다: 목록의 불러온 쪽에 없을 수 있다.
        if (!st.check.ok) A.get("/v1/admin/audit" + A.q({ before: st.check.broken_at + 6, limit: 11 })).then((w) => { st.around = w.items; render(); }, () => {});
      }, (e) => { st.checking = false; renderVerify(); app.notice(A.explain(e)); });
    }
    function renderVerify() {
      verifyBtn.disabled = st.checking;
      verifyBtn.lastChild.textContent = st.checking ? "검증하는 중…" : "체인 검증";
      D.clear(live); D.clear(ago);
      const c = st.check;
      if (!c || st.checking) return;
      live.appendChild(c.ok ? D.pill("정상 " + c.entries + "건", "ok", "on") : D.pill("순번 " + c.broken_at + "에서 어긋남", "bad", "error"));
      live.appendChild(h("span", { class: "verify-t" }, [c.ok ? ["머리 ", B.hash8(c.head)] : "총 " + c.entries + "건"]));
      ago.appendChild(D.timeEl(c.at));
    }

    // ---------- 불러오기 ----------
    function load(ctx) {
      // 서버는 «꽉 찬 쪽이면 다음 쪽이 있다»고 답한다. 불러온 만큼 정확히 받으면 끝인데도 다음 쪽이 있다고 보이므로 한 줄 더 요청한다.
      const epoch = st.epoch, limit = D.clamp(st.items.length ? st.items.length + 1 : PAGE, PAGE, 200);
      return A.get("/v1/admin/audit" + A.q({ profile: ctx.profile, limit }), { signal: ctx.signal }).then((d) => {
        if (!ctx.current() || epoch !== st.epoch) return;
        st.items = d.items; st.next = d.next; st.loaded = true;
        app.noteProfiles(d.items.map((e) => e.profile).filter(Boolean));
        render();
      });
    }
    function more() {
      if (st.next === null || st.busyMore) return;
      const epoch = st.epoch;
      st.busyMore = true; renderMore();
      A.get("/v1/admin/audit" + A.q({ profile: app.profile(), limit: PAGE, before: st.next })).then((d) => {
        if (epoch !== st.epoch) return;
        st.items = st.items.concat(d.items); st.next = d.next; st.busyMore = false; render();
      }).catch((e) => { st.busyMore = false; renderMore(); app.notice(A.explain(e)); });
    }

    // ---------- 표 ----------
    const act = (a) => h("span", { class: "act" }, [B.AUDIT_ACTION[a] ? h("span", { text: B.AUDIT_ACTION[a] }) : null, code(a)]);
    function row(e, multi, broken) {
      const cells = [String(e.seq), h("time", { datetime: new Date(e.at_ms).toISOString(), text: D.when(e.at_ms) }), code(e.actor), act(e.action),
        h("span", { class: "tgt" }, [code(e.target), e.args_sha256 ? h("span", { class: "tgt-h" }, ["인자 ", B.hash8(e.args_sha256)]) : null]), B.verdictPill(e.verdict), B.hash8(e.hash), B.hash8(e.prev)];
      if (multi) cells.splice(2, 0, e.profile || "-");
      return { cls: e.seq === broken ? "is-broken" : "", cells };
    }
    const columns = (multi) => { const head = ["순번", "시각", "주체", "동작", "대상", "판정", "해시", "이전 해시"]; if (multi) head.splice(2, 0, "프로필"); return head; };
    function render() {
      const multi = !app.profile(), broken = st.check && !st.check.ok ? st.check.broken_at : null;
      const sig = JSON.stringify([st.items, multi, broken, st.loaded]);
      D.patch(tableBox, [{ key: "table", sig, build: () => ({ node: st.loaded ? D.table(columns(multi), st.items.map((e) => row(e, multi, broken)), { num: [0], narrow: multi ? [0, 6, 7, 8] : [0, 5, 6, 7], label: "감사 기록 표", emptyTitle: "감사 기록이 없습니다", emptyHint: "승인·판정·도구 호출이 일어나면 기록이 쌓입니다.", emptyIcon: "inbox" }) : D.skeleton(5) }) }], memo, {});
      // 어긋난 곳 주변: 체인 전체 기준이라 프로필 칸을 늘 보인다.
      const near = broken !== null && st.around ? [{ key: "around", sig: JSON.stringify([st.around, broken]), build: () => ({ node: h("section", { class: "sec warnsec" }, [h("h3", { class: "sec-t" }, [h("span", { text: "어긋난 곳 주변" }), h("em", { text: "순번 " + broken })]), h("p", { class: "sec-h", text: "붉게 표시한 줄에서 해시 확인이 처음 어긋났습니다. 이 기록이나 그 앞 기록이 바뀌었을 수 있습니다." }), D.table(columns(true), st.around.map((e) => row(e, true, broken)), { num: [0], narrow: [0, 6, 7, 8], label: "어긋난 곳 주변 기록 표" })]) }) }] : [];
      D.patch(aroundBox, near, amemo, {});
      renderMore();
    }
    function renderMore() {
      moreBox.hidden = st.next === null;
      moreBtn.disabled = st.busyMore;
      moreBtn.textContent = st.busyMore ? "불러오는 중…" : "더 보기";
    }
    render(); renderVerify();

    return {
      load,
      reset() { st.epoch++; st.items = []; st.next = null; st.loaded = false; st.check = null; st.around = null; memo = {}; amemo = {}; D.clear(tableBox); D.clear(aroundBox); render(); renderVerify(); },
      busy() { return false; }, show() {}, hide() {},
    };
  }

  root.BotAudit = { create };
})(window);
