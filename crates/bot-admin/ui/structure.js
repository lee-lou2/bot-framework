// Structure view: dual-mode bot setup inspector (scannable Directory cards + interactive BotMap topology graph).
(function (root) {
  "use strict";
  const D = root.BotDom, L = root.BotLayout, P = root.BotPanel, B = root.BotLabels, S = root.BotStore, { $, h, icon, code } = D;

  function create(app) {
    const viewEl = $("view-structure"), headEl = $("struct-head"), dirEl = $("struct-dir"), graphEl = $("struct-graph"), detailEl = $("detail");
    const map = root.BotMap.create({
      els: {
        view: graphEl || viewEl, bar: $("bar"), query: $("query"), hits: $("hits"), kinds: $("kinds"),
        fit: $("fit"), map: $("map"), stage: $("stage"), dock: $("dock"), zin: $("zin"), zout: $("zout"), detail: detailEl, hint: $("maphint"),
      },
    });
    const dirPanel = P.createPanel({
      el: detailEl,
      canGo: (k) => !!(st.idx && st.idx.byKey[k]),
      onNavigate: (k) => openDirNode(k),
      onClose: () => closeDirNode(),
    });
    const st = { mode: (app.structMode && app.structMode()) || "directory", query: "", kind: "", selected: null, idx: null, fresh: true, drawn: null, loadingEl: null };

    const isGraph = () => st.mode === "graph";
    function loading(on) {
      const box = $("map");
      if (!box) return;
      if (on && !st.loadingEl) { st.loadingEl = h("div", { class: "map-loading" }, [D.skeleton(4)]); box.appendChild(st.loadingEl); }
      if (!on && st.loadingEl) { box.removeChild(st.loadingEl); st.loadingEl = null; }
    }

    function openDirNode(key) {
      if (!st.idx || !st.idx.byKey[key]) return;
      st.selected = key;
      dirPanel.show(st.idx, key);
      if (viewEl) viewEl.classList.add("has-panel");
      markSelected();
    }
    function closeDirNode() {
      st.selected = null;
      dirPanel.hide();
      if (viewEl) viewEl.classList.remove("has-panel");
      markSelected();
    }
    function markSelected() {
      if (!dirEl) return;
      Array.prototype.forEach.call(dirEl.querySelectorAll("[data-dkey]"), (el) => {
        el.classList.toggle("is-selected", el.getAttribute("data-dkey") === st.selected);
      });
    }

    function setMode(next) {
      const mode = next === "graph" ? "graph" : "directory";
      if (st.mode === mode) return;
      st.mode = mode;
      closeDirNode();
      map.deselect();
      if (app.setStructMode) app.setStructMode(mode);
      syncMode();
      draw();
    }
    function syncMode() {
      if (dirEl) dirEl.hidden = isGraph();
      if (graphEl) graphEl.hidden = !isGraph();
      if (viewEl) viewEl.classList.toggle("is-graph", isGraph());
      renderHead();
      if (isGraph()) map.show(); else map.hide();
    }

    function renderHead() {
      if (!headEl) return;
      D.clear(headEl);
      const sw = h("div", { class: "seg", role: "group", "aria-label": "Structure view mode" }, [
        h("button", { type: "button", "aria-pressed": String(!isGraph()), onclick: () => setMode("directory") }, [icon("grid", { size: 13 }), h("span", { text: "Directory" })]),
        h("button", { type: "button", "aria-pressed": String(isGraph()), onclick: () => setMode("graph") }, [icon("network", { size: 13 }), h("span", { text: "Graph" })]),
      ]);
      headEl.appendChild(h("div", { class: "struct-head-in" }, [
        h("div", null, [
          h("h2", { text: "Structure" }),
          h("p", { class: "sub", text: "Active profiles, agents, channels, triggers, skills, and tools (names and classifications only)" }),
        ]),
        h("div", { class: "ph-act" }, [sw]),
      ]));
    }

    function nodeChip(c, extra) {
      const key = L.key(c), off = L.isOff(c), lock = c.kind === "tool" && ["always", "policy"].indexOf(L.fact(c, "approval")) >= 0;
      const cls = "dchip" + (off ? " is-off" : "") + (extra && extra.ref ? " is-ref" : "") + (st.selected === key ? " is-selected" : "");
      return h("button", { type: "button", class: cls, "data-kind": c.kind, "data-dkey": key, title: c.summary || c.name, onclick: () => openDirNode(key) }, [
        icon(c.kind, { size: 14, off }),
        h("span", { class: "dchip-nm", text: c.name }),
        extra && extra.sub ? h("span", { class: "dchip-sub", text: extra.sub }) : null,
        extra && extra.ref ? h("span", { class: "chip-tag", text: "Inherited" }) : null,
        lock ? icon("lock", { size: 12, cls: "chip-lock" }) : null,
      ]);
    }

    function matchesQuery(c, q) {
      if (!q) return true;
      if (c.name.toLowerCase().indexOf(q) >= 0 || String(c.summary || "").toLowerCase().indexOf(q) >= 0) return true;
      return (c.facts || []).some((f) => String(f.val || "").toLowerCase().indexOf(q) >= 0);
    }

    function renderDirectory() {
      if (!dirEl) return;
      const nodes = S.map && S.map.nodes, access = (S.map && S.map.access) || {};
      D.clear(dirEl);
      if (!nodes) { dirEl.appendChild(h("div", { class: "wrap" }, [D.skeleton(4)])); return; }
      st.idx = L.index(nodes);
      const q = st.query.trim().toLowerCase(), kindFilter = st.kind;
      const searchIn = h("input", { type: "search", placeholder: "Filter agents, channels, triggers, skills, tools…", "aria-label": "Filter structure directory", value: st.query, oninput: (e) => { st.query = e.target.value; renderDirBody(body, nodes, access); } });
      const kindOpts = [["", "All"], ["binding", "Channels"], ["trigger", "Triggers"], ["skill", "Skills"], ["tool", "Tools"], ["scope", "Scopes"], ["secret", "Secrets"]];
      const chips = h("div", { class: "chips", role: "group", "aria-label": "Filter by kind" }, kindOpts.map(([v, label]) =>
        h("button", { type: "button", class: "fchip", "aria-pressed": String(kindFilter === v), text: label, onclick: () => { st.kind = v; renderDirectory(); } }),
      ));
      const toolbar = h("div", { class: "toolbar" }, [h("label", { class: "searchbox" }, [icon("search", { size: 15 }), searchIn]), chips]);
      const body = h("div", { class: "dir-body" });
      renderDirBody(body, nodes, access);
      dirEl.appendChild(h("div", { class: "wrap" }, [toolbar, body]));
      if (q) { searchIn.focus(); searchIn.setSelectionRange(searchIn.value.length, searchIn.value.length); }
    }

    function renderDirBody(host, nodes, access) {
      D.clear(host);
      const q = st.query.trim().toLowerCase(), kf = st.kind;
      const byProfile = {}, order = [];
      nodes.forEach((c) => { if (!byProfile[c.profile]) { byProfile[c.profile] = []; order.push(c.profile); } byProfile[c.profile].push(c); });
      order.sort();
      let shownProfiles = 0;
      order.forEach((pname) => {
        const list = byProfile[pname], byId = {};
        list.forEach((c) => { byId[c.id] = c; });
        const pcard = list.find((c) => c.kind === "profile"), acc = access[pname] || {};
        const agents = list.filter((c) => c.kind === "agent").sort((a, b) => (a.name === "main" ? -1 : b.name === "main" ? 1 : a.name < b.name ? -1 : 1));
        const scopes = list.filter((c) => c.kind === "scope" && (!kf || kf === "scope") && matchesQuery(c, q)).sort((a, b) => a.name.localeCompare(b.name));
        const secrets = list.filter((c) => c.kind === "secret" && (!kf || kf === "secret") && matchesQuery(c, q)).sort((a, b) => a.name.localeCompare(b.name));

        const agentCards = [];
        agents.forEach((a) => {
          const own = list.filter((c) => L.ownerOf(c) === a.name && L.ITEM_KINDS.indexOf(c.kind) >= 0);
          const inherited = a.links.filter((l) => l.rel === "uses").map((l) => byId[l.to]).filter((c) => c && c.kind === "skill" && L.ownerOf(c) !== a.name);
          const byK = (k) => own.filter((c) => c.kind === k && (!kf || kf === k) && matchesQuery(c, q)).sort((x, y) => x.name.localeCompare(y.name));
          const bindings = byK("binding"), triggers = byK("trigger"), skills = byK("skill"), tools = byK("tool");
          const inhSkills = (!kf || kf === "skill") ? inherited.filter((c) => matchesQuery(c, q)) : [];
          const hasChildren = bindings.length + triggers.length + skills.length + inhSkills.length + tools.length > 0;
          const agentHit = !kf && matchesQuery(a, q);
          if (!hasChildren && !agentHit) return;
          const rootFlag = L.fact(a, "root") === "true", model = L.fact(a, "model") || "-", builtins = B.list(L.fact(a, "builtin"));
          const group = (label, kids) => kids.length ? h("div", { class: "dir-grp" }, [h("span", { class: "dir-grp-t", text: label }), h("div", { class: "dir-chips" }, kids)]) : null;
          agentCards.push(h("article", { class: "dir-agent" + (rootFlag ? " is-root" : "") + (st.selected === L.key(a) ? " is-selected" : ""), "data-dkey": L.key(a) }, [
            h("header", { class: "dir-agent-h" }, [
              h("button", { type: "button", class: "dir-agent-btn", onclick: () => openDirNode(L.key(a)) }, [
                h("span", { class: "dir-tile", "data-kind": "agent" }, [icon("agent", { size: 16 })]),
                h("div", { class: "dir-agent-tx" }, [
                  h("div", { class: "dir-agent-top" }, [h("strong", { text: a.name }), rootFlag ? D.pill("Root", "info", null) : null]),
                  h("span", { class: "dir-agent-sum", text: a.summary || ("Model " + model) }),
                ]),
              ]),
              code(model),
            ]),
            group("Channels & Bindings", bindings.map((c) => nodeChip(c, { sub: L.fact(c, "adapter") }))),
            group("Triggers & Schedules", triggers.map((c) => nodeChip(c, { sub: L.fact(c, "when") }))),
            group("Skills", skills.map((c) => nodeChip(c)).concat(inhSkills.map((c) => nodeChip(c, { ref: true })))),
            group("Tools", tools.map((c) => nodeChip(c, { sub: L.fact(c, "class") }))),
            builtins.length && !kf ? h("div", { class: "dir-meta" }, [h("span", { text: "Built-in read-only:" }), B.tags(builtins, true)]) : null,
          ]));
        });

        const showShared = scopes.length > 0 || secrets.length > 0;
        if (!agentCards.length && !showShared) return;
        shownProfiles++;
        const headBtn = pcard
          ? h("button", { type: "button", class: "btn sm quiet", text: "Profile details", onclick: () => openDirNode(L.key(pcard)) })
          : null;
        host.appendChild(h("section", { class: "dir-prof" }, [
          h("header", { class: "prof-h" }, [
            h("span", { class: "tile" }, [icon("profile", { size: 18 })]),
            h("div", null, [h("p", { class: "ovl", text: "Profile" }), h("h3", { text: pname })]),
            h("p", { class: "prof-facts" }, [
              "Model ", code(acc.model || "-"), " · Timezone ", D.text(acc.timezone || "UTC"),
              " · Root ", code(acc.root || "main"), " · Rev ", code(acc.revision || "-"),
            ]),
            headBtn,
          ]),
          agentCards.length ? h("div", { class: "dir-agents" }, agentCards) : null,
          showShared ? h("div", { class: "dir-shared" }, [
            scopes.length ? h("div", { class: "dir-shared-box" }, [h("h4", { text: "Scopes (" + scopes.length + ")" }), h("div", { class: "dir-chips" }, scopes.map((c) => nodeChip(c)))]) : null,
            secrets.length ? h("div", { class: "dir-shared-box" }, [h("h4", { text: "Declared secrets (" + secrets.length + ")" }), h("div", { class: "dir-chips" }, secrets.map((c) => nodeChip(c)))]) : null,
          ]) : null,
        ]));
      });
      if (!shownProfiles) host.appendChild(D.empty("No matching structure items", "Try clearing your search query or kind filter.", "search"));
    }

    // Access view shares BotStore, so compare against `drawn` revision to catch graph updates loaded elsewhere.
    function draw() {
      const m = S.map;
      if (!m.nodes) {
        map.clearAll(); loading(true); st.drawn = null;
        renderDirectory();
        return;
      }
      if (st.fresh || m.rev !== st.drawn) {
        loading(false);
        map.setData(m.nodes, { reset: st.fresh });
        st.fresh = false; st.drawn = m.rev;
      }
      if (!isGraph()) renderDirectory();
    }

    syncMode();
    return {
      load(ctx) {
        return S.load(ctx).then((r) => {
          if (r.stale) return;
          app.noteProfiles(Object.keys(S.map.access || {}));
          if (app.setBuild && S.map.build) app.setBuild(S.map.build);
          app.stamp(S.map.at);
          draw();
        });
      },
      reset() { st.fresh = true; st.drawn = null; closeDirNode(); draw(); },
      show() { syncMode(); draw(); },
      hide() { map.hide(); closeDirNode(); },
      setMode,
      isGraph,
      busy() { return false; },
    };
  }

  root.BotStructure = { create };
})(window);
