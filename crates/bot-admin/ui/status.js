// Read-only Status view: runtime health, balanced per-profile run metrics, queue wait age, and compact observability exporters.
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, { h, icon, pill } = D;

  const secTitle = (title, extra) => h("h3", { class: "sec-t" }, [h("span", { text: title }), extra ? h("em", { text: extra }) : null]);
  const section = (title, extra, kids) => h("section", { class: "sec" }, [secTitle(title, extra)].concat(kids));

  function num(cells, key, value, isNew) {
    const b = h("b", { text: String(value) });
    cells[key] = b;
    if (isNew && typeof value === "number") D.countTo(b, value);
    return b;
  }
  function tile(cells, key, label, value, sub, tone, isNew, go) {
    const clickable = !!(go && Number(value) > 0);
    const attrs = { class: "kpi" + (tone ? " " + tone : "") + (clickable ? " is-link" : "") };
    if (clickable) { attrs.type = "button"; attrs.title = "View " + label.toLowerCase() + " runs"; attrs.onclick = go; }
    return h(clickable ? "button" : "div", attrs, [
      h("span", { class: "kpi-top" }, [h("span", { text: label }), clickable ? icon("chevron", { size: 13 }) : null]),
      num(cells, key, value, isNew),
      sub ? h("span", { class: "kpi-sub", text: sub }) : null,
    ]);
  }
  const link = (label, n, tone, go) => h("button", { type: "button", class: "todo" + (n > 0 ? " " + tone : ""), onclick: go }, [h("b", { text: String(n) }), h("span", { text: label }), icon("chevron", { size: 14 })]);

  function obsSection(o, isNew, title, offMsg) {
    const heading = title || "Trace export";
    if (!o || !o.enabled) {
      return h("div", { class: "obs-row sec" }, [
        h("div", { class: "obs-row-l" }, [icon("power", { size: 15 }), h("span", { text: heading }), pill("Disabled", "off", "off")]),
        h("span", { class: "obs-row-r", text: offMsg || "Configure admin.trace in host.json to export traces." }),
      ]);
    }
    const cells = {}, sent = o.sent || 0, queued = o.queued || 0, dropped = o.dropped || 0, failed = o.failed || 0;
    const kids = [h("div", { class: "strip" }, [
      tile(cells, "o.sent", "Sent", sent, "Delivered batches", "ok", isNew),
      tile(cells, "o.queued", "Queued", queued, "Pending flush", "", isNew),
      tile(cells, "o.dropped", "Dropped", dropped, "Queue full or stale", dropped > 0 ? "warn" : "", isNew),
      tile(cells, "o.failed", "Failed", failed, "Export errors", failed > 0 ? "bad" : "", isNew),
    ])];
    if (o.circuit) kids.push(h("p", { class: "inline" }, [h("span", { class: "inl-t", text: "Export circuit" }), B.circuitPill(o.circuit)]));
    kids.push(D.table(["Destination", "Kind", "Origin", "State"], (o.backends || []).map((b) => [b.name, b.kind, b.origin, B.circuitPill(b.state)]), { narrow: [3], label: heading + " destinations table", emptyTitle: "No destinations", emptyIcon: "inbox" }));
    if (o.last_error) kids.push(h("p", { class: "inline" }, [h("span", { class: "inl-t", text: "Last error class" }), pill(o.last_error, "bad", "error")]));
    return section(heading, null, kids);
  }

  function profileSections(p, d, app) {
    const k = (s) => p + ":" + s, runs = d.runs || {}, out = [];
    const to = (state) => () => app.go("runs", { profile: p, state });
    const st = (s) => runs[s] || 0;
    out.push({ key: k("head"), sig: JSON.stringify([p, d.approvals_pending, d.holds, st("dead")]), build: () => ({ cells: {}, node: h("header", { class: "prof-h" }, [
      h("span", { class: "tile" }, [icon("profile", { size: 18 })]),
      h("div", null, [h("p", { class: "ovl", text: "Profile" }), h("h3", { text: p })]),
      h("div", { class: "ph-act" }, [
        link("Pending approvals", d.approvals_pending || 0, "warn", () => app.go("decide", { profile: p })),
        link("Holds", d.holds || 0, "bad", () => app.go("decide", { profile: p })),
      ]),
    ]) }) });
    out.push({ key: k("queue"), sig: JSON.stringify([runs, d.oldest_ready_secs]), build: (isNew) => {
      const cells = {}, secs = d.oldest_ready_secs;
      const cards = [
        tile(cells, "ready", "Ready", st("ready"), secs !== null && secs !== undefined ? "Oldest wait " + secs + "s" : "No queue lag", secs > 60 ? "warn" : "", isNew, to("ready")),
        tile(cells, "leased", "Running", st("leased"), "Active leases", "", isNew, to("leased")),
        tile(cells, "waiting", "Waiting", st("waiting"), "Approvals, holds, children", st("waiting") > 0 ? "warn" : "", isNew, to("waiting")),
        tile(cells, "retry", "Retrying", st("retry"), "Scheduled retries", st("retry") > 0 ? "warn" : "", isNew, to("retry")),
        tile(cells, "succeeded", "Succeeded", st("succeeded"), "Completed runs", st("succeeded") > 0 ? "ok" : "", isNew, to("succeeded")),
        tile(cells, "dead", "Dead", st("dead"), "Cancelled: " + st("cancelled"), st("dead") > 0 ? "bad" : "", isNew, to("dead")),
      ];
      return { node: section("Run queues & outcomes", null, [h("div", { class: "strip" }, cards)]), cells };
    } });
    out.push({ key: k("obs"), sig: JSON.stringify(d.obs || null), build: (isNew) => ({ cells: {}, node: obsSection(d.obs, isNew, "Trace export (" + p + ")", "Trace export is disabled for this profile.") }) });
    return out;
  }

  function create(app, box) {
    let memo = {}, wrap = null, body = null, sub = null, live = null;
    function reset() { live = null; memo = {}; wrap = null; body = null; sub = null; D.clear(box); box.appendChild(h("div", { class: "wrap" }, [D.skeleton(5)])); }
    function render() {
      if (!live) { if (wrap || !box.firstChild) reset(); return; }
      if (!wrap) {
        D.clear(box);
        body = h("div", { class: "stack" }); sub = h("p", { class: "sub" });
        wrap = h("div", { class: "wrap" }, [h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "Status" }), sub])]), body]);
        box.appendChild(wrap);
      }
      const names = Object.keys(live.profiles || {}).sort(), shown = names.filter((p) => !app.profile() || p === app.profile());
      sub.textContent = "Uptime " + D.uptime(live.process.uptime_secs) + " · Profiles " + names.join(", ") + (live.build ? " · " + live.build.name + " " + live.build.version : "");
      let list = [];
      list.push({ key: "host:logs", sig: JSON.stringify(live.process && live.process.logs), build: (isNew) => ({ cells: {}, node: obsSection(live.process && live.process.logs, isNew, "Host log export (OTLP)", "Configure admin.logs in host.json to ship structured logs.") }) });
      list.push({ key: "host:issues", sig: JSON.stringify(live.process && live.process.issues), build: (isNew) => ({ cells: {}, node: obsSection(live.process && live.process.issues, isNew, "Host issue tracker (Sentry)", "Configure admin.issues in host.json to report errors.") }) });
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
          if (app.setBuild && d.build) app.setBuild(d.build);
          if (app.setBadges) {
            let approvals = 0, holds = 0, dead = 0;
            Object.keys(d.profiles || {}).forEach((p) => {
              if (ctx.profile && p !== ctx.profile) return;
              approvals += d.profiles[p].approvals_pending || 0;
              holds += d.profiles[p].holds || 0;
              dead += (d.profiles[p].runs && d.profiles[p].runs.dead) || 0;
            });
            app.setBadges({ decide: approvals + holds, decideTone: holds > 0 ? "bad" : "warn", dead });
          }
          app.stamp(d.at_ms);
          render();
        });
      },
      reset, busy() { return false; }, show() { render(); }, hide() {},
    };
  }

  root.BotStatus = { create };
})(window);
