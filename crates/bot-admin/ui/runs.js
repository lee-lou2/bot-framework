// Runs view: filterable run list and detail pane (lineage, wait reason, controls, child runs, tool calls, and effects).
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, { h, icon, code } = D;
  const PAGE = 30;

  function create(app, box) {
    const st = { items: [], next: null, state: "", sel: null, detail: null, detailErr: null, loaded: false, busyMore: false, epoch: 0, dkey: null };
    const controls = root.BotRunControl.create(app);
    let memo = {}, dmemo = {}, body = null;
    const selKey = () => (st.sel ? st.sel.profile + "|" + st.sel.run : null);

    const stateSel = h("select", { id: "run-state", "aria-label": "Filter by state", onchange: (e) => { st.state = e.target.value; clearList(); app.refresh(true); } }, [h("option", { value: "", text: "All" })].concat(B.RUN_ORDER.map((s) => h("option", { value: s, text: B.RUN_STATE[s][0] }))));
    const count = h("p", { class: "rl-count", "aria-live": "polite" });
    const list = h("ul", { class: "rl", "aria-label": "Run list" });
    const none = h("div", { class: "rl-none" });
    const moreBtn = h("button", { type: "button", class: "btn", text: "Load more", onclick: () => more() });
    const moreBox = h("div", { class: "more" }, [moreBtn]);
    // Do not mark the detail pane aria-live because relative timestamps update every second; move focus on open instead.
    const detail = h("section", { class: "rd-pane", "aria-label": "Run details", tabindex: "-1" });
    const split = h("div", { class: "split" }, [h("section", { class: "rl-pane", "aria-label": "Runs" }, [count, list, none, moreBox]), detail]);
    const head = h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "Runs" }), h("p", { class: "sub", text: "Status and history of runs started by messages, schedules, or delegations" })]), h("label", { class: "field ph-act" }, [h("span", { class: "lab", text: "State" }), h("span", { class: "sel" }, [stateSel, h("span", { class: "sel-ic" }, [icon("down", { size: 14 })])])])]);
    box.appendChild(h("div", { class: "wrap" }, [head, split]));
    box.classList.add("runs");
    renderAll();

    function clearList() { st.items = []; st.next = null; st.loaded = false; st.epoch++; memo = {}; D.clear(list); renderList(); }

    function load(ctx) {
      const limit = D.clamp(st.items.length || PAGE, PAGE, 200), epoch = st.epoch;
      const jobs = [A.get("/v1/admin/runs" + A.q({ profile: ctx.profile, state: st.state, limit }), { signal: ctx.signal })];
      if (st.sel) {
        const want = selKey();
        jobs.push(A.get("/v1/admin/runs/" + A.seg(st.sel.profile) + "/" + A.seg(st.sel.run), { signal: ctx.signal }).then((d) => ({ d, want }), (e) => { if (e.name === "AbortError") throw e; return { e, want }; }));
      }
      return Promise.all(jobs).then((r) => {
        if (!ctx.current() || epoch !== st.epoch) return;
        if (!ctx.manual && controls.busy()) return;
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

    const runHash = () => (st.sel ? "runs/" + A.seg(st.sel.profile) + "/" + A.seg(st.sel.run) : "runs");
    function clearSel() {
      if (!st.sel) return;
      st.sel = null; st.detail = null; st.detailErr = null;
      renderAll();
    }
    function open(profile, run, quiet) {
      if (st.sel && st.sel.profile === profile && st.sel.run === run) {
        app.setHash(runHash());
        return;
      }
      st.sel = { profile, run }; st.detail = null; st.detailErr = null;
      app.setHash(runHash());
      renderAll();
      if (!quiet) { app.refresh(true); detail.focus({ preventScroll: true }); }
      if (root.matchMedia && root.matchMedia("(max-width: 960px)").matches) root.scrollTo(0, 0);
    }
    function close() {
      const was = selKey();
      st.sel = null; st.detail = null; st.detailErr = null;
      app.setHash("runs");
      renderAll();
      Array.prototype.forEach.call(list.children, (li) => { if (li.firstChild.getAttribute("data-run") === was) li.firstChild.focus(); });
    }

    function meta(r) {
      const parts = [r.principal, B.conversation(r.kind) + (r.trigger ? " " + r.trigger : ""), "gen " + r.generation, "attempt " + r.attempts];
      if (r.waiting) parts.push(B.waitingShort(r.waiting));
      if (r.error) parts.push("error " + r.error);
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
      count.textContent = st.loaded ? (st.items.length ? st.items.length + (st.next ? "+" : "") + " runs" : "") : "";
      D.clear(none);
      if (!st.loaded) none.appendChild(D.skeleton(4));
      else if (!st.items.length) none.appendChild(D.empty(st.state ? "No runs in this state" : "No runs yet", st.state ? "Choose another state or return to All." : "Runs started by conversations or schedules will appear here.", "run"));
      renderMore();
    }
    function renderMore() {
      moreBox.hidden = !st.next;
      moreBtn.disabled = st.busyMore;
      moreBtn.textContent = st.busyMore ? "Loading…" : "Load more";
    }

    const line = (k, v) => [h("dt", { text: k }), h("dd", null, [v])];
    function section(title, n, kid) { return h("section", { class: "sec" }, [h("h3", { class: "sec-t" }, [h("span", { text: title }), n !== null ? h("em", { text: String(n) }) : null]), kid]); }
    function crumbs(d) {
      const last = d.lineage.length - 1;
      const items = d.lineage.map((r, i) => h("li", null, [i === last ? h("span", { class: "crumb is-now", "aria-current": "page" }, [code(r.run), h("span", { text: r.agent })]) : h("button", { type: "button", class: "crumb", title: "Open run", onclick: () => open(r.profile, r.run) }, [code(r.run), h("span", { text: r.agent })])]));
      return h("nav", { class: "crumbs", "aria-label": "Lineage (root to current)" }, [h("ol", null, items)]);
    }
    // Only allow http/https URLs for external trace links.
    function traceNode(r) {
      if (!r.trace_id) return "-";
      let href = null;
      try { const u = new URL(r.trace_link); href = u.protocol === "https:" || u.protocol === "http:" ? u.href : null; } catch (e) { href = null; }
      const id = code(r.trace_id.slice(0, 8), r.trace_id);
      if (!href) return h("span", null, [id, h("span", { class: "dim", text: " (no trace link configured)" })]);
      return h("span", { class: "trace" }, [h("a", { class: "xlink", href, target: "_blank", rel: "noreferrer noopener" }, [h("span", { text: "Open trace" }), icon("external", { size: 12 })]), id]);
    }
    function waitLine(r, rootHold) {
      const w = B.waiting(r.waiting);
      let kids = null;
      if (w) kids = [icon("pause", { size: 14 }), w];
      else if (r.state === "retry") kids = [icon("pause", { size: 14 }), h("span", { text: "Waiting for next retry after an error." })];
      else if (r.state === "dead") kids = [icon("alert", { size: 14 }), h("span", null, ["Stopped after exhausting retries.", r.error ? [" Last error: ", code(r.error)] : null])];
      if (!kids) return null;
      const go = r.waiting && (r.waiting.on === "approval" || (r.waiting.on === "hold" && app.me().operator && !rootHold));
      if (go) kids.push(h("button", { type: "button", class: "btn sm", text: "View in Approvals & Holds", onclick: () => app.go("decide", { profile: r.profile }) }));
      return h("p", { class: "rd-wait" }, kids);
    }
    function childList(d) {
      if (!d.children.length) return h("p", { class: "sec-h", text: "No child runs." });
      return h("ul", { class: "rl rl-flat", "aria-label": "Child runs" }, d.children.map((c) => row(c, false)));
    }
    function callTable(d) {
      return D.table(["Call", "Tool", "Risk", "State", "Error", "Args hash", "Started"], d.calls.map((c) => [code(c.call), code(c.tool), B.riskPill(c.risk), B.callPill(c.state), B.callError(c.error), B.hash8(c.args_sha256), D.timeEl(c.started_ms)]), { narrow: [2, 3, 4, 5], label: "Tool calls table", emptyTitle: "No tool calls", emptyIcon: "tool" });
    }
    function effectTable(d) {
      return D.table(["Call", "Step", "State", "Receipt", "Reason", "Time"], d.effects.map((e) => ({ cls: e.state === "held" ? "is-held" : "", cells: [code(e.call), e.step, B.effectPill(e.state), e.receipt ? code(e.receipt) : "-", e.state === "held" ? B.holdReason(e.reason) : "-", D.timeEl(e.at_ms)] })), { narrow: [2], label: "Effects table", emptyTitle: "No recorded effects", emptyHint: "Step receipts appear here when tools perform external writes.", emptyIcon: "check" });
    }
    function sections(d) {
      const r = d.run, k = (s) => s;
      const action = root.BotRunControl.offered(d, app.me().operator);
      return [
        { key: k("head"), sig: JSON.stringify([r.run, r.state]), build: () => ({ node: h("header", { class: "rd-head" }, [h("button", { type: "button", class: "btn back", onclick: close }, [icon("back", { size: 14 }), h("span", { text: "List" })]), h("div", { class: "rd-title" }, [h("p", { class: "ovl", text: "Run" }), h("h3", null, [code(r.run)])]), B.runPill(r.state)]) }) },
        { key: k("crumbs"), sig: JSON.stringify(d.lineage.map((x) => [x.run, x.agent])), build: () => ({ node: crumbs(d) }) },
        { key: k("wait"), sig: JSON.stringify([r.state, r.waiting, r.error, app.me().operator, action === "held"]), build: () => ({ node: waitLine(r, action === "held") || h("p", { class: "rd-wait is-none", text: "Not waiting." }) }) },
        ...(action ? [{ key: k("control"), sig: JSON.stringify([r.profile, r.run, r.state, r.generation, d.control]), build: () => ({ node: controls.mount(d, action) }) }] : []),
        { key: k("facts"), sig: JSON.stringify(r), build: () => ({ node: h("dl", { class: "facts rd-facts" }, [].concat(
          line("Profile", r.profile), line("Agent", code(r.agent)), line("Principal", code(r.principal)),
          line("Origin", h("span", null, [B.conversation(r.kind), r.trigger ? [" · Trigger ", code(r.trigger)] : null])),
          line("Gen & attempts", "Gen " + r.generation + " · " + r.attempts + (r.attempts === 1 ? " attempt" : " attempts")), line("Delegation step", r.step ? code(r.step) : "-"),
          line("Created", D.when(r.created_ms)), line("Updated", h("span", null, [D.when(r.updated_ms), " · ", D.timeEl(r.updated_ms)])),
          line("Private memory read", r.private_memory_read ? "Yes" : "No"), line("Last error", r.error ? code(r.error) : "-"), line("Trace", traceNode(r)))) }) },
        { key: k("children"), sig: JSON.stringify(d.children), build: () => ({ node: section("Child runs", d.children.length, childList(d)) }) },
        { key: k("calls"), sig: JSON.stringify(d.calls), build: () => ({ node: section("Tool calls", d.calls.length, callTable(d)) }) },
        { key: k("effects"), sig: JSON.stringify(d.effects), build: () => ({ node: section("Effects", d.effects.length, effectTable(d)) }) },
      ];
    }
    function renderDetail() {
      const key = selKey();
      if (st.dkey !== key) { D.clear(detail); dmemo = {}; body = null; st.dkey = key; }
      if (!key) { D.clear(detail); detail.appendChild(D.empty("Select a run", "Pick a run from the list to inspect its lineage, tool calls, and effects.", "run")); st.dkey = null; return; }
      if (!st.detail) {
        D.clear(detail); dmemo = {}; body = null;
        const back = h("button", { type: "button", class: "btn back", onclick: close }, [icon("back", { size: 14 }), h("span", { text: "List" })]);
        detail.appendChild(st.detailErr ? h("div", { class: "rd" }, [back, h("p", { class: "warnbox", text: A.explain(st.detailErr, { not_found: "Run not found. It may not be visible to you or may have been removed." }) })]) : h("div", { class: "rd" }, [back, D.skeleton(4)]));
        return;
      }
      if (!body) { D.clear(detail); body = h("div", { class: "rd" }); detail.appendChild(body); }
      D.patch(body, sections(st.detail).map((s) => ({ key: s.key, sig: s.sig, build: () => s.build() })), dmemo, {});
    }
    function renderAll() { renderList(); renderDetail(); split.classList.toggle("has-detail", !!st.sel); }

    return {
      load,
      reset() { st.sel = null; st.detail = null; st.detailErr = null; clearList(); renderAll(); },
      apply(arg) {
        if (arg && arg.run) { open(arg.profile, arg.run, true); return; }
        clearSel();
        if (arg && arg.state !== undefined) { st.state = arg.state || ""; stateSel.value = st.state; clearList(); }
      },
      hash() { return runHash(); },
      selected() { return !!st.sel; },
      busy() { return controls.busy(); },
      show() {}, hide() { controls.hide(); },
    };
  }

  root.BotRuns = { create };
})(window);
