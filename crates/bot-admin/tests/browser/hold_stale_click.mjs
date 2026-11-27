// Manual check (not run in public CI): second click races ahead of list refresh.
// An operator arms step A with a receipt, another operator resolves A first, and the stale second click targets A -> 409.
// After refresh leaves only step B, step B's inputs must not retain step A's receipt.
// Run with `DEMO_PORT=18790 cargo run -p bot-admin --example demo`, then:
//   PLAYWRIGHT_CORE=<path/to/playwright-core/index.mjs> PORT=18790 node tests/browser/hold_stale_click.mjs
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
await form().locator("button[type=submit]").click(); // arm pinned to e-2
await p.waitForTimeout(700);
const other = await p.evaluate(async () => {
  const me = await (await fetch("/v1/admin/me", { credentials: "same-origin" })).json();
  const r = await fetch("/v1/admin/holds/personal/r-4/c-9/resolve", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", "X-Bot-Csrf": me.csrf }, body: JSON.stringify({ verdict: "confirmed", receipt: "OTHER-OPERATOR", effect: "e-2", hold_id: "h-e-2" }) });
  return r.status;
});
ok("other operator resolves e-2", other === 200, String(other));
sent.length = 0; answers.length = 0;
await form().locator("button[type=submit]").click(); // stale click still targets e-2
await p.waitForTimeout(1500);
ok("stale second click targeted e-2 once and server returned 409", sent.length === 1 && sent[0].effect === "e-2" && answers[0] === 409, JSON.stringify([sent, answers]));
const card = p.locator("article.hcard").first();
ok("refresh leaves only step e-3 held", (await card.locator("select").count()) === 0 && /archive/.test((await card.textContent()) || ""));
const typed = await form().locator("input").inputValue();
ok("remaining step receipt input is cleared", typed === "", JSON.stringify(typed));
const typedOther = await card.locator("form").nth(1).locator("input").inputValue();
ok("not_executed evidence input is also cleared", typedOther === "", JSON.stringify(typedOther));
ok("already-resolved notice is visible", /already (resolved|judged|decided)/i.test((await card.textContent()) || ""));
sent.length = 0;
await form().locator("button[type=submit]").click();
await p.waitForTimeout(500);
ok("empty input submits nothing", sent.length === 0, JSON.stringify(sent));
const steps = await p.evaluate(async () => ((await (await fetch("/v1/admin/holds", { credentials: "same-origin" })).json()).items[0]?.steps ?? []).map((s) => s.effect + ":" + s.state + ":" + (s.receipt || "")));
ok("e-3 remains held without a receipt", steps.includes("e-3:held:"), steps.join(" "));
ok("zero console errors or warnings (excluding 409)", logs.length === 0, logs.join("|"));
await b.close();
for (const [s, n, e] of out) console.log(s, n, e ? "— " + e : "");
console.log(out.some(([s]) => s === "FAIL") ? "\nResult: FAILED" : "\nResult: ALL PASSED");
