// 구조 탭: 지도 모듈(map.js)을 구조 자료(store.js)에 이어 주는 얇은 보기. 그리기는 모두 map.js가 맡는다.
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
    // 다른 탭(권한)이 먼저 새 구조를 받았을 수도 있으니, 마지막으로 그린 판본(drawn)과 견줘 다시 그린다.
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
