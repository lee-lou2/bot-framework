// Manual contract stub server (not run in public CI): serves real `ui/` assets from disk with matching CSP and
// security headers, backed by `tests/js/support/contract.mjs` instead of PostgreSQL.
// Run: `PORT=18791 node tests/browser/preapproval_stub.mjs` (synthetic secrets: demo-operator-secret, demo-guest-secret).
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { contract, row } from "../js/support/contract.mjs";

const here = path.dirname(fileURLToPath(import.meta.url)), ui = path.join(here, "../../ui");
// Read CSP directly from server source to avoid drift.
const rust = fs.readFileSync(path.join(here, "../../src/assets.rs"), "utf8");
const CSP = /pub const CSP: &str = "([\s\S]*?)";/.exec(rust)[1].replace(/\\\n\s*/g, "");
const MIME = { html: "text/html; charset=utf-8", css: "text/css; charset=utf-8", js: "text/javascript; charset=utf-8" };
const OPERATOR = "demo-operator-secret", GUEST = "demo-guest-secret";

export const server = contract([
  row(),
  row({ trigger: "weekly", paused: true, epoch: 7, used_today: 24, max_per_day: 24, grant_id: "grant-0002-bbbbcccc" }),
  row({ agent: "main", trigger: "digest", used_today: 0, max_per_day: 8, grant_id: "grant-0003-dddd" }),
  row({ profile: "other", agent: "main/ops", trigger: "sweep", used_today: 11, max_per_day: 12, grant_id: "grant-0004-eeee" }),
]);

function who(req) {
  const basic = /^Basic (.+)$/.exec(req.headers.authorization || "");
  const pass = basic ? Buffer.from(basic[1], "base64").toString().split(":").slice(1).join(":") : "";
  return pass === OPERATOR ? "operator" : pass === GUEST ? "guest" : null;
}

const listener = http.createServer((req, res) => {
  const role = who(req), url = new URL(req.url, "http://x");
  const send = (status, type, text, extra) => { res.writeHead(status, { "content-type": type, "cache-control": "no-store", "x-content-type-options": "nosniff", "content-security-policy": CSP, ...extra }); res.end(text); };
  if (!role) return send(401, "application/json", '{"error":"unauthorized"}', { "www-authenticate": 'Basic realm="bot"' });
  if (url.pathname === "/ui" || url.pathname === "/ui/" || url.pathname.startsWith("/ui/")) {
    const name = url.pathname === "/ui" || url.pathname === "/ui/" ? "index.html" : url.pathname.slice(4);
    const file = path.join(ui, name);
    if (!/^[a-z0-9._-]+$/.test(name) || !fs.existsSync(file)) return send(404, "application/json", '{"error":"not_found"}');
    return send(200, MIME[name.split(".").pop()], fs.readFileSync(file));
  }
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const raw = Buffer.concat(chunks).toString();
    const call = { method: req.method, path: url.pathname, headers: { "X-Bot-Csrf": req.headers["x-bot-csrf"] }, body: raw ? JSON.parse(raw) : null };
    if (url.pathname === "/v1/admin/me") return send(200, "application/json", JSON.stringify({ principal: role === "operator" ? "local:owner" : "local:guest", operator: role === "operator", csrf: server.csrf }));
    // Non-operator initial views receive empty lists; operator APIs remain restricted.
    if (url.pathname === "/v1/admin/runs" || url.pathname === "/v1/admin/approvals") return send(200, "application/json", '{"items":[],"next":null}');
    if (role !== "operator") return send(403, "application/json", '{"error":"denied"}');
    if (url.pathname === "/v1/admin/status") return send(200, "application/json", JSON.stringify({ at_ms: Date.now(), build: { name: "stub", version: "0" }, process: { uptime_secs: 1, profiles: ["personal", "other"] }, profiles: { personal: {}, other: {} } }));
    const out = server.handler(call);
    send(out.status, "application/json", JSON.stringify(out.body));
  });
});
listener.listen(Number(process.env.PORT || 0), "127.0.0.1", () => console.log("listening " + listener.address().port));
