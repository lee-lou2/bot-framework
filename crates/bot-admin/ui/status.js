// Read-only Status view: per-profile run queues, approval/hold counters, and observability exporters.
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
  function mini(cells, key, label, value, tone, isNew, go) {
    const kids = [h("span", null, [tone === "bad" ? icon("alert", { size: 12 }) : null, D.text(label)]), num(cells, key, value, isNew)];
    const cls = "mini" + (tone ? " " + tone : "");
    if (go && Number(value) > 0) return h("button", { type: "button", class: cls + " is-link", title: "View " + label.toLowerCase() + " runs", onclick: go }, kids);
    return h("div", { class: cls }, kids);
  }
  const strip = (hero, minis) => h("div", { class: "strip" }, [hero, h("div", { class: "minis" }, minis)]);
  const link = (label, n, tone, go) => h("button", { type: "button", class: "todo" + (n > 0 ? " " + tone : ""), onclick: go }, [h("b", { text: String(n) }), h("span", { text: label }), icon("chevron", { size: 14 })]);

  function obsSection(o, isNew, title, offMsg) {
    const heading = title || "Trace export";
    if (!o || !o.enabled) return section(heading, null, [D.empty("Disabled", offMsg || "Trace export is disabled for this profile.", "power")]);
    const cells = {}, sent = o.sent || 0, queued = o.queued || 0, dropped = o.dropped || 0, failed = o.failed || 0;
    const hero = h("div", { class: "hero" }, [num(cells, "o.sent", sent, isNew), h("span", { text: "Sent" })]);
    const kids = [strip(hero, [mini(cells, "o.queued", "Queued", queued, "", isNew), mini(cells, "o.dropped", "Dropped", dropped, dropped > 0 ? "warn" : "", isNew), mini(cells, "o.failed", "Failed", failed, failed > 0 ? "bad" : "", isNew)])];
    if (o.circuit) kids.push(h("p", { class: "inline" }, [h("span", { class: "inl-t", text: "Export circuit" }), B.circuitPill(o.circuit)]));
    kids.push(D.table(["Destination", "Kind", "Origin", "State"], (o.backends || []).map((b) => [b.name, b.kind, b.origin, B.circuitPill(b.state)]), { narrow: [3], label: heading + " destinations table", emptyTitle: "No destinations", emptyIcon: "inbox" }));
    if (o.last_error) kids.push(h("p", { class: "inline" }, [h("span", { class: "inl-t", text: "Last error class" }), pill(o.last_error, "bad", "error")]));
    return section(heading, null, kids);
  }

  function profileSections(p, d, app) {
    const k = (s) => p + ":" + s, runs = d.runs || {}, out = [];
    const to = (state) => () => app.go("runs", { profile: p, state });
    out.push({ key: k("head"), sig: p, build: () => ({ cells: {}, node: h("header", { class: "prof-h" }, [h("span", { class: "tile" }, [icon("profile", { size: 18 })]), h("div", null, [h("p", { class: "ovl", text: "Profile" }), h("h3", { text: p })])]) }) });
    out.push({ key: k("queue"), sig: JSON.stringify([runs, d.oldest_ready_secs]), build: (isNew) => {
      const cells = {}, secs = d.oldest_ready_secs, st = (s) => runs[s] || 0;
      const hero = h("button", { type: "button", class: "hero" + (st("ready") > 0 ? " is-link" : ""), title: "View ready runs", onclick: to("ready"), disabled: !st("ready") }, [num(cells, "ready", st("ready"), isNew), h("span", { text: B.RUN_STATE.ready[0] })]);
      const minis = ["leased", "waiting", "retry", "succeeded", "dead", "cancelled"].map((s) => mini(cells, s, B.RUN_STATE[s][0], st(s), s === "dead" && st(s) > 0 ? "bad" : "", isNew, to(s)));
      minis.push(mini(cells, "oldest", "Oldest ready (s)", secs === null || secs === undefined ? "-" : secs, secs > 60 ? "warn" : "", isNew));
      return { node: section("Run status", null, [strip(hero, minis)]), cells };
    } });
    out.push({ key: k("todo"), sig: JSON.stringify([d.approvals_pending, d.holds]), build: () => ({ cells: {}, node: h("div", { class: "todos" }, [link("Pending approvals", d.approvals_pending || 0, "warn", () => app.go("decide", { profile: p })), link("Holds", d.holds || 0, "bad", () => app.go("decide", { profile: p }))]) }) });
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
        wrap = h("div", { class: "wrap" }, [h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "Status" }), sub])]), body]);
        box.appendChild(wrap);
      }
      const names = Object.keys(live.profiles || {}).sort(), shown = names.filter((p) => !app.profile() || p === app.profile());
      sub.textContent = "Uptime " + D.uptime(live.process.uptime_secs) + " · Profiles " + names.join(", ") + (live.build ? " · " + live.build.name + " " + live.build.version : "");
      let list = [];
      if (live.process && live.process.logs && live.process.logs.enabled) {
        list.push({ key: "host:logs", sig: JSON.stringify(live.process.logs), build: (isNew) => ({ cells: {}, node: obsSection(live.process.logs, isNew, "Log export (OTLP)", "Log export is disabled.") }) });
      }
      if (live.process && live.process.issues && live.process.issues.enabled) {
        list.push({ key: "host:issues", sig: JSON.stringify(live.process.issues), build: (isNew) => ({ cells: {}, node: obsSection(live.process.issues, isNew, "Issue tracker", "Issue reporting is disabled.") }) });
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
