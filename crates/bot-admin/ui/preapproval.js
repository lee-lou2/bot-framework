// Pure pre-approval rules: row validation, pause/resume request pinning, reconciliation, and response verification.
(function (root) {
  "use strict";

  const ACTIONS = ["pause", "resume"];
  // Disallow control characters and the key separator "|"; agent may contain "/".
  const LINE = /^[^\u0000-\u001f\u007f-\u009f|]{1,256}$/;
  const isText = (v) => typeof v === "string" && LINE.test(v);
  const isCount = (v) => typeof v === "number" && Number.isSafeInteger(v) && v >= 0;

  function valid(r) {
    return !!r && typeof r === "object" && !Array.isArray(r)
      && isText(r.profile) && isText(r.agent) && isText(r.trigger) && isText(r.grant_id)
      && isCount(r.epoch) && typeof r.paused === "boolean" && isCount(r.used_today) && isCount(r.max_per_day);
  }
  const key = (r) => [r.profile, r.agent, r.trigger].join("|");
  const before = (a, b) => (key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0);
  const sort = (rows) => rows.slice().sort(before);

  // Drop invalid or duplicate-key rows without exposing controls for them.
  function parse(body) {
    if (!body || typeof body !== "object" || !Array.isArray(body.items)) return null;
    const items = body.items.filter(valid), seen = {};
    items.forEach((r) => { seen[key(r)] = (seen[key(r)] || 0) + 1; });
    const rows = items.filter((r) => seen[key(r)] === 1);
    return { rows: sort(rows), skipped: body.items.length - rows.length };
  }

  const offered = (r) => (r.paused ? "resume" : "pause");
  function usage(r) {
    const used = r.used_today, max = r.max_per_day;
    return { used, max, pct: max > 0 ? Math.min(100, Math.floor((used * 100) / max)) : 100, left: Math.max(0, max - used), full: used >= max };
  }
  const counts = (rows) => ({ total: rows.length, paused: rows.filter((r) => r.paused).length });

  function arm(r, action) {
    if (!valid(r) || ACTIONS.indexOf(action) < 0 || offered(r) !== action) return null;
    return { key: key(r), action, profile: r.profile, agent: r.agent, trigger: r.trigger, grant_id: r.grant_id, epoch: r.epoch };
  }
  // Encode each path segment exactly once (e.g. main/developer -> main%2Fdeveloper).
  function path(pin) {
    if (ACTIONS.indexOf(pin.action) < 0) throw new Error("Unknown action");
    return "/v1/admin/preapprovals/" + [pin.profile, pin.agent, pin.trigger].map(encodeURIComponent).join("/") + "/" + pin.action;
  }
  const payload = (pin) => (pin ? { grant_id: pin.grant_id, epoch: pin.epoch } : null);

  function reconcile(pin, rows) {
    if (!pin) return { ok: true };
    const now = rows.find((r) => key(r) === pin.key);
    if (!now) return { ok: false, reason: "gone" };
    if (now.grant_id !== pin.grant_id) return { ok: false, reason: "renewed" };
    if (now.epoch !== pin.epoch || offered(now) !== pin.action) return { ok: false, reason: "changed" };
    return { ok: true };
  }

  function settle(pin, reply) {
    if (!valid(reply) || key(reply) !== pin.key || reply.grant_id !== pin.grant_id) return null;
    return reply.paused === (pin.action === "pause") ? reply : null;
  }
  const replace = (rows, row) => sort(rows.filter((r) => key(r) !== key(row)).concat([row]));

  const ERR = {
    stale: "Changed elsewhere or replaced by a new pre-approval. The list was reloaded; review the current state and try again.",
    denied: "Only operators can pause or resume pre-approvals.",
    not_found: "Pre-approval not found. Its declaration may have been removed. Reloading the list.",
    invalid: "Invalid request payload. Reloading the list.",
    unavailable: "Could not reach the store, so we could not confirm whether the change was applied. Reload the list to check the current state.",
    network: "Could not reach the server, so we could not confirm whether the change was applied. Reload the list to check the current state.",
    timeout: "No response received in time, so we could not confirm whether the change was applied. Reload the list to check the current state.",
    mismatch: "The server response did not match the request, so the displayed state was not changed. Reload the list to check the current state.",
  };
  const WHY = {
    gone: "The pre-approval being confirmed disappeared from the list (its declaration may have been removed). Confirmation cleared.",
    renewed: "A new pre-approval was issued (activation changed). Confirmation cleared; review the new scope before confirming again.",
    changed: "State changed elsewhere. Confirmation cleared; review the current state before confirming again.",
  };

  const api = { ACTIONS, valid, key, parse, offered, usage, counts, arm, path, payload, reconcile, settle, replace, sort, ERR, WHY };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotPreapproval = api;
})(typeof window !== "undefined" ? window : globalThis);
