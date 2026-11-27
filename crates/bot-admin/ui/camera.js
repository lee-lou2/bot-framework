// Pure map camera math: screen = (x, y) + k * stage.
(function (root) {
  "use strict";

  const Z = {
    MIN: 0.25, MAX: 2.2,
    FIT_MIN: 0.2, FIT_MAX: 1.1,
    // Minimum scale keeping 13px chip labels >= 12px on initial fit.
    READABLE: 0.86,
    STEP: 80, KEY_ZOOM: 1.2, BTN_ZOOM: 1.25, WHEEL: 0.0022, DRAG_PX: 5, KEEP: 96,
  };

  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

  function viewArea(vp, ins) {
    const i = Object.assign({ top: 0, right: 0, bottom: 0, left: 0 }, ins);
    return { x: i.left, y: i.top, w: Math.max(40, vp.w - i.left - i.right), h: Math.max(40, vp.h - i.top - i.bottom) };
  }

  function fitCamera(rect, vp, ins, o) {
    o = o || {};
    const a = viewArea(vp, ins);
    const k = clamp(Math.min(a.w / Math.max(1, rect.w), a.h / Math.max(1, rect.h)), o.kMin || Z.FIT_MIN, o.kMax || Z.FIT_MAX);
    const x = a.x + (a.w - rect.w * k) / 2 - rect.x * k;
    return { x, y: o.anchor === "top" ? a.y - rect.y * k : a.y + (a.h - rect.h * k) / 2 - rect.y * k, k };
  }

  // Fit whole graph when readable; otherwise fit width and anchor to the top so users pan down.
  function defaultCamera(layout, vp, ins) {
    if (!layout.cards.length) return { x: 0, y: 0, k: 1 };
    const r = layout.bounds, a = viewArea(vp, ins);
    if (Math.min(a.w / r.w, a.h / r.h) >= Z.READABLE) return fitCamera(r, vp, ins, { kMax: Z.FIT_MAX });
    const k = clamp(a.w / r.w, Z.FIT_MIN, 1);
    return { k, x: a.x + (a.w - r.w * k) / 2 - r.x * k, y: a.y - r.y * k };
  }

  function focusCamera(rect, vp, ins, o) {
    o = o || {};
    const m = o.margin === undefined ? 56 : o.margin;
    return fitCamera({ x: rect.x - m, y: rect.y - m, w: rect.w + 2 * m, h: rect.h + 2 * m }, vp, ins, { kMax: o.kMax || 1.3, kMin: o.kMin || 0.6 });
  }

  function zoomAt(cam, factor, cx, cy, kMin, kMax) {
    const k = clamp(cam.k * factor, kMin || Z.MIN, kMax || Z.MAX), f = k / cam.k;
    return { k, x: cx - (cx - cam.x) * f, y: cy - (cy - cam.y) * f };
  }

  function wheelCamera(cam, e, pt) {
    const unit = e.mode === 1 ? 16 : e.mode === 2 ? 120 : 1;
    let dx = (e.dx || 0) * unit, dy = (e.dy || 0) * unit;
    if (e.ctrl || e.meta) return zoomAt(cam, clamp(Math.exp(-dy * Z.WHEEL), 0.5, 2), pt.x, pt.y);
    if (e.shift && !dx) { dx = dy; dy = 0; }
    return { k: cam.k, x: cam.x - dx, y: cam.y - dy };
  }

  function pinchCamera(start, now) {
    const d = Math.hypot(now.a.x - now.b.x, now.a.y - now.b.y) || 1, mid = { x: (now.a.x + now.b.x) / 2, y: (now.a.y + now.b.y) / 2 };
    const z = zoomAt(start.cam, d / (start.dist || 1), start.mid.x, start.mid.y);
    return { k: z.k, x: z.x + (mid.x - start.mid.x), y: z.y + (mid.y - start.mid.y) };
  }

  function keyCamera(cam, key, vp) {
    const s = Z.STEP, pan = { ArrowLeft: [s, 0], ArrowRight: [-s, 0], ArrowUp: [0, s], ArrowDown: [0, -s] }[key];
    if (pan) return { k: cam.k, x: cam.x + pan[0], y: cam.y + pan[1] };
    if (key === "+" || key === "=") return zoomAt(cam, Z.KEY_ZOOM, vp.w / 2, vp.h / 2);
    if (key === "-" || key === "_") return zoomAt(cam, 1 / Z.KEY_ZOOM, vp.w / 2, vp.h / 2);
    return key === "0" ? "fit" : null;
  }

  function revealCamera(cam, rect, vp, ins, pad) {
    const a = viewArea(vp, ins), p = pad === undefined ? 16 : pad;
    const move = (lo, size, from, len) => (from < lo ? lo - from + p : from + len > lo + size ? lo + size - (from + len) - p : 0);
    const dx = move(a.x, a.w, rect.x, rect.w), dy = move(a.y, a.h, rect.y, rect.h);
    return dx || dy ? { k: cam.k, x: cam.x + dx, y: cam.y + dy } : null;
  }

  // Keep at least `keep` px of graph bounds inside the viewport so users cannot lose the canvas.
  function limitCamera(cam, bounds, vp, keep) {
    const m = keep === undefined ? Z.KEEP : keep;
    const left = cam.x + bounds.x * cam.k, right = left + bounds.w * cam.k, top = cam.y + bounds.y * cam.k, bottom = top + bounds.h * cam.k;
    let x = cam.x, y = cam.y;
    if (right < m) x += m - right; else if (left > vp.w - m) x -= left - (vp.w - m);
    if (bottom < m) y += m - bottom; else if (top > vp.h - m) y -= top - (vp.h - m);
    return { k: cam.k, x, y };
  }

  const isDrag = (dx, dy) => Math.hypot(dx, dy) >= Z.DRAG_PX;

  function union(rects) {
    if (!rects.length) return null;
    const b = rects.reduce((m, r) => ({ x1: Math.min(m.x1, r.x), y1: Math.min(m.y1, r.y), x2: Math.max(m.x2, r.x + r.w), y2: Math.max(m.y2, r.y + r.h) }), { x1: 1e9, y1: 1e9, x2: -1e9, y2: -1e9 });
    return { x: b.x1, y: b.y1, w: b.x2 - b.x1, h: b.y2 - b.y1 };
  }

  const api = { Z, clamp, viewArea, fitCamera, defaultCamera, focusCamera, zoomAt, wheelCamera, pinchCamera, keyCamera, revealCamera, limitCamera, isDrag, union };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotCamera = api;
})(typeof window !== "undefined" ? window : globalThis);
