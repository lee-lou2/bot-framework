// 관리 API 계약(사전 승인 정지·재개)의 가짜 서버: 실제 서버(PG)가 아니라 계약의 규칙만 흉내 낸다. 화면 시험과 브라우저 점검이 함께 쓴다.
//   GET  /v1/admin/preapprovals                       → 200 {"items":[행]}
//   POST /v1/admin/preapprovals/{profile}/{agent}/{trigger}/pause|resume  본문 {grant_id, epoch}
//        같은 승인(grant)·현재 epoch·이미 그 상태 → 200 무변경 / 같은 승인·현재 epoch·다른 상태 → 바꾸고 epoch+1, 200 새 행
//        직전 성공 명령의 정확한 재전달(응답 유실)이고 그 뒤 전환이 없으면 → 200 현재 행 / 그 밖(옛 epoch·승인 교체) → 409 {"error":"stale"} · 모르는 대상 404 · 모양 틀림 400
// 경로 조각은 한 번만 디코딩한다(`main%2Fdeveloper` → `main/developer`).
export function row(over) {
  return { profile: "personal", agent: "main/developer", trigger: "notify", grant_id: "grant-0001-aaaa", epoch: 4, paused: false, used_today: 3, max_per_day: 24, ...over };
}

export function contract(initial) {
  const rows = (initial || [row()]).map((r) => ({ ...r }));
  const last = {};
  const server = {
    rows, posts: [], gets: 0, audits: 0, csrf: "csrf-1", deny: false,
    find: (p, a, t) => rows.find((r) => r.profile === p && r.agent === a && r.trigger === t),
    handler(call) {
      if (call.method === "GET" && call.path === "/v1/admin/preapprovals") { server.gets++; return { status: 200, body: { items: rows.map((r) => ({ ...r })) } }; }
      if (call.method === "GET" && call.path === "/v1/admin/me") return { status: 200, body: { principal: "local:owner", operator: true, csrf: server.csrf } };
      const m = /^\/v1\/admin\/preapprovals\/([^/]+)\/([^/]+)\/([^/]+)\/(pause|resume)$/.exec(call.path);
      if (call.method !== "POST" || !m) return { status: 404, body: { error: "not_found" } };
      server.posts.push(call);
      if (call.headers["X-Bot-Csrf"] !== server.csrf) return { status: 403, body: { error: "csrf" } };
      if (server.deny) return { status: 403, body: { error: "denied" } };
      const [p, a, t] = m.slice(1, 4).map(decodeURIComponent);
      const r = server.find(p, a, t), b = call.body;
      const keys = b && typeof b === "object" ? Object.keys(b).sort().join(",") : "";
      if (keys !== "epoch,grant_id" || typeof b.grant_id !== "string" || !Number.isSafeInteger(b.epoch)) return { status: 400, body: { error: "invalid" } };
      if (!r) return { status: 404, body: { error: "not_found" } };
      const action = m[4], want = action === "pause", mark = `${p}|${a}|${t}`, prev = last[mark];
      if (b.grant_id !== r.grant_id) return { status: 409, body: { error: "stale" } };
      if (b.epoch === r.epoch) {
        if (r.paused !== want) { r.paused = want; r.epoch += 1; server.audits++; last[mark] = { action, expected: b.epoch, result: r.epoch, grant: r.grant_id }; }
        return { status: 200, body: { ...r } };
      }
      if (prev && prev.grant === r.grant_id && prev.action === action && prev.expected === b.epoch && prev.result === r.epoch) return { status: 200, body: { ...r } };
      return { status: 409, body: { error: "stale" } };
    },
  };
  return server;
}
