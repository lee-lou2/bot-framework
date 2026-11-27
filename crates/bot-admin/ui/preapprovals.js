// Operator-only Pre-approvals view: immediate emergency pause and two-step confirmed resume backed by BotPreapproval.
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, R = root.BotPreapproval, { h, icon, code } = D;
  const ARM_MS = 12000, ARM_GUARD_MS = 600, SEND_MS = 15000;
  const HELP = {
    pause: "Pausing immediately stops new occurrences and new external write grants. Result recording and hold recovery for already-started writes continue. Occurrences during a pause are skipped and resume applies from the next occurrence.",
    resume: "Currently paused. Resuming re-enables external writes without human approval cards starting from the next occurrence.",
  };

  function create(app, box) {
    const st = { rows: null, skipped: 0, err: null, epoch: 0 };
    const ui = { busy: {}, notes: {}, arm: {}, btn: {}, ids: {}, seq: 0 };
    let memo = {};

    const count = h("em"), sub = h("p", { class: "sub" }), warn = h("div"), none = h("div");
    const cards = h("div", { class: "cards pcards", "aria-label": "Pre-approvals list" });
    const title = h("h3", { class: "sec-t", id: "pa-title", tabindex: "-1" }, [h("span", { text: "Pre-approved triggers" }), count]);
    box.appendChild(h("div", { class: "wrap" }, [
      h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "Pre-approvals" }), sub])]),
      h("div", { class: "stack" }, [h("section", { class: "sec", "aria-labelledby": "pa-title" }, [title, h("p", { class: "sec-h", text: "Static triggers pre-authorized by an operator. External writes run without approval cards within the declared scope and daily cap, and can be paused per trigger." }), warn, none, cards])]),
    ]));
    box.classList.add("preapprovals");

    const idOf = (k) => ui.ids[k] || (ui.ids[k] = "pa-" + ++ui.seq);
    const armed = () => Object.keys(ui.arm);

    function disarm(k) {
      const a = ui.arm[k];
      if (a) clearTimeout(a.timer);
      delete ui.arm[k];
    }
    function refocus(k) {
      const now = document.activeElement;
      if (now && now !== document.body) return;
      const b = ui.btn[k];
      (b && box.contains(b) ? b : title).focus();
    }

    function card(r, multi) {
      const k = R.key(r), id = idOf(k), u = R.usage(r), busy = ui.busy[k], pending = ui.arm[k], note = ui.notes[k];
      const tid = id + "-t", hid = id + "-h", sid = id + "-s";
      let main;
      if (!r.paused) {
        main = h("button", { type: "button", class: "btn stop", disabled: !!busy, "aria-describedby": hid, onclick: () => send(R.arm(r, "pause")) }, [icon("pause", { size: 14 }), h("span", { text: busy === "pause" ? "Pausing…" : "Emergency pause" })]);
      } else {
        const label = busy === "resume" ? "Resuming…" : pending ? "Press again to resume" : "Resume";
        main = h("button", { type: "button", class: "btn danger" + (pending ? " is-armed" : ""), disabled: !!busy, "aria-describedby": pending ? sid : hid, onclick: () => (pending ? confirmResume(k) : armResume(r)) }, [icon("play", { size: 14 }), h("span", { text: label })]);
      }
      ui.btn[k] = main;
      const cancel = pending && !busy ? h("button", { type: "button", class: "btn quiet", text: "Cancel", onclick: () => { disarm(k); render(); refocus(k); } }) : null;
      const fill = h("i");
      fill.style.setProperty("--pct", String(u.pct));
      const sum = pending ? h("p", { class: "pc-sum", id: sid, role: "status", text: "Resuming will re-enable external writes without human approval cards starting from the next occurrence of " + r.trigger + " (" + u.left + " remaining today, " + u.max + "/day limit). Occurrences skipped while paused will not replay. Press again to resume." }) : null;
      return h("article", { class: "acard pcard" + (r.paused ? " is-paused" : ""), "aria-labelledby": tid, "data-pkey": id }, [
        h("header", { class: "ac-h" }, [
          h("h4", { class: "ac-title", id: tid }, [code(r.trigger)]),
          r.paused ? D.pill("Paused", "warn", "held") : D.pill("Active", "ok", "on"),
          u.full ? D.pill("Daily limit reached", "bad", "error") : null,
          h("span", { class: "ac-left", text: "Today " + u.used + " / " + u.max }),
        ]),
        h("p", { class: "ac-meta" }, [multi ? "Profile " + r.profile + " · " : null, "Agent ", code(r.agent), " · Grant ID ", code(r.grant_id.slice(0, 12), r.grant_id), " · Pause epoch ", code(String(r.epoch))]),
        h("div", { class: "pc-use" }, [
          h("div", { class: "meter" + (u.full ? " is-full" : ""), role: "meter", "aria-label": "External writes authorized today", "aria-valuemin": "0", "aria-valuemax": String(u.max), "aria-valuenow": String(Math.min(u.used, u.max)), "aria-valuetext": u.used + " / " + u.max }, [fill]),
          h("p", { class: "pc-use-t", text: "External writes authorized today (UTC): " + u.used + " · Remaining: " + u.left }),
        ]),
        h("p", { class: "pc-help", id: hid, text: HELP[R.offered(r)] }),
        sum,
        h("div", { class: "ac-act" }, [main, cancel]),
        note ? h("p", { class: "ac-note is-" + note.tone, role: "status", text: note.text }) : null,
      ]);
    }

    function armResume(r) {
      const k = R.key(r), pin = R.arm(r, "resume");
      if (!pin || ui.busy[k]) return;
      disarm(k);
      delete ui.notes[k];
      const a = ui.arm[k] = { pin, at: Date.now(), timer: setTimeout(() => { if (ui.arm[k] === a) { delete ui.arm[k]; render(); } }, ARM_MS) };
      render();
      refocus(k);
    }
    function confirmResume(k) {
      const a = ui.arm[k];
      if (!a || ui.busy[k] || Date.now() - a.at < ARM_GUARD_MS) return;
      send(a.pin);
    }
    // Keep row state untouched until the server replies; allow one in-flight request per row and always reload afterward.
    function send(pin) {
      if (!pin) return;
      const k = pin.key;
      if (ui.busy[k]) return;
      const epoch = st.epoch, ctl = new AbortController(), timer = setTimeout(() => ctl.abort(), SEND_MS);
      disarm(k);
      delete ui.notes[k];
      ui.busy[k] = pin.action;
      render();
      A.post(R.path(pin), R.payload(pin), { signal: ctl.signal }).then((reply) => {
        clearTimeout(timer);
        delete ui.busy[k];
        const row = R.settle(pin, reply);
        if (row && epoch === st.epoch && st.rows) st.rows = R.replace(st.rows, row);
        const text = row ? (pin.action === "pause" ? "Paused · " : "Resumed · ") + pin.trigger : R.ERR.mismatch;
        ui.notes[k] = { tone: row ? "ok" : "bad", text };
        app.toast(text, row ? "ok" : "bad");
        render();
        app.refresh(true).then(() => refocus(k));
      }, (e) => {
        clearTimeout(timer);
        delete ui.busy[k];
        const text = e && e.name === "AbortError" ? R.ERR.timeout : A.explain(e, R.ERR);
        ui.notes[k] = { tone: "bad", text };
        app.toast(text, "bad");
        render();
        app.refresh(true).then(() => refocus(k));
      });
    }

    function render() {
      const p = app.profile(), all = st.rows || [], rows = all.filter((r) => !p || r.profile === p), multi = !p, n = R.counts(rows);
      sub.textContent = st.rows && n.total ? n.total + " pre-approved · " + n.paused + " paused" : "";
      D.sync(cards, rows.map((r) => { const k = R.key(r); return { key: k, sig: JSON.stringify([r, multi, ui.busy[k] || "", ui.notes[k] || null, !!ui.arm[k]]), build: () => ({ node: card(r, multi) }) }; }), memo, { stagger: true });
      count.textContent = st.rows ? String(n.total) : "";
      D.clear(warn);
      if (st.skipped) warn.appendChild(h("p", { class: "warnbox", text: "Skipped " + st.skipped + " unreadable row(s) without action buttons. Server and UI versions may differ; reload the page." }));
      if (st.err) warn.appendChild(h("p", { class: "warnbox", text: st.err }));
      D.clear(none);
      if (!st.rows) { if (!st.err) none.appendChild(D.skeleton(3)); }
      else if (!rows.length) none.appendChild(D.empty(all.length ? "No pre-approved triggers in this profile" : "No pre-approved triggers", "Declare preapproved in triggers.yaml and have an operator explicitly activate it to appear here.", "check"));
    }
    function load(ctx) {
      const epoch = st.epoch;
      return A.get("/v1/admin/preapprovals", { signal: ctx.signal }).then((d) => {
        if (!ctx.current() || epoch !== st.epoch) return;
        const got = R.parse(d);
        if (!got) { st.err = "Skipped list response due to unexpected format. Displayed state may be outdated."; render(); return; }
        st.rows = got.rows; st.skipped = got.skipped; st.err = null;
        app.noteProfiles(got.rows.map((r) => r.profile));
        armed().forEach((k) => {
          const check = R.reconcile(ui.arm[k].pin, st.rows);
          if (check.ok) return;
          disarm(k);
          ui.notes[k] = { tone: "bad", text: R.WHY[check.reason] || R.WHY.changed };
        });
        render();
      }, (e) => {
        if (ctx.current() && epoch === st.epoch && !(e && e.name === "AbortError")) { st.err = "Failed to reload list. Displayed state may be outdated."; render(); }
        throw e;
      });
    }
    box.addEventListener("keydown", (ev) => {
      if (ev.key !== "Escape" || !armed().length) return;
      ev.stopPropagation();
      const keys = armed();
      keys.forEach(disarm);
      render();
      refocus(keys[0]);
    });
    render();

    return {
      load,
      reset() { st.epoch++; st.rows = null; st.skipped = 0; st.err = null; armed().forEach(disarm); ui.notes = {}; memo = {}; D.clear(cards); render(); },
      busy() { return Object.keys(ui.busy).length > 0 || armed().length > 0; },
      show() {},
      hide() { if (armed().length) { armed().forEach(disarm); render(); } },
    };
  }

  root.BotPreapprovals = { create };
})(window);
