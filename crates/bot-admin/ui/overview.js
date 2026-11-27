// Overview home view: live system health, immediate action inbox, KPI summary, recent runs, and configured bot profiles.
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, S = root.BotStore, { h, icon, code, pill } = D;

  function sumRuns(status, scope) {
    const out = { ready: 0, leased: 0, waiting: 0, retry: 0, succeeded: 0, dead: 0, cancelled: 0, approvals: 0, holds: 0, oldest: null };
    const profiles = (status && status.profiles) || {};
    Object.keys(profiles).forEach((p) => {
      if (scope && p !== scope) return;
      const d = profiles[p] || {}, r = d.runs || {};
      B.RUN_ORDER.forEach((k) => { out[k] += r[k] || 0; });
      out.approvals += d.approvals_pending || 0;
      out.holds += d.holds || 0;
      if (d.oldest_ready_secs !== null && d.oldest_ready_secs !== undefined) {
        out.oldest = out.oldest === null ? d.oldest_ready_secs : Math.max(out.oldest, d.oldest_ready_secs);
      }
    });
    return out;
  }

  function create(app, box) {
    const st = { status: null, runs: [], pending: [], holds: [], preapprovals: [], loaded: false };
    let wrap = null;

    function load(ctx) {
      const op = app.me().operator;
      const quiet = (p) => p.catch((e) => { if (e && e.name === "AbortError") throw e; return null; });
      const jobs = [
        A.get("/v1/admin/runs" + A.q({ profile: ctx.profile, limit: 8 }), { signal: ctx.signal }),
        A.get("/v1/admin/approvals" + A.q({ profile: ctx.profile, state: "pending", limit: 20 }), { signal: ctx.signal }),
      ];
      if (op) {
        jobs.push(
          A.get("/v1/admin/status", { signal: ctx.signal }),
          quiet(A.get("/v1/admin/holds" + A.q({ profile: ctx.profile }), { signal: ctx.signal })),
          quiet(A.get("/v1/admin/preapprovals", { signal: ctx.signal })),
          quiet(S.load(ctx)),
        );
      }
      return Promise.all(jobs).then((r) => {
        if (!ctx.current()) return;
        st.runs = (r[0] && r[0].items) || [];
        st.pending = (r[1] && r[1].items) || [];
        if (op) {
          st.status = r[2] || null;
          st.holds = (r[3] && r[3].items) || [];
          const pa = root.BotPreapproval && r[4] ? root.BotPreapproval.parse(r[4]) : null;
          st.preapprovals = pa ? pa.rows : [];
          if (st.status) {
            app.noteProfiles(Object.keys(st.status.profiles || {}));
            if (app.setBuild && st.status.build) app.setBuild(st.status.build);
          }
          app.stamp((st.status && st.status.at_ms) || D.now());
        } else {
          app.noteProfiles(st.runs.map((x) => x.profile).concat(st.pending.map((x) => x.profile)));
        }
        st.loaded = true;
        pushBadges();
        render();
      });
    }

    function pushBadges() {
      if (!app.setBadges) return;
      const scope = app.profile();
      const op = app.me().operator;
      if (op && st.status) {
        const s = sumRuns(st.status, scope);
        const paused = st.preapprovals.filter((r) => (!scope || r.profile === scope) && r.paused).length;
        app.setBadges({ decide: s.approvals + s.holds, decideTone: s.holds > 0 ? "bad" : "warn", dead: s.dead, paused });
      } else {
        const mine = st.pending.filter((a) => a.approver === app.me().principal).length;
        app.setBadges({ decide: mine || st.pending.length, decideTone: "warn", dead: st.runs.filter((r) => r.state === "dead").length });
      }
    }

    function healthHero(op, s, mine, paused) {
      const urgent = (op ? s.holds : 0) + (op ? s.dead : 0);
      const attn = mine + (op ? s.approvals : 0) + paused;
      let tone = "is-ok", ic = "check", title = "All systems running normally", desc = "";
      if (urgent > 0) {
        tone = "is-bad"; ic = "alert";
        const parts = [];
        if (op && s.holds > 0) parts.push(s.holds + (s.holds === 1 ? " unresolved hold" : " unresolved holds"));
        if (op && s.dead > 0) parts.push(s.dead + (s.dead === 1 ? " dead run" : " dead runs"));
        title = "Action required — " + parts.join(" · ");
      } else if (attn > 0) {
        tone = "is-warn"; ic = "inbox";
        title = mine > 0 ? mine + (mine === 1 ? " approval waiting for you" : " approvals waiting for you") : "Items need review";
      }
      if (op && st.status) {
        const names = Object.keys(st.status.profiles || {}).sort();
        const scope = app.profile();
        desc = "Uptime " + D.uptime(st.status.process.uptime_secs) + " · " + (scope ? "Profile " + scope : names.length + (names.length === 1 ? " active profile" : " active profiles"));
      } else {
        desc = "Signed in as " + app.me().principal + " · Showing your runs and assigned approvals.";
      }
      const actions = [];
      if (mine > 0 || (op && s.holds > 0)) actions.push(h("button", { type: "button", class: "btn primary sm", onclick: () => app.go("decide") }, [icon("inbox", { size: 14 }), h("span", { text: "Open inbox" })]));
      if (op && s.dead > 0) actions.push(h("button", { type: "button", class: "btn danger sm", onclick: () => app.go("runs", { state: "dead" }) }, [icon("alert", { size: 14 }), h("span", { text: "Review dead runs" })]));
      if (op && !actions.length) actions.push(h("button", { type: "button", class: "btn sm", onclick: () => app.go("structure") }, [icon("grid", { size: 14 }), h("span", { text: "Explore bots" })]));
      return h("section", { class: "ov-hero " + tone }, [
        h("div", { class: "ov-hero-l" }, [h("span", { class: "ov-hero-ic" }, [icon(ic, { size: 20 })]), h("div", null, [h("h2", { text: title }), h("p", { text: desc })])]),
        actions.length ? h("div", { class: "ov-hero-act" }, actions) : null,
      ]);
    }

    function actionCard(title, sub, count, tone, onclick) {
      return h("button", { type: "button", class: "ov-act" + (count > 0 ? " " + tone : ""), onclick }, [
        h("span", { class: "ov-act-tx" }, [h("strong", { text: title }), h("span", { text: sub })]),
        h("span", { class: "ov-act-n" }, [D.text(String(count)), icon("chevron", { size: 15 })]),
      ]);
    }

    function kpiTile(label, value, sub, tone, onclick) {
      const attrs = { class: "kpi" + (tone ? " " + tone : "") + (onclick ? " is-link" : "") };
      if (onclick) { attrs.type = "button"; attrs.onclick = onclick; }
      return h(onclick ? "button" : "div", attrs, [
        h("span", { class: "kpi-top" }, [h("span", { text: label }), onclick ? icon("chevron", { size: 13 }) : null]),
        h("b", { text: String(value) }),
        h("span", { class: "kpi-sub", text: sub }),
      ]);
    }

    function recentRunsCard() {
      const multi = !app.profile();
      const body = st.runs.length
        ? h("ul", { class: "ov-runs" }, st.runs.slice(0, 6).map((r) => h("li", null, [
          h("button", { type: "button", class: "ov-run", onclick: () => app.go("runs", { profile: r.profile, run: r.run }) }, [
            h("span", { class: "ov-run-top" }, [code(r.run), h("span", { class: "ov-run-agent", text: r.agent }), multi ? h("span", { class: "ri-prof", text: r.profile }) : null]),
            h("span", { class: "ov-run-r" }, [B.runPill(r.state), D.timeEl(r.updated_ms)]),
            h("span", { class: "ov-run-sub", text: B.conversation(r.kind) + (r.trigger ? " · " + r.trigger : "") + (r.waiting ? " · " + B.waitingShort(r.waiting) : "") + (r.error ? " · Error: " + r.error : "") }),
          ]),
        ])))
        : h("div", { class: "ov-card-b" }, [D.empty("No runs yet", "Runs started by messages, channels, or schedules will appear here.", "run")]);
      return h("section", { class: "ov-card" }, [
        h("header", { class: "ov-card-h" }, [
          h("h3", null, [icon("run", { size: 16 }), h("span", { text: "Recent activity" })]),
          h("button", { type: "button", class: "btn sm quiet", text: "View all", onclick: () => app.go("runs") }),
        ]),
        h("div", { class: "ov-card-b is-flush" }, [body]),
      ]);
    }

    function profilesCard() {
      const scope = app.profile();
      const access = (S.map && S.map.access) || {}, nodes = (S.map && S.map.nodes) || [];
      const names = Object.keys(access).filter((p) => !scope || p === scope).sort();
      const items = names.map((p) => {
        const acc = access[p] || {}, pnodes = nodes.filter((n) => n.profile === p);
        const countOf = (k) => pnodes.filter((n) => n.kind === k).length;
        const stat = (n, one, many) => h("span", { class: "ov-stat" }, [h("b", { text: String(n) }), D.text(" " + (n === 1 ? one : many))]);
        const ps = (st.status && st.status.profiles && st.status.profiles[p]) || {};
        return h("div", { class: "ov-prof" }, [
          h("div", { class: "ov-prof-h" }, [
            h("div", { class: "ov-prof-title" }, [icon("profile", { size: 16 }), h("strong", { text: p })]),
            pill(acc.model || "-", "info", null),
          ]),
          h("p", { class: "ov-prof-meta", text: "Root agent " + (acc.root || "main") + " · Timezone " + (acc.timezone || "UTC") + " · Rev " + (acc.revision || "-") }),
          h("div", { class: "ov-prof-stats" }, [
            stat(countOf("agent"), "agent", "agents"),
            stat(countOf("binding"), "channel", "channels"),
            stat(countOf("trigger"), "trigger", "triggers"),
            stat(countOf("skill"), "skill", "skills"),
            stat(countOf("tool"), "tool", "tools"),
            (ps.approvals_pending || 0) > 0 ? pill(ps.approvals_pending + " pending", "warn", "held") : null,
            (ps.holds || 0) > 0 ? pill(ps.holds + " held", "bad", "error") : null,
          ]),
          h("div", { class: "ov-prof-act" }, [
            h("button", { type: "button", class: "btn sm", text: "Inspect setup", onclick: () => app.go("structure", { profile: p }) }),
            h("button", { type: "button", class: "btn sm quiet", text: "Runs", onclick: () => app.go("runs", { profile: p }) }),
            h("button", { type: "button", class: "btn sm quiet", text: "Access", onclick: () => app.go("access", { profile: p }) }),
          ]),
        ]);
      });
      return h("section", { class: "ov-card" }, [
        h("header", { class: "ov-card-h" }, [
          h("h3", null, [icon("profile", { size: 16 }), h("span", { text: "Active bots & profiles" })]),
          h("button", { type: "button", class: "btn sm quiet", text: "Structure", onclick: () => app.go("structure") }),
        ]),
        h("div", { class: "ov-card-b" }, [items.length ? h("div", { class: "ov-profs" }, items) : D.empty("No profiles loaded", "Activate a profile directory to manage bots here.", "profile")]),
      ]);
    }

    function automationCard(scope) {
      const rows = st.preapprovals.filter((r) => !scope || r.profile === scope);
      const body = rows.length
        ? h("div", { class: "ov-mini-list" }, rows.slice(0, 4).map((r) => h("div", { class: "ov-mini-row" }, [
          h("div", { class: "ov-mini-row-l" }, [h("strong", { text: r.trigger }), h("span", { text: r.profile + " · " + r.agent + " · " + r.used_today + " / " + r.max_per_day + " today" })]),
          r.paused ? pill("Paused", "warn", "held") : pill("Active", "ok", "on"),
        ])))
        : D.empty("No pre-approved automations", "Pre-authorized cron triggers appear here for one-click pause/resume.", "trigger");
      return h("section", { class: "ov-card" }, [
        h("header", { class: "ov-card-h" }, [
          h("h3", null, [icon("trigger", { size: 16 }), h("span", { text: "Pre-approved automations" })]),
          h("button", { type: "button", class: "btn sm quiet", text: "Manage", onclick: () => app.go("preapprovals") }),
        ]),
        h("div", { class: "ov-card-b" }, [body]),
      ]);
    }

    function runtimeCard(scope) {
      const proc = (st.status && st.status.process) || {};
      const profiles = (st.status && st.status.profiles) || {};
      const names = Object.keys(profiles).filter((p) => !scope || p === scope).sort();
      const traceOn = names.filter((p) => profiles[p] && profiles[p].obs && profiles[p].obs.enabled).length;
      const rows = [
        h("div", { class: "ov-mini-row" }, [
          h("div", { class: "ov-mini-row-l" }, [h("strong", { text: "Trace export (OTLP)" }), h("span", { text: traceOn ? traceOn + " of " + names.length + " profile(s) enabled" : "Not configured in host.json" })]),
          traceOn ? pill("Enabled", "ok", "on") : pill("Off", "off", "off"),
        ]),
        h("div", { class: "ov-mini-row" }, [
          h("div", { class: "ov-mini-row-l" }, [h("strong", { text: "Log export (OTLP)" }), h("span", { text: proc.logs && proc.logs.enabled ? (proc.logs.sent || 0) + " sent · " + (proc.logs.failed || 0) + " failed" : "Host log export disabled" })]),
          proc.logs && proc.logs.enabled ? B.circuitPill(proc.logs.circuit || "closed") : pill("Off", "off", "off"),
        ]),
        h("div", { class: "ov-mini-row" }, [
          h("div", { class: "ov-mini-row-l" }, [h("strong", { text: "Issue tracker (Sentry)" }), h("span", { text: proc.issues && proc.issues.enabled ? (proc.issues.sent || 0) + " reported" : "Issue reporting disabled" })]),
          proc.issues && proc.issues.enabled ? B.circuitPill(proc.issues.circuit || "closed") : pill("Off", "off", "off"),
        ]),
      ];
      return h("section", { class: "ov-card" }, [
        h("header", { class: "ov-card-h" }, [
          h("h3", null, [icon("activity", { size: 16 }), h("span", { text: "Observability & runtime" })]),
          h("button", { type: "button", class: "btn sm quiet", text: "Details", onclick: () => app.go("status") }),
        ]),
        h("div", { class: "ov-card-b" }, [h("div", { class: "ov-mini-list" }, rows)]),
      ]);
    }

    function render() {
      D.clear(box);
      if (!st.loaded) {
        box.appendChild(h("div", { class: "wrap" }, [D.skeleton(5)]));
        return;
      }
      const op = app.me().operator, scope = app.profile();
      const s = sumRuns(st.status, scope);
      const mine = st.pending.filter((a) => a.approver === app.me().principal).length;
      const paused = st.preapprovals.filter((r) => (!scope || r.profile === scope) && r.paused).length;
      const kids = [healthHero(op, s, mine, paused)];
      if (op) {
        kids.push(h("section", { class: "ov-inbox", "aria-label": "Action queue" }, [
          actionCard("Pending approvals", mine ? mine + " assigned to you" : "Waiting on other approvers", s.approvals, mine > 0 ? "is-accent" : "is-warn", () => app.go("decide")),
          actionCard("Unresolved holds", s.holds ? "External writes need verification" : "All write outcomes confirmed", s.holds, "is-bad", () => app.go("decide")),
          actionCard("Dead runs", s.dead ? "Exhausted retries · can resume" : "No failed runs", s.dead, "is-bad", () => app.go("runs", { state: "dead" })),
          actionCard("Paused automations", paused ? "Pre-approved triggers paused" : "All pre-approvals active", paused, "is-warn", () => app.go("preapprovals")),
        ]));
        const inProg = s.ready + s.leased + s.retry;
        const total = inProg + s.waiting + s.succeeded + s.dead + s.cancelled;
        kids.push(h("section", { class: "strip ov-kpis", "aria-label": "Run metrics" }, [
          kpiTile("Active queue", inProg, s.oldest !== null ? "Oldest wait " + s.oldest + "s" : s.leased + " running · " + s.ready + " ready", "", () => app.go("runs", { state: s.leased ? "leased" : "ready" })),
          kpiTile("Waiting", s.waiting, "Approvals, holds, or children", "", () => app.go("runs", { state: "waiting" })),
          kpiTile("Succeeded", s.succeeded, "Completed cleanly", s.succeeded > 0 ? "ok" : "", () => app.go("runs", { state: "succeeded" })),
          kpiTile("Total tracked", total, "Dead: " + s.dead + " · Cancelled: " + s.cancelled, "", () => app.go("runs")),
        ]));
        kids.push(h("div", { class: "ov-grid" }, [recentRunsCard(), profilesCard()]));
        kids.push(h("div", { class: "ov-bottom" }, [automationCard(scope), runtimeCard(scope)]));
      } else {
        kids.push(h("section", { class: "ov-inbox", "aria-label": "Your queue" }, [
          actionCard("Assigned approvals", mine ? "Ready for your decision" : "Nothing waiting on you", mine, "is-accent", () => app.go("decide")),
          actionCard("Recent runs", "Runs initiated by you", st.runs.length, "", () => app.go("runs")),
        ]));
        kids.push(recentRunsCard());
      }
      wrap = h("div", { class: "wrap" }, kids);
      box.appendChild(wrap);
    }
    render();

    return {
      load,
      reset() { st.status = null; st.runs = []; st.pending = []; st.holds = []; st.preapprovals = []; st.loaded = false; render(); },
      busy() { return false; }, show() {}, hide() {},
    };
  }

  root.BotOverview = { create };
})(window);
