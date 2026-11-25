// 사전 승인 화면의 순수 규칙: 서버 행이 쓸 수 있는 모양인가, 정지·재개 요청이 무엇을 싣고 어디로 가는가, 서버 응답을 받아 상태를 바꿔도 되는가.
// 화면 요소를 모르는 함수라 노드로 시험한다(tests/js/preapproval.test.mjs). 서버 계약(관리 API):
//   GET  /v1/admin/preapprovals → {"items":[행]}                      행: profile·agent·trigger·grant_id(문자열) epoch·used_today·max_per_day(정수) paused(불리언)
//   POST /v1/admin/preapprovals/{profile}/{agent}/{trigger}/pause|resume  본문 {grant_id, epoch} → 200 + 래핑 없는 새 행 · 409 stale · 403 · 404 · 503
// 화면은 서버가 준 행으로만 상태를 바꾼다(요청을 보냈다고 미리 바꾸지 않는다). 필수 필드가 하나라도 틀린 행에는 버튼을 주지 않는다.
(function (root) {
  "use strict";

  const ACTIONS = ["pause", "resume"];
  // 한 줄 글: 제어 문자와 «|»(키 구분자) 없음. agent는 `main/developer`처럼 «/»를 가질 수 있다.
  const LINE = /^[^\u0000-\u001f\u007f-\u009f|]{1,256}$/;
  const isText = (v) => typeof v === "string" && LINE.test(v);
  const isCount = (v) => typeof v === "number" && Number.isSafeInteger(v) && v >= 0;

  // 서버가 준 행이 화면이 쓸 수 있는 모양인가. 모르는 필드는 무시한다.
  function valid(r) {
    return !!r && typeof r === "object" && !Array.isArray(r)
      && isText(r.profile) && isText(r.agent) && isText(r.trigger) && isText(r.grant_id)
      && isCount(r.epoch) && typeof r.paused === "boolean" && isCount(r.used_today) && isCount(r.max_per_day);
  }
  // 행의 키: (프로필, 에이전트, 트리거). 서버의 유일 키와 같다.
  const key = (r) => [r.profile, r.agent, r.trigger].join("|");
  const before = (a, b) => (key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0);
  const sort = (rows) => rows.slice().sort(before);

  // 목록 응답 → { rows, skipped }. 모양이 틀린 응답은 null이다(목록으로 읽지 않는다).
  // 읽을 수 없는 행과, 같은 키가 둘 이상인 행(어느 쪽이 현재인지 알 수 없다)은 버튼 없이 건너뛰고 수만 센다.
  function parse(body) {
    if (!body || typeof body !== "object" || !Array.isArray(body.items)) return null;
    const items = body.items.filter(valid), seen = {};
    items.forEach((r) => { seen[key(r)] = (seen[key(r)] || 0) + 1; });
    const rows = items.filter((r) => seen[key(r)] === 1);
    return { rows: sort(rows), skipped: body.items.length - rows.length };
  }

  // 지금 이 행에서 할 수 있는 동작: 정지돼 있으면 재개, 아니면 정지.
  const offered = (r) => (r.paused ? "resume" : "pause");
  // 오늘 사용량: 퍼센트는 0~100 정수, 상한에 닿았는가.
  function usage(r) {
    const used = r.used_today, max = r.max_per_day;
    return { used, max, pct: max > 0 ? Math.min(100, Math.floor((used * 100) / max)) : 100, left: Math.max(0, max - used), full: used >= max };
  }
  const counts = (rows) => ({ total: rows.length, paused: rows.filter((r) => r.paused).length });

  // 확인 한 번(정지는 곧바로, 재개는 첫 누름)이 고정하는 것: 대상 식별자와 화면이 본 승인 ID·정지 세대. 이 행이 지금 그 동작을 할 수 있을 때만 만든다.
  function arm(r, action) {
    if (!valid(r) || ACTIONS.indexOf(action) < 0 || offered(r) !== action) return null;
    return { key: key(r), action, profile: r.profile, agent: r.agent, trigger: r.trigger, grant_id: r.grant_id, epoch: r.epoch };
  }
  // 요청 주소: 식별자마다 한 번만 인코딩한다(`main/developer` → `main%2Fdeveloper`가 한 조각).
  function path(pin) {
    if (ACTIONS.indexOf(pin.action) < 0) throw new Error("알 수 없는 동작");
    return "/v1/admin/preapprovals/" + [pin.profile, pin.agent, pin.trigger].map(encodeURIComponent).join("/") + "/" + pin.action;
  }
  // 요청 본문: 키는 둘뿐이다(서버는 모르는 키를 받지 않는다). 현재 행이 아니라 고정한 값을 보낸다.
  const payload = (pin) => (pin ? { grant_id: pin.grant_id, epoch: pin.epoch } : null);

  // 목록이 새로 왔을 때 고정한 확인이 아직 같은 대상·같은 승인·같은 세대·같은 동작인가. ok가 아니면 호출하는 쪽이 확인을 거둔다.
  function reconcile(pin, rows) {
    if (!pin) return { ok: true };
    const now = rows.find((r) => key(r) === pin.key);
    if (!now) return { ok: false, reason: "gone" };
    if (now.grant_id !== pin.grant_id) return { ok: false, reason: "renewed" };
    if (now.epoch !== pin.epoch || offered(now) !== pin.action) return { ok: false, reason: "changed" };
    return { ok: true };
  }

  // 성공 응답(200)의 행을 받아들여도 되는가: 같은 대상·같은 승인이고 요청한 동작의 결과 상태여야 한다. 아니면 null(상태를 바꾸지 않고 목록을 다시 읽는다).
  function settle(pin, reply) {
    if (!valid(reply) || key(reply) !== pin.key || reply.grant_id !== pin.grant_id) return null;
    return reply.paused === (pin.action === "pause") ? reply : null;
  }
  // 목록에서 이 행만 서버가 준 새 행으로 바꾼다.
  const replace = (rows, row) => sort(rows.filter((r) => key(r) !== key(row)).concat([row]));

  // 서버 오류 낱말 → 문장(정지·재개 요청용). 응답을 받지 못한 경우(unavailable·network·timeout)는 적용됐는지 모른다고 알린다.
  const ERR = {
    stale: "다른 곳에서 먼저 바뀌었거나 새 사전 승인으로 교체됐습니다. 목록을 다시 불러왔으니 현재 상태를 보고 다시 하세요.",
    denied: "운영자만 정지·재개할 수 있습니다.",
    not_found: "사전 승인을 찾을 수 없습니다. 선언이 제거됐을 수 있습니다. 목록을 다시 불러옵니다.",
    invalid: "요청 내용이 올바르지 않습니다. 목록을 다시 불러옵니다.",
    unavailable: "저장소에 연결하지 못해 적용됐는지 확인하지 못했습니다. 목록을 다시 불러와 현재 상태를 확인하세요.",
    network: "서버에 연결하지 못해 적용됐는지 확인하지 못했습니다. 목록을 다시 불러와 현재 상태를 확인하세요.",
    timeout: "응답이 오지 않아 적용됐는지 확인하지 못했습니다. 목록을 다시 불러와 현재 상태를 확인하세요.",
    mismatch: "서버 응답이 요청과 맞지 않아 화면의 상태를 바꾸지 않았습니다. 목록을 다시 불러와 현재 상태를 확인하세요.",
  };
  // 재개 확인을 거둔 이유
  const WHY = {
    gone: "확인하던 사전 승인이 목록에서 사라졌습니다(선언이 제거됐을 수 있습니다). 확인을 거두었습니다.",
    renewed: "사전 승인이 새로 발급됐습니다(활성화가 바뀜). 확인을 거두었으니 새 범위를 보고 다시 확인하세요.",
    changed: "다른 곳에서 상태가 바뀌었습니다. 확인을 거두었으니 현재 상태를 보고 다시 확인하세요.",
  };

  const api = { ACTIONS, valid, key, parse, offered, usage, counts, arm, path, payload, reconcile, settle, replace, sort, ERR, WHY };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotPreapproval = api;
})(typeof window !== "undefined" ? window : globalThis);
