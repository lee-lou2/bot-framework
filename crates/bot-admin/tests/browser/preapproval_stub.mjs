// 수동 점검용 계약 대역 서버(공개 CI는 돌리지 않는다): 실제 `ui/` 파일을 디스크에서 같은 CSP·보안 머리글로 내주고, 로그인(Basic)·`/v1/admin/me`(CSRF)·사전 승인 API는
// `tests/js/support/contract.mjs`의 계약 규칙으로 답한다. 실제 서버(PG)가 아니다 — 화면이 계약대로 움직이는지 브라우저에서 보려는 도구다.
// 실행: `PORT=18791 node tests/browser/preapproval_stub.mjs` (합성 비밀: 운영자 demo-operator-secret, 일반 demo-guest-secret). 데이터는 메모리다.
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { contract, row } from "../js/support/contract.mjs";

const here = path.dirname(fileURLToPath(import.meta.url)), ui = path.join(here, "../../ui");
// 화면이 쓰는 CSP는 서버 소스의 값을 그대로 읽어 쓴다(복사해 두면 어긋난다).
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
    // 일반 주체의 첫 화면(실행)이 부르는 목록은 비어 있게 답한다(실제 서버는 자기 범위만 준다). 그 밖의 운영자 API는 운영자만 쓴다.
    if (url.pathname === "/v1/admin/runs" || url.pathname === "/v1/admin/approvals") return send(200, "application/json", '{"items":[],"next":null}');
    if (role !== "operator") return send(403, "application/json", '{"error":"denied"}');
    if (url.pathname === "/v1/admin/status") return send(200, "application/json", JSON.stringify({ at_ms: Date.now(), build: { name: "stub", version: "0" }, process: { uptime_secs: 1, profiles: ["personal", "other"] }, profiles: { personal: {}, other: {} } }));
    const out = server.handler(call);
    send(out.status, "application/json", JSON.stringify(out.body));
  });
});
listener.listen(Number(process.env.PORT || 0), "127.0.0.1", () => console.log("listening " + listener.address().port));
