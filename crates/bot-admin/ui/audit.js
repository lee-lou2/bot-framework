// Read-only Audit view: searchable reverse-chronological hash-chained log and chain verification.
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, { h, icon, code } = D;
  const PAGE = 50;

  function create(app, box) {
    const st = { items: [], next: null, loaded: false, busyMore: false, epoch: 0, check: null, checking: false, around: null, query: "", verdict: "" };
    let memo = {}, amemo = {};

    // Keep the ticking relative time outside the status live region to avoid every-second screen reader announcements.
    const live = h("span", { class: "verify-live", role: "status" }), ago = h("span", { class: "verify-t" });
    const result = h("p", { class: "verify" }, [live, ago]);
    const verifyBtn = h("button", { type: "button", class: "btn", onclick: () => verify() }, [icon("check", { size: 14 }), h("span", { text: "Verify chain" })]);
    const searchIn = h("input", { type: "search", placeholder: "Filter loaded entries by actor, action, target…", "aria-label": "Filter audit entries", oninput: (e) => { st.query = e.target.value; render(); } });
    const chipsBox = h("div", { class: "chips", role: "group", "aria-label": "Filter by verdict" });
    const tableBox = h("div", { class: "audit-table" }), aroundBox = h("div", { class: "audit-around" });
    const moreBtn = h("button", { type: "button", class: "btn", text: "Load more", onclick: () => more() });
    const moreBox = h("div", { class: "more" }, [moreBtn]);
    box.appendChild(h("div", { class: "wrap" }, [
      h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "Audit" }), h("p", { class: "sub", text: "Tamper-evident hash-chained activity log in reverse chronological order" })]), h("div", { class: "ph-act verify-box" }, [verifyBtn, result])]),
      h("div", { class: "toolbar" }, [h("label", { class: "searchbox" }, [icon("search", { size: 15 }), searchIn]), chipsBox]),
      h("div", { class: "stack" }, [aroundBox, tableBox, moreBox]),
    ]));

    function renderChips() {
      D.clear(chipsBox);
      [["", "All"], ["allow", "Allowed"], ["deny", "Denied"], ["confirmed", "Confirmed"], ["not_executed", "Not executed"], ["ok", "OK"]].forEach(([v, label]) => {
        chipsBox.appendChild(h("button", { type: "button", class: "fchip", "aria-pressed": String(st.verdict === v), text: label, onclick: () => { st.verdict = v; renderChips(); render(); } }));
      });
    }
    renderChips();

    function verify() {
      if (st.checking) return;
      st.checking = true; renderVerify();
      A.get("/v1/admin/audit/verify").then((d) => {
        st.check = { ok: !!d.ok, entries: d.entries, head: d.head, broken_at: d.broken_at, at: D.now() }; st.checking = false; st.around = null;
        renderVerify(); render();
        if (!st.check.ok) A.get("/v1/admin/audit" + A.q({ before: st.check.broken_at + 6, limit: 11 })).then((w) => { st.around = w.items; render(); }, () => {});
      }, (e) => { st.checking = false; renderVerify(); app.notice(A.explain(e)); });
    }
    function renderVerify() {
      verifyBtn.disabled = st.checking;
      verifyBtn.lastChild.textContent = st.checking ? "Verifying…" : "Verify chain";
      D.clear(live); D.clear(ago);
      const c = st.check;
      if (!c || st.checking) return;
      live.appendChild(c.ok ? D.pill("Valid (" + c.entries + ")", "ok", "on") : D.pill("Broken at #" + c.broken_at, "bad", "error"));
      live.appendChild(h("span", { class: "verify-t" }, [c.ok ? ["Head ", B.hash8(c.head)] : c.entries + " total"]));
      ago.appendChild(D.timeEl(c.at));
    }

    function load(ctx) {
      // Request one extra row because the server signals a next page whenever the page is full.
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

    const act = (a) => h("span", { class: "act" }, [B.AUDIT_ACTION[a] ? h("span", { text: B.AUDIT_ACTION[a] }) : null, code(a)]);
    function row(e, multi, broken) {
      const cells = [String(e.seq), h("time", { datetime: new Date(e.at_ms).toISOString(), text: D.when(e.at_ms) }), code(e.actor), act(e.action),
        h("span", { class: "tgt" }, [code(e.target), e.args_sha256 ? h("span", { class: "tgt-h" }, ["Args ", B.hash8(e.args_sha256)]) : null]), B.verdictPill(e.verdict), B.hash8(e.hash), B.hash8(e.prev)];
      if (multi) cells.splice(2, 0, e.profile || "-");
      return { cls: e.seq === broken ? "is-broken" : "", cells };
    }
    const columns = (multi) => { const head = ["Seq", "Time", "Actor", "Action", "Target", "Verdict", "Hash", "Prev hash"]; if (multi) head.splice(2, 0, "Profile"); return head; };
    function matches(e, q) {
      if (st.verdict && e.verdict !== st.verdict) return false;
      if (!q) return true;
      const hay = [e.seq, e.profile || "", e.actor, e.action, B.AUDIT_ACTION[e.action] || "", e.target, e.verdict].join(" ").toLowerCase();
      return hay.indexOf(q) >= 0;
    }
    function render() {
      const multi = !app.profile(), broken = st.check && !st.check.ok ? st.check.broken_at : null, q = st.query.trim().toLowerCase();
      const shown = (q || st.verdict) ? st.items.filter((e) => matches(e, q)) : st.items;
      const sig = JSON.stringify([shown, multi, broken, st.loaded, q, st.verdict]);
      D.patch(tableBox, [{ key: "table", sig, build: () => ({ node: st.loaded ? D.table(columns(multi), shown.map((e) => row(e, multi, broken)), { num: [0], narrow: multi ? [0, 6, 7, 8] : [0, 5, 6, 7], label: "Audit log table", emptyTitle: (q || st.verdict) ? "No matching audit entries" : "No audit entries", emptyHint: (q || st.verdict) ? "Clear filters or load more entries." : "Entries are recorded as approvals, decisions, and tool calls occur.", emptyIcon: "inbox" }) : D.skeleton(5) }) }], memo, {});
      // Chain verification spans all profiles, so always show the Profile column in the break context table.
      const near = broken !== null && st.around ? [{ key: "around", sig: JSON.stringify([st.around, broken]), build: () => ({ node: h("section", { class: "sec warnsec" }, [h("h3", { class: "sec-t" }, [h("span", { text: "Entries around break" }), h("em", { text: "#" + broken })]), h("p", { class: "sec-h", text: "The highlighted row is where hash verification first failed. This entry or a preceding entry may have been altered." }), D.table(columns(true), st.around.map((e) => row(e, true, broken)), { num: [0], narrow: [0, 6, 7, 8], label: "Entries around broken chain table" })]) }) }] : [];
      D.patch(aroundBox, near, amemo, {});
      renderMore();
    }
    function renderMore() {
      moreBox.hidden = st.next === null;
      moreBtn.disabled = st.busyMore;
      moreBtn.textContent = st.busyMore ? "Loading…" : "Load more";
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
