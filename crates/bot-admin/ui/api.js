// 관리 API 호출: 같은 출처 fetch(자격은 브라우저가 붙인다), 신원·CSRF 토큰, 오류 낱말 → 한국어 문장.
// 서버 오류 본문은 고정 낱말 하나뿐이다(`{"error":"stale"}`). 화면은 그 낱말만 읽고 문장은 여기서 고른다.
(function (root) {
  "use strict";
  const D = root.BotDom;
  const state = { me: null, req: null };

  class ApiError extends Error {
    constructor(status, word) { super(word); this.name = "ApiError"; this.status = status; this.word = word; }
  }

  const SENTENCE = {
    unauthorized: "인증이 필요합니다. 로그인 창에 비밀번호를 입력하세요.",
    rate_limited: "실패가 많아 잠시 막혔습니다. 잠시 뒤에 다시 시도하세요.",
    host: "허용되지 않은 주소로 열었습니다. 안내된 주소로 다시 여세요.",
    csrf: "보안 확인에 실패했습니다. 화면을 새로고침한 뒤 다시 시도하세요.",
    denied: "이 작업을 할 권한이 없습니다.",
    not_found: "찾을 수 없습니다. 이미 처리됐거나 볼 수 없는 항목입니다.",
    invalid: "요청 내용이 올바르지 않습니다.",
    conflict: "이미 처리된 항목입니다.",
    stale: "이미 처리됐거나 조건이 바뀌어 새 승인이 필요합니다.",
    changed: "내용이 바뀌었습니다. 다시 확인하세요.",
    unavailable: "저장소에 연결하지 못했습니다. 잠시 뒤에 다시 시도하세요.",
    leak: "응답에 비밀 값이 섞여 있어 보여 줄 수 없습니다.",
    too_large: "응답이 너무 커서 보여 줄 수 없습니다.",
    network: "서버에 연결하지 못했습니다.",
  };
  // 승인 판정·보류 판정이 쓰는 문장(같은 낱말도 하는 일에 따라 뜻이 다르다)
  const DECIDE = { denied: "지정된 승인자만 누를 수 있습니다.", no_body: "카드에서 승인 지문과 일치하는 내용 전체를 확인할 수 없어 승인할 수 없습니다. 거절은 할 수 있습니다.", not_found: "승인 요청을 찾을 수 없습니다. 이미 처리됐을 수 있습니다." };
  const RESOLVE = { conflict: "이미 판정된 보류입니다. 목록을 새로 불러옵니다.", denied: "운영자만 판정할 수 있습니다.", not_found: "보류를 찾을 수 없습니다. 이미 처리됐을 수 있습니다.", invalid: "입력을 확인하세요. 한 줄(1~512자)이어야 합니다." };

  // 주소에 사용자:비밀번호@가 들어 있으면 브라우저가 fetch 자체를 거부한다(서버 문제가 아님).
  // 주소창·location에서는 지워지지만 baseURI에는 남아 있다.
  function networkMessage() {
    try {
      const here = new URL(document.baseURI);
      if (here.username || here.password) return "주소에 사용자 이름·비밀번호가 들어 있으면 조회가 막힙니다. 주소창에서 지우고 다시 여세요.";
    } catch (e) { /* 주소를 읽지 못하면 일반 문장 */ }
    return SENTENCE.network;
  }
  const byStatus = (s) => (s === 401 ? "unauthorized" : s === 403 ? "denied" : s === 404 ? "not_found" : s === 429 ? "rate_limited" : s >= 500 ? "unavailable" : "invalid");

  function explain(e, over) {
    if (!(e instanceof ApiError)) return "알 수 없는 오류가 났습니다.";
    if (over && over[e.word]) return over[e.word];
    if (e.word === "network") return networkMessage();
    return SENTENCE[e.word] || "요청에 실패했습니다 (" + e.status + ").";
  }

  function read(res) {
    // 서버 시각과의 차이: 응답의 Date(초 단위)로 맞춘다. 만료·경과 시간이 내 시계가 틀려도 맞게 보인다.
    const date = Date.parse(res.headers.get("Date") || "");
    if (!isNaN(date)) D.setSkew(date - Date.now());
    return res.text().then((body) => {
      let data = null;
      try { data = body ? JSON.parse(body) : null; } catch (e) { data = null; }
      if (res.ok) return data;
      throw new ApiError(res.status, (data && typeof data.error === "string" && data.error) || byStatus(res.status));
    });
  }
  // o: { body(JSON 객체), signal }
  function request(method, path, o) {
    o = o || {};
    const headers = { Accept: "application/json" };
    const init = { method, cache: "no-store", credentials: "same-origin", headers, signal: o.signal };
    if (method !== "GET") {
      headers["Content-Type"] = "application/json";
      headers["X-Bot-Csrf"] = state.me ? state.me.csrf : "";
      init.body = JSON.stringify(o.body || {});
    }
    return fetch(path, init).then(read, (e) => { throw e && e.name === "AbortError" ? e : new ApiError(0, "network"); });
  }

  // 신원과 CSRF 토큰. 토큰은 인증된 GET으로만 받는다.
  function me(force) {
    if (state.me && !force) return Promise.resolve(state.me);
    if (!state.req) state.req = request("GET", "/v1/admin/me").then((d) => { state.me = d; state.req = null; return d; }, (e) => { state.req = null; throw e; });
    return state.req;
  }
  const get = (path, o) => request("GET", path, o);
  // 바꾸는 요청: 403 csrf면 /me를 다시 받아 한 번만 다시 보낸다(첫 요청은 CSRF 검사에서 막혀 실행되지 않았다).
  // o(선택): { signal } — 응답이 오지 않을 때 호출하는 쪽이 끊을 수 있게 한다.
  function post(path, body, o) {
    const signal = o && o.signal;
    return request("POST", path, { body, signal }).catch((e) => {
      if (!(e instanceof ApiError) || e.status !== 403 || e.word !== "csrf") throw e;
      return me(true).then(() => request("POST", path, { body, signal }));
    });
  }
  const q = (params) => {
    const out = [];
    Object.keys(params).forEach((k) => { const v = params[k]; if (v !== null && v !== undefined && v !== "") out.push(k + "=" + encodeURIComponent(v)); });
    return out.length ? "?" + out.join("&") : "";
  };
  const seg = encodeURIComponent;

  root.BotApi = { ApiError, me, get, post, q, seg, explain, DECIDE, RESOLVE, SENTENCE, get current() { return state.me; } };
})(window);
