// 보류 판정의 «첫 확인에 고정»: 되돌릴 수 없는 판정은 두 번 눌러야 기록된다. 첫 확인에서 대상 단계·보류 식별자·판정 종류·입력값을 고정하고,
// 두 번째 누름은 그 고정값만 보낸다. 그 사이 목록이 다시 그려져 기본 선택이 바뀌거나(다른 운영자가 그 단계를 먼저 처리한 경우)
// 같은 단계가 다시 보류돼 새 식별자를 받으면, 같은 영수증·근거가 다른 단계나 새 보류에 기록되지 않도록 확인과 입력을 모두 해제한다.
// 화면 요소를 모르는 순수 함수라 노드로 시험한다(tests/js/holdpin.test.mjs).
(function (root) {
  "use strict";

  // 아직 판정을 기다리는 단계인가: 보류이고 미실행 판정을 받지 않았다. 미실행 판정을 받은 단계(resolution)는 재개를 기다릴 뿐 다시 고를 수 없다.
  const open = (s) => s.state === "held" && !s.resolution;
  // 보류 단계 목록에서 아직 판정을 기다리는 단계의 id.
  const held = (steps) => (steps || []).filter(open).map((s) => s.effect);

  // 보류 식별자(hold_id): 미결 단계는 각 단계의 값이고, 호출 전체(최종 결과·단일 도구·회신·민감 DM)는 보류 자체의 값이다.
  // 저장소가 새 보류에 들어갈 때마다 새로 발급하므로 같은 단계가 다시 보류되면 값이 달라진다. 식별자가 없는 대상(null)은 판정할 수 없다.
  function holdIdFor(hold, effect) {
    if (effect === null || effect === undefined) return hold.hold_id || null;
    const step = (hold.steps || []).find((s) => s.effect === effect);
    return (step && step.hold_id) || null;
  }

  // 호출 전체 판정에 이미 실행된 단계가 있으면 수신처의 전체 영수증만 받는다.
  const receiptOnly = (hold) => !!hold.hold_id && (hold.steps || []).some((s) => s.state === "confirmed");

  // 보류 카드·판정 폼의 키. 구판 데이터는 같은 프로필·run·호출 ID 아래 도구와 회신을 따로 보류할 수 있어 보류 종류까지 넣는다.
  // 종류를 빼면 목록은 둘인데 카드는 하나로 합쳐지고 입력·확인 상태도 서로 섞인다.
  const keyOf = (hold) => [hold.profile, hold.run, hold.call, hold.kind || ""].join("|");

  // 판정할 수 있는 대상인가. 서버는 보류 식별자가 없는 판정을 받지 않으므로(400) 식별자가 없으면 판정 단추를 끄고 목록을 다시 불러오게 한다.
  const decidable = (hold, effect) => holdIdFor(hold, effect) !== null;

  // 첫 확인: 지금 고른 단계(단일 호출이면 null)·그 보류 식별자·판정 종류·입력값을 고정한다. 보류 식별자가 없으면 고정할 수 없다(null).
  function arm(kind, effect, text, holdId) {
    if (holdId === undefined || holdId === null || holdId === "") return null;
    return { kind, effect: effect === undefined ? null : effect, text, hold: holdId };
  }

  // 지금 고른 단계를 목록에 맞춘다: 아직 보류면 그대로, 아니면 첫 보류 단계(없으면 null).
  // 고른 단계가 바뀌었거나 같은 단계가 새 식별자로 다시 보류됐으면(changed) 이전 보류용으로 적은 영수증·근거를 쓰면 안 되므로
  // 호출하는 쪽이 입력을 모두 지워야 한다.
  function retarget(current, currentHold, hold) {
    const cur = current === undefined ? null : current;
    const curHold = currentHold === undefined ? null : currentHold;
    const ids = held(hold.steps);
    const next = ids.indexOf(cur) >= 0 ? cur : ids.length ? ids[0] : null;
    const nextHold = holdIdFor(hold, next);
    const moved = cur !== null && next !== cur;
    const renewed = curHold !== null && nextHold !== curHold;
    return { effect: next, holdId: nextHold, changed: moved || renewed };
  }

  // 서버가 «이미 판정됨/값이 다름»(conflict)이나 «없음»(not_found)이라고 답한 요청: 그 요청의 입력은 남은 다른 보류에 쓰면 안 되므로 지운다.
  const STALE = { conflict: true, not_found: true };
  const staleWord = (word) => STALE[word] === true;

  // 목록이 새로 그려질 때: 고정한 보류가 아직 같은 보류인가. 호출 전체(effect=null)는 미결 단계가 없고 같은 발생 ID여야 한다.
  // 반환: { ok: true } 또는 { ok: false, reason } — ok가 아니면 호출하는 쪽이 확인과 모든 입력을 해제해야 한다.
  function reconcile(pin, hold) {
    if (!pin) return { ok: true };
    const steps = hold.steps || [];
    if (pin.effect === null) {
      if (steps.some(open) || (steps.length && !hold.hold_id) || (pin.kind === "not_executed" && receiptOnly(hold))) return { ok: false, reason: "stepped" };
      return (hold.hold_id || null) === pin.hold ? { ok: true } : { ok: false, reason: "renewed" };
    }
    const now = steps.find((s) => s.effect === pin.effect && open(s));
    if (!now) return { ok: false, reason: "gone" };
    return (now.hold_id || null) === pin.hold ? { ok: true } : { ok: false, reason: "renewed" };
  }

  // 두 번째 누름이 보낼 본문. 현재 선택·입력칸이 아니라 고정값만 쓴다(보류 식별자 포함). 고정이 없으면(확인 전) null.
  function payload(pin, fields) {
    if (!pin || pin.hold === null) return null;
    const body = { verdict: pin.kind };
    body[fields[pin.kind]] = pin.text.trim();
    if (pin.effect !== null) body.effect = pin.effect;
    body.hold_id = pin.hold;
    return body;
  }

  // 다시 확인해야 하는 이유를 사람이 읽는 말로.
  const WHY = {
    gone: "확인하던 단계가 이미 다른 곳에서 처리됐습니다. 입력을 지웠으니 남은 단계를 다시 확인하세요.",
    stepped: "이 호출의 단계 구성이 바뀌었습니다. 입력을 지웠으니 다시 확인하세요.",
    renewed: "같은 대상이 새로 보류됐습니다(새 보류). 입력을 지웠으니 새 보류를 다시 확인하세요.",
    noid: "이 보류에는 판정에 쓸 식별자가 없어 판정할 수 없습니다. 목록을 다시 불러온 뒤 판정하세요.",
  };

  const api = { open, held, receiptOnly, holdIdFor, keyOf, decidable, arm, retarget, staleWord, reconcile, payload, WHY };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotHoldPin = api;
})(typeof window !== "undefined" ? window : globalThis);
