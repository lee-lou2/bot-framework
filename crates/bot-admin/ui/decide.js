// Approvals & Holds view: pending approval cards, operator hold resolution forms with two-step pinning, and recent decisions.
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, P = root.BotHoldPin, { h, icon, code } = D;
  const PAGE = 20, ARM_MS = 12000, ARM_GUARD_MS = 600;
  // Matches bot_core::schedule::DELIVERY_CARD_LABEL; rendered verbatim because args_sha256 hashes this exact text.
  const DRAFT = "Delivery draft";
  const LINE = /[\u0000-\u001f\u007f-\u009f]/;
  const okText = (t) => t.length > 0 && Array.from(t).length <= 512 && !LINE.test(t);
  const flat = (rows) => [].concat.apply([], rows);

  function create(app, box) {
    const st = { pending: [], next: null, recent: [], holds: null, holdsErr: null, loaded: false, epoch: 0 };
    const ui = { busy: {}, notes: {}, forms: {} };
    let apMemo = {}, holdMemo = {}, recentMemo = {};

    const apCount = h("em"), holdCount = h("em"), recentCount = h("em");
    const apBox = h("div", { class: "cards", "aria-label": "Pending approvals" }), apNone = h("div");
    const moreBtn = h("button", { type: "button", class: "btn", text: "Load more", onclick: () => more() });
    const moreBox = h("div", { class: "more" }, [moreBtn]);
    const holdBox = h("div", { class: "cards", "aria-label": "Held calls" }), holdNone = h("div");
    const recentBody = h("div", { class: "recent-body" });
    const recent = h("details", { class: "recent" }, [h("summary", null, [h("span", { text: "Recent decisions" }), recentCount]), recentBody]);
    const holdTitle = h("h3", { class: "sec-t", id: "hold-title", tabindex: "-1" }, [h("span", { text: "Holds" }), holdCount]);
    const apTitle = h("h3", { class: "sec-t", id: "ap-title", tabindex: "-1" }, [h("span", { text: "Pending approvals" }), apCount]);
    const holdSec = h("section", { class: "sec", "aria-labelledby": "hold-title" }, [holdTitle, h("p", { class: "sec-h", text: "Calls paused because an external write outcome could not be confirmed. Verify at the destination and record a verdict: executed with a receipt, or not executed to resume the call (private DMs close and require a new request)." }), holdNone, holdBox]);
    const sub = h("p", { class: "sub" });
    const stack = h("div", { class: "stack" }, [
      h("section", { class: "sec", "aria-labelledby": "ap-title" }, [apTitle, apNone, apBox, moreBox]),
      holdSec, h("section", { class: "sec" }, [recent]),
    ]);
    box.appendChild(h("div", { class: "wrap" }, [h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "Approvals & Holds" }), sub])]), stack]));
    box.classList.add("decide");

    const opOnly = () => app.me().operator;
    function settleFocus(title, scope) {
      const a = document.activeElement;
      if (a && a !== document.body) return;
      const next = scope.querySelector(".btn.primary:not(:disabled), .rform .btn:not(:disabled)");
      (next || title).focus();
    }

    function load(ctx) {
      const epoch = st.epoch, limit = D.clamp(st.pending.length || PAGE, PAGE, 200);
      const quiet = (p) => p.catch((e) => { if (e.name === "AbortError") throw e; return { error: e }; });
      const jobs = [A.get("/v1/admin/approvals" + A.q({ profile: ctx.profile, state: "pending", limit }), { signal: ctx.signal }), A.get("/v1/admin/approvals" + A.q({ profile: ctx.profile, state: "decided", limit: 20 }), { signal: ctx.signal })];
      if (opOnly()) jobs.push(quiet(A.get("/v1/admin/holds" + A.q({ profile: ctx.profile }), { signal: ctx.signal })));
      return Promise.all(jobs).then((r) => {
        if (!ctx.current() || epoch !== st.epoch) return;
        st.pending = r[0].items; st.next = r[0].next;
        st.recent = r[1].items;
        if (r[2]) { if (r[2].error) st.holdsErr = r[2].error; else { st.holds = r[2].items; st.holdsErr = null; } }
        st.loaded = true;
        app.noteProfiles(st.pending.concat(st.recent).map((a) => a.profile).concat((st.holds || []).map((x) => x.profile)));
        render();
      });
    }
    function more() {
      if (!st.next) return;
      const epoch = st.epoch;
      moreBtn.disabled = true;
      A.get("/v1/admin/approvals" + A.q({ profile: app.profile(), state: "pending", limit: PAGE, after: st.next })).then((d) => {
        if (epoch !== st.epoch) return;
        const have = {};
        st.pending.forEach((a) => { have[a.nonce] = true; });
        st.pending = st.pending.concat(d.items.filter((a) => !have[a.nonce]));
        st.next = d.next; moreBtn.disabled = false; render();
      }).catch((e) => { moreBtn.disabled = false; app.notice(A.explain(e)); });
    }

    function runLink(profile, run) { return h("button", { type: "button", class: "linkbtn", title: "View run details", onclick: () => app.go("runs", { profile, run }) }, [code(run)]); }
    function approvalCard(a, multi) {
      const mine = a.approver === app.me().principal, busy = ui.busy[a.nonce], note = ui.notes[a.nonce], over = a.expires_ms <= D.now();
      const why = !mine ? "Only the designated approver (" + a.approver + ") can decide." : over ? "Expired. A new approval is required." : "";
      // Server also blocks approval when owner-only body content cannot be verified in full; denial stays allowed.
      const bodyWhy = !why && a.approve_blocked ? "Cannot approve because the card does not show the full " + (a.tool === "schedule.deliver" ? "delivery draft" : "memory ID and body") + ". You can still deny it." : "", shown = why || bodyWhy;
      const off = !!busy || !!why, whyId = "ap-why-" + a.nonce, tid = "ap-t-" + a.nonce;
      const btn = (allow, cls, ic, text, busyText) => h("button", { type: "button", class: "btn " + cls, disabled: off || (allow && !!bodyWhy), "aria-describedby": shown ? whyId : null, onclick: () => decide(a, allow) }, [icon(ic, { size: 14 }), h("span", { text: busy === (allow ? "allow" : "deny") ? busyText : text })]);
      return h("article", { class: "acard", "aria-labelledby": tid }, [
        h("header", { class: "ac-h" }, [h("h4", { class: "ac-title", id: tid }, [code(a.tool)]), B.riskPill(a.risk), D.untilEl(a.expires_ms, { cls: "ac-left" })]),
        h("p", { class: "ac-who" }, ["Requester ", code(a.requester), " · Approver ", code(a.approver), mine ? h("span", { class: "me", text: " (you)" }) : null]),
        a.render.length ? h("dl", { class: "facts ac-render" }, flat(a.render.map((l) => [h("dt", { text: l.label }), h("dd", null, [l.label === DRAFT ? h("pre", { class: "ac-draft", tabindex: "0", "aria-label": "Raw delivery draft", text: l.value }) : l.value])]))) : h("p", { class: "sec-h", text: a.render_hidden ? "Card body is visible only to the requester. Requester, approver, and args hash remain visible." : "No target or change details to display." }),
        h("p", { class: "ac-meta" }, [multi ? "Profile " + a.profile + " · " : null, "Args hash ", B.hash8(a.args_sha256), " · Run ", runLink(a.profile, a.run), " · Call ", code(a.call), " · Policy rev ", code(a.policy_revision), " · Tool rev ", code(a.tool_revision)]),
        h("div", { class: "ac-act" }, [btn(true, "primary", "check", "Approve", "Approving…"), btn(false, "danger", "close", "Deny", "Denying…"), shown ? h("span", { class: "ac-why", id: whyId, text: shown }) : null]),
        note ? h("p", { class: "ac-note is-" + note.tone, text: note.text }) : null,
      ]);
    }
    function decide(a, allow) {
      if (ui.busy[a.nonce]) return;
      ui.busy[a.nonce] = allow ? "allow" : "deny"; delete ui.notes[a.nonce];
      renderApprovals();
      // Include the displayed args_sha256 so the server rejects the decision if the payload changed.
      A.post("/v1/admin/approvals/" + A.seg(a.nonce) + "/decide", { allow, seen: a.args_sha256 }).then(() => {
        delete ui.busy[a.nonce];
        app.toast((allow ? "Approved" : "Denied") + " · " + a.tool, "ok");
        app.refresh(true).then(() => settleFocus(apTitle, apBox));
      }, (e) => {
        delete ui.busy[a.nonce];
        const text = A.explain(e, A.DECIDE);
        ui.notes[a.nonce] = { text, tone: "bad" };
        app.toast(text, "bad");
        renderApprovals();
        if (e.word === "stale" || e.word === "changed" || e.word === "not_found") app.refresh(true);
      });
    }
    function renderApprovals() {
      const multi = !app.profile(), me = app.me().principal;
      D.sync(apBox, st.pending.map((a) => ({ key: a.nonce, sig: JSON.stringify([a, multi, me, ui.busy[a.nonce] || "", ui.notes[a.nonce] || null, a.expires_ms <= D.now()]), build: () => ({ node: approvalCard(a, multi) }) })), apMemo, { stagger: true });
      apCount.textContent = st.loaded ? String(st.pending.length) + (st.next ? "+" : "") : "";
      D.clear(apNone);
      if (!st.loaded) apNone.appendChild(D.skeleton(3));
      else if (!st.pending.length) apNone.appendChild(D.empty("No pending approvals", "Tool calls requiring human approval will appear here.", "check"));
      moreBox.hidden = !st.next;
    }

    function renderRecent() {
      const rows = st.recent.map((a) => [a.decided_ms || a.state === "expired" ? D.timeEl(a.decided_ms || a.expires_ms) : "-", code(a.tool), B.approvalPill(a.state), a.decided_by ? code(a.decided_by) : "-", code(a.requester), B.hash8(a.args_sha256)]);
      D.patch(recentBody, [{ key: "recent", sig: JSON.stringify([st.recent, st.loaded]), build: () => ({ node: D.table(["Time", "Tool", "Outcome", "Decided by", "Requester", "Args hash"], rows, { narrow: [2, 5], label: "Recent decisions table", emptyTitle: "No decisions yet", emptyIcon: "inbox" }) }) }], recentMemo, {});
      recentCount.textContent = st.recent.length ? String(st.recent.length) : "";
    }

    const FORMS = {
      confirmed: { title: "Confirm receipt", label: "Receipt", ph: "e.g. msg-9", field: "receipt", need: "Enter a receipt on one line (1 to 512 characters)." },
      not_executed: { title: "Confirm not executed", label: "Evidence", ph: "e.g. No matching request found at destination", field: "evidence", need: "Enter evidence on one line (1 to 512 characters)." },
    };
    const FIELDS = { confirmed: FORMS.confirmed.field, not_executed: FORMS.not_executed.field };
    const KIND = {
      final: {
        confirmed: { help: "Recorded steps are already confirmed. Enter the overall call receipt once the prior tool process has exited and the full outcome is verified at the destination.", btn: "Confirm call outcome", then: "confirm the overall call outcome." },
      },
      tool: {
        confirmed: { help: "Enter the receipt ID if you verified the action executed at the destination.", btn: "Record as executed", then: "confirm as executed." },
        not_executed: { help: "Enter evidence only after confirming both that the prior tool process has exited and that no execution exists at the destination.", btn: "Record as not executed", then: "record as not executed (requires prior process exit and destination verification)." },
      },
      reply: {
        confirmed: { help: "Enter the receipt ID if you verified the reply was delivered.", btn: "Record as executed", then: "confirm as executed." },
        not_executed: { help: "Enter evidence if you verified the reply was not delivered. The same reply will be sent again from scratch.", btn: "Record as not executed", then: "record as not executed and resend the same reply." },
      },
      private: {
        confirmed: { help: "Enter the receipt ID if you verified execution at the destination. The request will close as completed.", btn: "Record as executed", then: "confirm and close the request as completed." },
        not_executed: { help: "Private DMs do not retain the message body and cannot be retried. Marking not executed closes the request so the user can submit a new one.", btn: "Close as not executed · New request needed", then: "close as not executed. It will not retry; the user must submit a new request." },
      },
    };
    const kindOf = (x) => P.receiptOnly(x) ? "final" : (KIND[x.kind] ? x.kind : "tool");
    const KIND_PILL = { final: "Final call outcome", reply: "Reply", private: "Private DM" };
    const holdKey = P.keyOf;
    function formState(x) { return ui.forms[holdKey(x)] || (ui.forms[holdKey(x)] = { arm: null, busy: false, err: "", errKind: "", text: { confirmed: "", not_executed: "" }, effect: null, holdId: null, timer: 0, pin: null }); }
    function release(f, reason) {
      clearTimeout(f.timer);
      const was = f.pin ? f.pin.kind : "confirmed";
      f.arm = null; f.pin = null; f.text = { confirmed: "", not_executed: "" };
      if (reason) { f.err = P.WHY[reason] || P.WHY.gone; f.errKind = was; }
    }
    function stepsTable(x) {
      if (!x.steps.length) return h("p", { class: "sec-h", text: "Entire call is held (no step entries)." });
      return D.table(["Step", "State", "Receipt", "Reason"], x.steps.map((s) => ({ cls: P.open(s) ? "is-held" : s.receipt ? "is-kept" : "", cells: [s.step, B.effectPill(s.state), s.receipt ? h("span", { class: "kept" }, [code(s.receipt), h("span", { class: "kept-t", text: "Preserved" })]) : "-", s.resolution === "not_executed" ? "Marked not executed" : s.state === "held" ? B.holdReason(s.reason) : "-"] })), { narrow: [1], label: "Held call steps table" });
    }
    function holdCard(x) {
      const f = formState(x), held = x.steps.filter(P.open), tid = "hold-t-" + holdKey(x).replace(/[^A-Za-z0-9_-]/g, "_"), kind = kindOf(x);
      const lastStep = kind === "tool" && held.length === 1 && !x.steps.some((s) => s.state === "started");
      let afterNotExecuted = " Resumes with the same call ID.";
      if (kind === "tool" && held.length) {
        if (!lastStep) afterNotExecuted = " Waits until the remaining steps finish.";
        else if (x.steps.some((s) => s.state === "confirmed")) afterNotExecuted = " A final call outcome card opens. Confirm the overall call receipt on that card.";
      }
      const next = (k) => kind === "tool" && k === "not_executed" ? afterNotExecuted : "";
      const check = P.reconcile(f.pin, x);
      if (!check.ok) release(f, check.reason);
      const sel = P.retarget(f.effect, f.holdId, x);
      if (sel.changed) f.text = { confirmed: "", not_executed: "" };
      f.effect = sel.effect; f.holdId = sel.holdId;
      // Server rejects not_executed retry on replies with missing_transport, so hide that form and show recovery guidance.
      const unroutedReply = kind === "reply" && x.reason === "missing_transport";
      const parts = {}, verdicts = unroutedReply ? ["confirmed"] : Object.keys(KIND[kind]);
      const unroutedNote = unroutedReply ? h("p", { class: "warnbox", text: "Cannot automatically resend because no delivery adapter is configured. Populate the delivery key in the database before resending, or deliver the message manually and close as executed." }) : null;
      const nid = "hn-" + tid;
      const noid = h("div", { class: "rf-noid", id: nid, role: "status", hidden: true }, [h("p", { class: "warnbox", text: P.WHY.noid }), h("button", { type: "button", class: "btn", onclick: () => app.refresh(true) }, [icon("arrow", { size: 14 }), h("span", { text: "Reload" })])]);
      const at = () => (f.pin ? f.pin.effect : f.effect);
      const target = () => (at() ? x.tool + " · " + (held.filter((s) => s.effect === at())[0] || {}).step + " (" + at() + ")" : x.tool + " (whole call)");
      const said = () => (f.pin ? f.pin.text : f.text[f.arm]).trim();
      const summary = (k) => "Irreversible action. For " + target() + (k === "confirmed" ? " with receipt \"" + said() + "\", " : ", ") + KIND[kind][k].then + next(k) + (k === "not_executed" ? " Evidence: " + said() : "");
      function sync() {
        const blocked = f.holdId === null || f.holdId === undefined;
        noid.hidden = !blocked;
        verdicts.forEach((k) => {
          const p = parts[k], armed = f.arm === k;
          p.input.readOnly = armed || f.busy;
          p.btn.disabled = f.busy || blocked || (!!f.arm && !armed);
          if (blocked) p.btn.setAttribute("aria-describedby", nid); else p.btn.removeAttribute("aria-describedby");
          p.btn.classList.toggle("is-armed", armed);
          p.btnText.textContent = f.busy && armed ? "Recording…" : armed ? "Press again to record" : KIND[kind][k].btn;
          p.cancel.hidden = !armed || f.busy;
          p.sum.hidden = !armed;
          if (armed) p.sum.textContent = summary(k);
          const err = f.err && f.errKind === k;
          p.err.hidden = !err; p.err.textContent = err ? f.err : "";
        });
        if (parts.select) parts.select.disabled = !!f.arm || f.busy;
      }
      f.sync = sync;
      function disarm() { clearTimeout(f.timer); f.arm = null; f.pin = null; f.sync(); }
      function wipe() { f.text = { confirmed: "", not_executed: "" }; f.err = ""; Object.keys(FORMS).forEach((k) => { if (parts[k]) parts[k].input.value = ""; }); }
      function submit(k, ev) {
        ev.preventDefault();
        if (f.busy) return;
        if (f.holdId === null || f.holdId === undefined) { sync(); return; }
        const p = parts[k], t = p.input.value.trim();
        f.text[k] = p.input.value;
        if (f.arm !== k) {
          if (!okText(t)) { f.err = FORMS[k].need; f.errKind = k; sync(); p.input.focus(); return; }
          f.err = ""; f.arm = k; f.pin = P.arm(k, f.effect, p.input.value, f.holdId); f.armedAt = Date.now(); clearTimeout(f.timer); f.timer = setTimeout(() => { if (f.arm && !f.busy) { f.arm = null; f.pin = null; f.sync(); } }, ARM_MS);
          sync(); p.btn.focus();
          return;
        }
        if (Date.now() - (f.armedAt || 0) < ARM_GUARD_MS) return;
        // Second press sends strictly what the first press pinned, never the live DOM selection or input.
        const body = P.payload(f.pin, FIELDS);
        if (!body || f.pin.kind !== k) { disarm(); return; }
        f.busy = true; f.err = ""; clearTimeout(f.timer); sync();
        A.post("/v1/admin/holds/" + A.seg(x.profile) + "/" + A.seg(x.run) + "/" + A.seg(x.call) + "/resolve", body).then((res) => {
          delete ui.forms[holdKey(x)];
          app.toast((res && res.repeated ? "Same verdict already recorded · " : "Verdict recorded · ") + x.tool, "ok");
          app.refresh(true).then(() => settleFocus(holdTitle, holdBox));
        }, (e) => {
          f.busy = false; f.arm = null; f.pin = null;
          if (P.staleWord(e.word)) wipe();
          f.err = A.explain(e, A.RESOLVE); f.errKind = k; f.sync();
          app.toast(f.err, "bad");
          if (e.word === "conflict" || e.word === "not_found") app.refresh(true);
        });
      }
      const formNode = (k) => {
        const spec = FORMS[k], fid = "hf-" + k + "-" + tid;
        const help = KIND[kind][k].help + next(k) + (k === "confirmed" && lastStep ? " Once the receipt for the last step is recorded, a final call outcome card opens. Confirm the overall call receipt on that card." : "");
        const input = h("input", { type: "text", id: fid, class: "in", maxlength: "512", autocomplete: "off", spellcheck: "false", placeholder: spec.ph, "aria-describedby": fid + "-h", oninput: (e) => { f.text[k] = e.target.value; if (f.err && f.errKind === k) { f.err = ""; sync(); } } });
        input.value = f.text[k];
        const btnText = h("span", { text: KIND[kind][k].btn }), btn = h("button", { type: "submit", class: "btn danger" }, [icon("check", { size: 14 }), btnText]);
        const cancel = h("button", { type: "button", class: "btn quiet", text: "Cancel", hidden: true, onclick: disarm });
        const sum = h("p", { class: "rf-sum", role: "status", hidden: true }), err = h("p", { class: "rf-err", role: "alert", hidden: true });
        parts[k] = { input, btn, btnText, cancel, sum, err };
        return h("form", { class: "rform", "aria-labelledby": fid + "-t", onsubmit: (ev) => submit(k, ev), onkeydown: (ev) => { if (ev.key === "Escape" && f.arm && !f.busy) { ev.stopPropagation(); disarm(); } } }, [h("h5", { id: fid + "-t", text: spec.title }), h("p", { class: "rf-help", id: fid + "-h", text: help }), h("label", { class: "rf-field", for: fid }, [h("span", { text: spec.label }), input]), sum, err, h("div", { class: "rf-act" }, [btn, cancel])]);
      };
      let pick = null;
      if (held.length > 1) {
        pick = h("label", { class: "rf-pick" }, [h("span", { text: "Step to resolve" })]);
        parts.select = h("select", { class: "in", onchange: (e) => { f.effect = e.target.value; f.holdId = P.holdIdFor(x, f.effect); wipe(); disarm(); } }, held.map((s) => h("option", { value: s.effect, text: s.step + " (" + s.effect + ")" })));
        parts.select.value = f.effect;
        pick.appendChild(parts.select);
      }
      const node = h("article", { class: "hcard", "aria-labelledby": tid }, [
        h("header", { class: "ac-h" }, [h("h4", { class: "ac-title", id: tid }, [code(x.tool)]), KIND_PILL[kind] ? D.pill(KIND_PILL[kind], "off", null) : null, D.pill(B.holdReason(x.reason), "warn", "held"), h("span", { class: "ac-left" }, ["Held ", D.timeEl(x.since_ms)])]),
        h("p", { class: "ac-meta" }, [app.profile() ? null : "Profile " + x.profile + " · ", "Run ", runLink(x.profile, x.run), " · Call ", code(x.call), " · Held at ", D.when(x.since_ms)]),
        stepsTable(x), pick, noid, unroutedNote,
        h("div", { class: "hc-forms" }, verdicts.map(formNode)),
      ]);
      sync();
      return node;
    }
    function renderHolds() {
      holdSec.hidden = !opOnly();
      if (!opOnly()) return;
      const items = st.holds || [];
      D.sync(holdBox, items.map((x) => ({ key: holdKey(x), sig: JSON.stringify(x), build: () => ({ node: holdCard(x) }) })), holdMemo, { stagger: true });
      Object.keys(ui.forms).forEach((k) => { if (!items.some((x) => holdKey(x) === k)) delete ui.forms[k]; });
      holdCount.textContent = st.holds ? String(items.length) : "";
      D.clear(holdNone);
      if (st.holdsErr) holdNone.appendChild(h("p", { class: "warnbox", text: "Could not load holds. This does not mean none exist." }));
      else if (!st.holds) holdNone.appendChild(D.skeleton(3));
      else if (!items.length) holdNone.appendChild(D.empty("No held calls", "No external writes are paused with an unknown outcome.", "check"));
    }
    function render() {
      sub.textContent = opOnly() ? "Manage pending approvals and held calls in one place." : "Shows only approvals you requested or are assigned to decide.";
      renderApprovals(); renderHolds(); renderRecent();
    }
    render();

    return {
      load,
      reset() { st.epoch++; st.pending = []; st.next = null; st.recent = []; st.holds = null; st.holdsErr = null; st.loaded = false; apMemo = {}; holdMemo = {}; recentMemo = {}; D.clear(apBox); D.clear(holdBox); D.clear(recentBody); ui.notes = {}; render(); },
      busy() { return Object.keys(ui.busy).length > 0 || Object.keys(ui.forms).some((k) => ui.forms[k].arm || ui.forms[k].busy); },
      show() {}, hide() {},
    };
  }

  root.BotDecide = { create };
})(window);
