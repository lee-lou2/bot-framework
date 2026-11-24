// 수동 점검(공개 CI는 돌리지 않는다): 둘째 클릭이 목록 갱신보다 먼저 가는 경우.
// 운영자가 단계 A에 영수증을 적고 첫 확인을 눌렀는데 다른 운영자가 A를 먼저 처리했다. 화면은 아직 모르고 둘째 클릭이 A로 간다 → 서버는 409로 막는다.
// 재조회 뒤 보류가 B 하나만 남아도 B의 입력칸에 A용 영수증이 남아 있으면 안 된다.
// 실행: 데모 서버를 새로 띄우고(`DEMO_PORT=18790 cargo run -p bot-admin --example demo`)
//   PLAYWRIGHT_CORE=<playwright-core/index.mjs 경로> PORT=18790 node tests/browser/hold_stale_click.mjs
import { pathToFileURL } from "node:url";
const pw = await import(pathToFileURL(process.env.PLAYWRIGHT_CORE).href);
const chromium = pw.chromium ?? pw.default.chromium;
const BASE = "http://127.0.0.1:" + process.env.PORT;
const out = [];
const ok = (n, c, e = "") => out.push([c ? "PASS" : "FAIL", n, e]);
const b = await chromium.launch({ channel: "chrome", headless: true });
const c = await b.newContext({ viewport: { width: 1440, height: 1000 }, httpCredentials: { username: "u", password: "demo-operator-secret" } });
const p = await c.newPage();
const logs = [];
p.on("console", (m) => { if (["error", "warning"].includes(m.type()) && !/status of 409/.test(m.text())) logs.push(m.text()); });
const sent = [];
p.on("request", (r) => { if (r.method() === "POST" && r.url().includes("/resolve")) sent.push(JSON.parse(r.postData() || "{}")); });
const answers = [];
p.on("response", (r) => { if (r.request().method() === "POST" && r.url().includes("/resolve")) answers.push(r.status()); });
await p.goto(BASE + "/ui", { waitUntil: "load" });
await p.waitForSelector("#tabs [role=tab]");
await p.click("#tab-decide"); await p.waitForTimeout(900);
const form = () => p.locator("article.hcard").first().locator("form").first();
await form().locator("input").fill("RCPT-FOR-A");
await form().locator("button[type=submit]").click(); // 첫 확인: e-2에 고정
await p.waitForTimeout(700);
const other = await p.evaluate(async () => {
  const me = await (await fetch("/v1/admin/me", { credentials: "same-origin" })).json();
  const r = await fetch("/v1/admin/holds/personal/r-4/c-9/resolve", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", "X-Bot-Csrf": me.csrf }, body: JSON.stringify({ verdict: "confirmed", receipt: "OTHER-OPERATOR", effect: "e-2", hold_id: "h-e-2" }) });
  return r.status;
});
ok("다른 운영자의 e-2 처리가 성공한다", other === 200, String(other));
sent.length = 0; answers.length = 0; // 이후 이 화면이 보내는 요청만 센다(화면은 아직 갱신되지 않았다)
await form().locator("button[type=submit]").click(); // 둘째 클릭: 낡은 화면이 e-2로 보낸다
await p.waitForTimeout(1500);
ok("낡은 화면의 둘째 클릭은 e-2로 한 번 갔고 서버가 409로 막았다", sent.length === 1 && sent[0].effect === "e-2" && answers[0] === 409, JSON.stringify([sent, answers]));
const card = p.locator("article.hcard").first();
ok("재조회 뒤 보류는 e-3 하나만 남았다", (await card.locator("select").count()) === 0 && /archive/.test((await card.textContent()) || ""));
const typed = await form().locator("input").inputValue();
ok("남은 단계의 영수증 입력칸에 A용 영수증이 없다", typed === "", JSON.stringify(typed));
const typedOther = await card.locator("form").nth(1).locator("input").inputValue();
ok("미실행 근거 입력칸도 비어 있다", typedOther === "", JSON.stringify(typedOther));
ok("이미 판정됐다는 안내가 보인다", /이미 판정된 보류/.test((await card.textContent()) || ""));
sent.length = 0;
await form().locator("button[type=submit]").click(); // 입력이 비어 있으니 아무것도 보내지 않는다
await p.waitForTimeout(500);
ok("빈 입력으로는 아무것도 보내지 않는다", sent.length === 0, JSON.stringify(sent));
const steps = await p.evaluate(async () => ((await (await fetch("/v1/admin/holds", { credentials: "same-origin" })).json()).items[0]?.steps ?? []).map((s) => s.effect + ":" + s.state + ":" + (s.receipt || "")));
ok("e-3은 영수증 없이 그대로 보류다", steps.includes("e-3:held:"), steps.join(" "));
ok("콘솔 오류·경고 0건(409 응답 로그 제외)", logs.length === 0, logs.join("|"));
await b.close();
for (const [s, n, e] of out) console.log(s, n, e ? "— " + e : "");
console.log(out.some(([s]) => s === "FAIL") ? "\n결과: 실패 있음" : "\n결과: 모두 통과");
