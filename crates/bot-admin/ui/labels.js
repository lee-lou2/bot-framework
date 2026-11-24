// 화면 말씨: 서버가 주는 고정 낱말(상태·위험 등급·사유 코드·사실 키)을 한국어 글자와 알약으로 바꾼다.
// 모르는 값은 «기타»로 보인다(서버 글을 그대로 믿고 풀어 쓰지 않는다). ID·해시는 code로 보인다.
(function (root) {
  "use strict";
  const D = root.BotDom, { h, pill, code, icon } = D;

  // [글자, 색조, 상태 점]. 색만으로 뜻을 전하지 않도록 글자와 점 모양을 함께 둔다.
  const RUN_STATE = { ready: ["대기", "info", "off"], leased: ["실행 중", "info", "on"], waiting: ["응답 대기", "warn", "held"], retry: ["재시도 대기", "warn", "held"], succeeded: ["완료", "ok", "on"], dead: ["실패(dead)", "bad", "error"], cancelled: ["취소", "off", "off"] };
  const RUN_ORDER = ["ready", "leased", "waiting", "retry", "succeeded", "dead", "cancelled"];
  const CALL_STATE = { running: ["실행 중", "info", "on"], awaiting: ["승인 대기", "warn", "held"], done: ["완료", "ok", "on"], held: ["보류", "warn", "held"], denied: ["거절됨", "off", "off"], failed: ["실패", "bad", "error"] };
  const EFFECT_STATE = { started: ["시작함", "info", "off"], confirmed: ["확정", "ok", "on"], rejected: ["거부됨", "bad", "error"], held: ["보류", "warn", "held"] };
  const APPROVAL_STATE = { pending: ["대기", "warn", "held"], approved: ["승인됨", "ok", "on"], consumed: ["사용됨", "ok", "on"], denied: ["거절됨", "off", "off"], expired: ["만료", "off", "off"], superseded: ["무효화", "warn", "error"], revoked: ["철회됨", "off", "off"] };
  const RISK = { read: ["읽기", "off", null], write: ["쓰기", "warn", null], irreversible: ["되돌릴 수 없음", "bad", "error"] };
  const CIRCUIT = { closed: ["정상", "ok", "on"], open: ["차단", "bad", "error"], half_open: ["회복 중", "warn", "held"] };
  const APPROVAL_REQ = { always: ["항상 승인", "bad"], policy: ["정책에 따라 승인", "warn"], none: ["승인 불필요", "off"] };
  // 보류 사유 코드: 정해진 말로만 보이고 모르는 값은 «기타»다.
  // 도구 호출이 끝나지 못한 이유(서버가 확인한 분류만 온다). 모르는 값은 «기타».
  const CALL_ERROR = { denied: "거절됨", timeout: "시간 초과", invalid_result: "결과 형식 오류", outcome_unknown: "결과를 알 수 없음", execution_failed: "실행 실패" };
  const HOLD_REASON = { outcome_unknown: "결과를 알 수 없음", unknown_outcome: "결과를 알 수 없음", owner_lost: "이전 실행 결과 미확인", no_evidence: "실행 여부를 가릴 증거 없음" };
  const CONVERSATION = { direct: "직접 대화", channel: "채널", cli: "CLI", http: "HTTP", scheduled: "예약" };
  const AUDIT_ACTION = { "approval.decide": "승인 판정", "hold.resolve": "보류 판정", "profile.activate": "프로필 활성화", "tool.call": "도구 호출" };
  const AUDIT_VERDICT = { allow: ["허용", "ok", "on"], deny: ["거절", "off", "off"], ok: ["정상", "ok", "on"], confirmed: ["처리됨 확인", "ok", "on"], not_executed: ["미실행 확인", "warn", "held"] };

  const fromTable = (t) => (key) => { const m = t[key]; return m ? pill(m[0], m[1], m[2]) : pill(key === null || key === undefined || key === "" ? "-" : "기타", "off"); };
  const runPill = fromTable(RUN_STATE), callPill = fromTable(CALL_STATE), effectPill = fromTable(EFFECT_STATE);
  const approvalPill = fromTable(APPROVAL_STATE), riskPill = fromTable(RISK), circuitPill = fromTable(CIRCUIT);
  const verdictPill = (v) => { const m = AUDIT_VERDICT[v]; return m ? pill(m[0], m[1], m[2]) : pill(v || "-", "off"); };
  const holdReason = (r) => HOLD_REASON[r] || "기타";
  const callError = (r) => (r ? CALL_ERROR[r] || "기타" : "-");
  const conversation = (k) => CONVERSATION[k] || "기타";

  function approvalReq(v) {
    const m = APPROVAL_REQ[v];
    if (!m) return pill("기타", "off");
    return h("span", { class: "pill pill-" + m[1] }, [v === "none" ? null : icon("lock", { size: 12 }), h("span", { text: m[0] })]);
  }
  const hash8 = (hex) => h("code", { text: String(hex || "").slice(0, 8) || "-", title: hex || null });

  // 대기 사유 한 줄(상세)과 짧은 말(목록)
  function waiting(w) {
    if (!w) return null;
    if (w.on === "approval") return h("span", null, ["승인을 기다리는 중입니다."]);
    if (w.on === "child") return h("span", null, ["자식 실행 ", code(w.run), "의 결과를 기다리는 중입니다."]);
    if (w.on === "hold") return h("span", null, ["보류된 호출 ", code(w.call), "의 판정을 기다리는 중입니다."]);
    return h("span", null, ["기다리는 중입니다."]);
  }
  function waitingShort(w) {
    if (!w) return "";
    return w.on === "approval" ? "승인 대기" : w.on === "child" ? "자식 " + w.run + " 대기" : w.on === "hold" ? "보류 " + w.call + " 대기" : "대기";
  }

  // 이름표 모음(길면 «+N»으로 접는다)
  const MORE = 6;
  function tags(names, mono) {
    if (!names.length) return "-";
    const row = h("div", { class: "ctags" });
    const add = (n) => row.appendChild(h("span", { class: "ctag" + (mono ? " mono" : ""), text: n }));
    names.slice(0, MORE).forEach(add);
    if (names.length > MORE) {
      const more = h("button", { type: "button", class: "ctag more", "aria-expanded": "false", text: "+" + (names.length - MORE), onclick: () => { names.slice(MORE).forEach(add); row.removeChild(more); } });
      row.appendChild(more);
    }
    return row;
  }
  const list = (v) => (!v || v === "-" ? [] : String(v).split(", "));

  // ---------- 지도 카드의 사실(facts) ----------
  const FACT = {
    profile: { revision: "판본", policy_revision: "정책 판본", timezone: "시간대", model: "모델", agents: "에이전트 수", root: "루트 에이전트" },
    agent: { model: "모델", scopes: "스코프", builtin: "내장 도구", skills: "스킬 수", triggers: "트리거 수", root: "루트 여부" },
    skill: { server: "도구 서버", tools: "도구 수" },
    tool: { class: "위험 등급", scopes: "스코프", approval: "승인 요건", server: "도구 서버", revision: "판본", external: "제3자 전송", effects: "단계 기록", egress: "허용 외부 주소", destination: "전송 대상" },
    trigger: { on: "종류", when: "조건", order: "검사 순서", private: "비공개 입력", principal: "실행 주체", beat: "신호 합치기", approver: "승인자", response: "응답 방식", target: "실행 대상" },
    binding: { adapter: "어댑터", detail: "상세", agent: "처리 에이전트" },
    scope: { granted: "정책 허용", approval: "승인 필요", roles: "역할" },
    secret: { declared: "선언됨" },
  };
  const TRIGGER_ON = { cron: "예약(cron)", event: "이벤트", http: "HTTP 요청" };
  const ADAPTER = { cli: "CLI", slack: "Slack", http: "HTTP" };
  const RESPONSE = { accepted: "접수 후 바로 응답(202)", sync: "도구 결과를 응답" };
  const factLabel = (kind, key) => (FACT[kind] && FACT[kind][key]) || key;

  // 전송 대상: `reply`는 원래 대화로의 답장, `받는 곳 <- 내용`은 인자 경로 둘이다.
  function destination(v) {
    if (v === "reply") return "원래 대화로 답장";
    const m = /^(.+) <- (.+)$/.exec(v);
    return m ? "받는 곳 " + m[1] + " · 내용 " + m[2] : v;
  }
  // 사실 한 줄의 값 노드(문자열 또는 요소)
  function factNode(card, f) {
    const k = f.key, v = f.val, kind = card.kind;
    if (v === "" || v === null || v === undefined) return "-";
    if (v === "true") return "예";
    if (v === "false") return "아니오";
    if (k === "scopes" || k === "roles" || k === "egress") return tags(list(v), true);
    if (kind === "tool" && k === "class") return riskPill(v);
    if (kind === "tool" && k === "approval") return approvalReq(v);
    if (k === "revision" || k === "policy_revision" || k === "server" || k === "model" || k === "principal" || k === "approver" || k === "timezone") return code(v);
    if (kind === "tool" && k === "destination") return destination(v);
    if (kind === "trigger" && k === "on") return TRIGGER_ON[v] || v;
    if (kind === "trigger" && k === "response") return RESPONSE[v] || v;
    if (kind === "trigger" && k === "target") return v === "agent" ? "에이전트에게 지시" : /^tool /.test(v) ? "도구 " + v.slice(5) + " 실행" : v;
    if (kind === "binding" && k === "adapter") return ADAPTER[v] || v;
    return v;
  }

  root.BotLabels = {
    RUN_STATE, RUN_ORDER, CALL_STATE, CALL_ERROR, EFFECT_STATE, APPROVAL_STATE, RISK, HOLD_REASON, CONVERSATION, AUDIT_ACTION, AUDIT_VERDICT,
    runPill, callPill, effectPill, approvalPill, riskPill, circuitPill, verdictPill, approvalReq, holdReason, callError, conversation,
    hash8, waiting, waitingShort, tags, list, factLabel, factNode, destination,
  };
})(window);
