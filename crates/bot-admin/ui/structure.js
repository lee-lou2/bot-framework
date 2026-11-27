// Structure view connecting BotMap to the shared BotStore graph cache.
(function (root) {
  "use strict";
  const D = root.BotDom, S = root.BotStore, { $, h } = D;

  function create(app) {
    const map = root.BotMap.create({ els: { view: $("view-structure"), bar: $("bar"), query: $("query"), hits: $("hits"), kinds: $("kinds"), fit: $("fit"), map: $("map"), stage: $("stage"), dock: $("dock"), zin: $("zin"), zout: $("zout"), detail: $("detail"), hint: $("maphint") } });
    let fresh = true, drawn = null, loadingEl = null;

    function loading(on) {
      const box = $("map");
      if (on && !loadingEl) { loadingEl = h("div", { class: "map-loading" }, [D.skeleton(4)]); box.appendChild(loadingEl); }
      if (!on && loadingEl) { box.removeChild(loadingEl); loadingEl = null; }
    }
    // Access view shares BotStore, so compare against `drawn` revision to catch graph updates loaded elsewhere.
    function draw() {
      const m = S.map;
      if (!m.nodes) { map.clearAll(); loading(true); drawn = null; return; }
      if (fresh || m.rev !== drawn) { loading(false); map.setData(m.nodes, { reset: fresh }); fresh = false; drawn = m.rev; }
    }
    return {
      load(ctx) {
        return S.load(ctx).then((r) => {
          if (r.stale) return;
          app.noteProfiles(Object.keys(S.map.access || {}));
          app.stamp(S.map.at);
          draw();
        });
      },
      reset() { fresh = true; drawn = null; draw(); },
      show() { draw(); map.show(); },
      hide() { map.hide(); },
      busy() { return false; },
    };
  }

  root.BotStructure = { create };
})(window);
