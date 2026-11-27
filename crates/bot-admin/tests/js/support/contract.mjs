// In-memory stub implementing the pre-approval pause/resume Admin API contract for JS and browser tests.
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
      // Exact redelivery of the last committed transition is idempotent.
      if (prev && prev.grant === r.grant_id && prev.action === action && prev.expected === b.epoch && prev.result === r.epoch) return { status: 200, body: { ...r } };
      return { status: 409, body: { error: "stale" } };
    },
  };
  return server;
}
