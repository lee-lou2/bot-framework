// «필요» 선: 노드를 골랐을 때만, 고른 칩과 그것이 필요로 하는(또는 그것을 필요로 하는) 공통 자원(스코프·비밀)을 곡선으로 잇는다.
// 공통 자원은 아래 독에 있어서 화면 좌표(지도 칸 위 겹침 층)에서 그리고, 카메라가 움직이는 동안 map.js가 다시 그려 따라간다.
(function (root) {
  "use strict";
  const L = typeof module !== "undefined" && module.exports ? require("./layout.js") : root.BotLayout;

  // 순수: 고른 노드가 «필요»로 이어진 상대. 항목이면 필요한 공통 자원(out), 공통 자원이면 그것을 필요로 하는 항목(inc).
  function pairs(idx, key) {
    const needs = (list) => (list || []).filter((e) => e.rel === "needs").map((e) => e.key);
    return idx.byKey[key] ? { out: needs(idx.out[key]), inc: needs(idx.inc[key]) } : { out: [], inc: [] };
  }

  // 순수: 같은 좌표계의 두 사각형을 잇는 곡선. 위쪽 사각형의 아래 변 가운데에서 아래쪽 사각형의 위 변 가운데로.
  function link(a, b) {
    const down = a.y + a.h / 2 <= b.y + b.h / 2;
    const p = [a.x + a.w / 2, down ? a.y + a.h : a.y], q = [b.x + b.w / 2, down ? b.y : b.y + b.h];
    return { d: L.curve(p[0], p[1], q[0], q[1]), p, q };
  }

  // o = { host, idx(): 관계 색인, rect(key): { x, y, w, h, group } | null } — rect는 host 기준 화면 좌표, group이 같은 대상(같은 알약)은 선 하나로 모은다.
  function create(o) {
    const D = root.BotDom;
    const layer = D.svg("svg", { class: "needs", "aria-hidden": "true", focusable: "false" });
    o.host.appendChild(layer);
    const clear = () => { while (layer.firstChild) layer.removeChild(layer.firstChild); };
    function draw(key) {
      clear();
      const idx = o.idx();
      if (!key || !idx) return 0;
      const pr = pairs(idx, key), me = o.rect(key), seen = {};
      let n = 0;
      if (!me) return 0;
      pr.out.concat(pr.inc).forEach((k) => {
        const r = o.rect(k), g = r && (r.group || k);
        if (!r || seen[g]) return;
        seen[g] = true;
        const l = link(me, r);
        layer.appendChild(D.svg("path", { class: "need", d: l.d }));
        layer.appendChild(D.svg("circle", { class: "need-dot", cx: l.p[0], cy: l.p[1], r: 3 }));
        layer.appendChild(D.svg("circle", { class: "need-dot", cx: l.q[0], cy: l.q[1], r: 3 }));
        n++;
      });
      return n;
    }
    return { draw, clear, get layer() { return layer; } };
  }

  const api = { pairs, link, create };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotNeeds = api;
})(typeof window !== "undefined" ? window : globalThis);
