// Viewport overlay drawing curved "needs" links between the selected node and shared dock resources.
(function (root) {
  "use strict";
  const L = typeof module !== "undefined" && module.exports ? require("./layout.js") : root.BotLayout;

  function pairs(idx, key) {
    const needs = (list) => (list || []).filter((e) => e.rel === "needs").map((e) => e.key);
    return idx.byKey[key] ? { out: needs(idx.out[key]), inc: needs(idx.inc[key]) } : { out: [], inc: [] };
  }

  function link(a, b) {
    const down = a.y + a.h / 2 <= b.y + b.h / 2;
    const p = [a.x + a.w / 2, down ? a.y + a.h : a.y], q = [b.x + b.w / 2, down ? b.y : b.y + b.h];
    return { d: L.curve(p[0], p[1], q[0], q[1]), p, q };
  }

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
