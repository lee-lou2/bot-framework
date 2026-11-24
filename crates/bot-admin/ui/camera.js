// 지도 카메라 계산(순수): 화면 좌표 = (x, y) + k * 스테이지 좌표. DOM·시간·네트워크를 쓰지 않아 node에서 그대로 불러 시험할 수 있다.
// 맞춤·확대 범위·휠(Ctrl/⌘ = 핀치)·핀치·키보드·초점 이동·이동 한계가 모두 여기에 있다.
(function (root) {
  "use strict";

  const Z = {
    MIN: 0.25, MAX: 2.2,            // 사용자가 만들 수 있는 확대 범위
    FIT_MIN: 0.2, FIT_MAX: 1.1,     // 맞춤(전체 보기·처음 화면)이 고르는 배율 범위
    READABLE: 0.86,                 // 13px 글자가 12px 아래로 내려가지 않는 최소 배율(기본 화면은 이 선을 지킨다)
    STEP: 80, KEY_ZOOM: 1.2, BTN_ZOOM: 1.25, WHEEL: 0.0022, DRAG_PX: 5, KEEP: 96,
  };

  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

  function viewArea(vp, ins) {
    const i = Object.assign({ top: 0, right: 0, bottom: 0, left: 0 }, ins);
    return { x: i.left, y: i.top, w: Math.max(40, vp.w - i.left - i.right), h: Math.max(40, vp.h - i.top - i.bottom) };
  }

  // rect를 화면 영역 가운데(anchor "top"이면 위쪽)에 놓고, 배율은 kMin~kMax 안에서 한 화면에 들어가게.
  function fitCamera(rect, vp, ins, o) {
    o = o || {};
    const a = viewArea(vp, ins);
    const k = clamp(Math.min(a.w / Math.max(1, rect.w), a.h / Math.max(1, rect.h)), o.kMin || Z.FIT_MIN, o.kMax || Z.FIT_MAX);
    const x = a.x + (a.w - rect.w * k) / 2 - rect.x * k;
    return { x, y: o.anchor === "top" ? a.y - rect.y * k : a.y + (a.h - rect.h * k) / 2 - rect.y * k, k };
  }

  // 처음 보이는 카메라: 한 화면에 다 들어가고 읽을 수 있으면(READABLE 이상) 맞춤, 아니면 폭 맞춤으로 위에 붙인다(아래는 끌어서 본다).
  function defaultCamera(layout, vp, ins) {
    if (!layout.cards.length) return { x: 0, y: 0, k: 1 };
    const r = layout.bounds, a = viewArea(vp, ins);
    if (Math.min(a.w / r.w, a.h / r.h) >= Z.READABLE) return fitCamera(r, vp, ins, { kMax: Z.FIT_MAX });
    const k = clamp(a.w / r.w, Z.FIT_MIN, 1);
    return { k, x: a.x + (a.w - r.w * k) / 2 - r.x * k, y: a.y - r.y * k };
  }

  // 한 노드로 다가가기: 둘레 여백을 두고 맞추되 너무 확대하지 않는다.
  function focusCamera(rect, vp, ins, o) {
    o = o || {};
    const m = o.margin === undefined ? 56 : o.margin;
    return fitCamera({ x: rect.x - m, y: rect.y - m, w: rect.w + 2 * m, h: rect.h + 2 * m }, vp, ins, { kMax: o.kMax || 1.3, kMin: o.kMin || 0.6 });
  }

  // (cx, cy)를 눌러 둔 점으로 고정한 채 factor배. 배율은 [kMin, kMax](기본 MIN~MAX)로 제한된다.
  function zoomAt(cam, factor, cx, cy, kMin, kMax) {
    const k = clamp(cam.k * factor, kMin || Z.MIN, kMax || Z.MAX), f = k / cam.k;
    return { k, x: cx - (cx - cam.x) * f, y: cy - (cy - cam.y) * f };
  }

  // 휠·트랙패드: Ctrl/⌘ + 휠(트랙패드 핀치도 이렇게 온다)은 포인터 기준 확대, 그냥 휠은 이동. e: { dx, dy, ctrl, meta, shift, mode }, pt: 포인터(지도 기준).
  function wheelCamera(cam, e, pt) {
    const unit = e.mode === 1 ? 16 : e.mode === 2 ? 120 : 1;
    let dx = (e.dx || 0) * unit, dy = (e.dy || 0) * unit;
    if (e.ctrl || e.meta) return zoomAt(cam, clamp(Math.exp(-dy * Z.WHEEL), 0.5, 2), pt.x, pt.y);
    if (e.shift && !dx) { dx = dy; dy = 0; }
    return { k: cam.k, x: cam.x - dx, y: cam.y - dy };
  }

  // 두 손가락 핀치: start = { dist, mid, cam }(시작 때 두 점 사이 거리·가운데·카메라), now = { a, b }(지금 두 점). 가운데 점을 따라 이동하며 확대한다.
  function pinchCamera(start, now) {
    const d = Math.hypot(now.a.x - now.b.x, now.a.y - now.b.y) || 1, mid = { x: (now.a.x + now.b.x) / 2, y: (now.a.y + now.b.y) / 2 };
    const z = zoomAt(start.cam, d / (start.dist || 1), start.mid.x, start.mid.y);
    return { k: z.k, x: z.x + (mid.x - start.mid.x), y: z.y + (mid.y - start.mid.y) };
  }

  // 키보드(지도에 포커스가 있을 때): 방향키 이동, + − 확대·축소, 0은 «맞춤» 요청("fit"). 모르는 키는 null.
  function keyCamera(cam, key, vp) {
    const s = Z.STEP, pan = { ArrowLeft: [s, 0], ArrowRight: [-s, 0], ArrowUp: [0, s], ArrowDown: [0, -s] }[key];
    if (pan) return { k: cam.k, x: cam.x + pan[0], y: cam.y + pan[1] };
    if (key === "+" || key === "=") return zoomAt(cam, Z.KEY_ZOOM, vp.w / 2, vp.h / 2);
    if (key === "-" || key === "_") return zoomAt(cam, 1 / Z.KEY_ZOOM, vp.w / 2, vp.h / 2);
    return key === "0" ? "fit" : null;
  }

  // Tab으로 포커스가 간 칩이 화면(여백 안) 밖이면 최소한만 옮겨 안으로 들인다. rect는 지도 기준 화면 좌표. 이미 안이면 null.
  function revealCamera(cam, rect, vp, ins, pad) {
    const a = viewArea(vp, ins), p = pad === undefined ? 16 : pad;
    const move = (lo, size, from, len) => (from < lo ? lo - from + p : from + len > lo + size ? lo + size - (from + len) - p : 0);
    const dx = move(a.x, a.w, rect.x, rect.w), dy = move(a.y, a.h, rect.y, rect.h);
    return dx || dy ? { k: cam.k, x: cam.x + dx, y: cam.y + dy } : null;
  }

  // 이동 한계: 그림(bounds, 스테이지 좌표)이 화면 밖으로 다 사라지지 않게 keep px는 늘 보이게 붙들어 둔다.
  function limitCamera(cam, bounds, vp, keep) {
    const m = keep === undefined ? Z.KEEP : keep;
    const left = cam.x + bounds.x * cam.k, right = left + bounds.w * cam.k, top = cam.y + bounds.y * cam.k, bottom = top + bounds.h * cam.k;
    let x = cam.x, y = cam.y;
    if (right < m) x += m - right; else if (left > vp.w - m) x -= left - (vp.w - m);
    if (bottom < m) y += m - bottom; else if (top > vp.h - m) y -= top - (vp.h - m);
    return { k: cam.k, x, y };
  }

  const isDrag = (dx, dy) => Math.hypot(dx, dy) >= Z.DRAG_PX;

  // 사각형들을 감싸는 상자
  function union(rects) {
    if (!rects.length) return null;
    const b = rects.reduce((m, r) => ({ x1: Math.min(m.x1, r.x), y1: Math.min(m.y1, r.y), x2: Math.max(m.x2, r.x + r.w), y2: Math.max(m.y2, r.y + r.h) }), { x1: 1e9, y1: 1e9, x2: -1e9, y2: -1e9 });
    return { x: b.x1, y: b.y1, w: b.x2 - b.x1, h: b.y2 - b.y1 };
  }

  const api = { Z, clamp, viewArea, fitCamera, defaultCamera, focusCamera, zoomAt, wheelCamera, pinchCamera, keyCamera, revealCamera, limitCamera, isDrag, union };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotCamera = api;
})(typeof window !== "undefined" ? window : globalThis);
