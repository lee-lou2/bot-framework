// Manual check (not run in public CI): under the same profile, run, and call ID, a tool hold and a reply hold
// (legacy data) render as two separate cards and resolve independently. A hold without a hold_id disables submit buttons
// and prompts to reload.
// Run with `DEMO_PAIR=1 DEMO_PORT=18790 cargo run -p bot-admin --example demo`, then:
//   PLAYWRIGHT_CORE=<path/to/playwright-core/index.mjs> PORT=18790 node tests/browser/hold_pair.mjs
// Demo data is in-memory; restart the server between runs. Pure rules live in tests/js, server checks in tests/suite/hold_pairs.rs.
import { pathToFileURL } from "node:url";
const pw = await import(pathToFileURL(process.env.PLAYWRIGHT_CORE).href);
const chromium = pw.chromium ?? pw.default.chromium;
const BASE = "http://127.0.0.1:" + process.env.PORT;
const out = [];
const ok = (n, c, e = "") => out.push([c ? "PASS" : "FAIL", n, e]);
const b = await chromium.launch({ channel: "chrome", headless: true });
const c = await b.newContext({ viewport: { width: 1440, height: 1400 }, httpCredentials: { username: "u", password: "demo-operator-secret" } });
const p = await c.newPage();
const logs = [];
p.on("console", (m) => { if (["error", "warning"].includes(m.type())) logs.push(m.text()); });
const posts = [], listReads = [];
p.on("request", (r) => {
  if (r.method() === "POST" && r.url().includes("/resolve")) posts.push({ url: new URL(r.url()).pathname, body: JSON.parse(r.postData() || "{}") });
  if (r.method() === "GET" && /\/v1\/admin\/holds(\?|$)/.test(r.url())) listReads.push(r.url());
});
await p.goto(BASE + "/ui", { waitUntil: "load" });
await p.waitForSelector("#tabs [role=tab]");
await p.click("#tab-decide"); await p.waitForTimeout(1000);

const cards = () => p.locator("article.hcard");
const forCall = (call) => cards().filter({ hasText: call });
const badge = async () => ((await p.locator("#hold-title em").textContent()) || "").trim();
const serverCount = await p.evaluate(async () => (await (await fetch("/v1/admin/holds", { credentials: "same-origin" })).json()).items.length);
ok("server reports 6 holds including pair", serverCount === 6, String(serverCount));
ok("card count matches badge count", (await cards().count()) === 6 && (await badge()) === "6", (await cards().count()) + " / " + (await badge()));

const report = async () => {
  await b.close();
  for (const [s, n, e] of out) console.log(s, n, e ? "— " + e : "");
  const failed = out.some(([s]) => s === "FAIL");
  console.log(failed ? "\nResult: FAILED" : "\nResult: ALL PASSED");
  process.exit(failed ? 1 : 0);
};
const pair = forCall("c-pair");
ok("same call renders two hold cards", (await pair.count()) === 2, String(await pair.count()));
if ((await pair.count()) !== 2) await report();
const tool = pair.filter({ hasNotText: /\breply\b/i }), reply = pair.filter({ hasText: /\breply\b/i });
ok("one tool card and one reply card", (await tool.count()) === 1 && (await reply.count()) === 1);
const form = (card, n) => card.locator("form").nth(n); // 0 = confirm receipt, 1 = not_executed
await form(tool, 0).locator("input").fill("RCPT-T");
await form(reply, 1).locator("input").fill("NO-RECORD-R");
ok("inputs stay isolated across paired cards", (await form(tool, 1).locator("input").inputValue()) === "" && (await form(reply, 0).locator("input").inputValue()) === "");
await form(tool, 0).locator("button[type=submit]").click();
await p.waitForTimeout(300);
const toolArmed = (await form(tool, 0).locator("button[type=submit].is-armed").count()) === 1;
const replyArmed = (await reply.locator("button[type=submit].is-armed").count()) > 0;
ok("only the tool card is armed", toolArmed && !replyArmed, `${toolArmed} / ${replyArmed}`);
ok("reply card buttons remain enabled", !(await form(reply, 0).locator("button[type=submit]").isDisabled()) && !(await form(reply, 1).locator("button[type=submit]").isDisabled()));
await p.waitForTimeout(700); // double-click guard window
posts.length = 0;
await form(tool, 0).locator("button[type=submit]").click();
await p.waitForTimeout(1500);
ok("only the tool hold submitted with its own hold_id", posts.length === 1 && posts[0].url === "/v1/admin/holds/personal/r-pair/c-pair/resolve" && posts[0].body.hold_id === "h-pair-t" && posts[0].body.verdict === "confirmed" && posts[0].body.receipt === "RCPT-T" && !("effect" in posts[0].body), JSON.stringify(posts));
ok("only the resolved tool card disappears", (await forCall("c-pair").count()) === 1 && (await forCall("c-pair").filter({ hasText: /\breply\b/i }).count()) === 1, String(await forCall("c-pair").count()));
ok("remaining reply card keeps its typed input", (await form(forCall("c-pair"), 1).locator("input").inputValue()) === "NO-RECORD-R");
await form(forCall("c-pair"), 1).locator("button[type=submit]").click();
await p.waitForTimeout(800);
posts.length = 0;
await form(forCall("c-pair"), 1).locator("button[type=submit]").click();
await p.waitForTimeout(1500);
ok("reply hold resolves with its own hold_id", posts.length === 1 && posts[0].body.hold_id === "h-pair-r" && posts[0].body.verdict === "not_executed" && posts[0].body.evidence === "NO-RECORD-R", JSON.stringify(posts));
ok("both paired cards disappear leaving four", (await forCall("c-pair").count()) === 0 && (await cards().count()) === 4 && (await badge()) === "4", (await cards().count()) + " / " + (await badge()));

const noid = forCall("c-noid");
const notice = noid.locator(".rf-noid");
const reloadBtn = notice.locator("button");
ok("missing hold_id shows notice and reload button", (await notice.isVisible()) && /hold.?id|identifier/i.test((await notice.textContent()) || "") && (await reloadBtn.isVisible()));
ok("both submit buttons are disabled without hold_id", (await form(noid, 0).locator("button[type=submit]").isDisabled()) && (await form(noid, 1).locator("button[type=submit]").isDisabled()));
ok("missing hold_id notice appears on only one card", (await p.locator(".rf-noid:visible").count()) === 1, String(await p.locator(".rf-noid:visible").count()));
posts.length = 0;
await form(noid, 0).locator("input").fill("RCPT-X");
await form(noid, 0).locator("input").press("Enter");
await p.waitForTimeout(600);
ok("Enter does not arm or submit without hold_id", posts.length === 0 && (await form(noid, 0).locator("button[type=submit].is-armed").count()) === 0, JSON.stringify(posts));
const before = listReads.length;
await reloadBtn.click();
await p.waitForTimeout(1000);
ok("reload button refetches holds list", listReads.length > before, before + " -> " + listReads.length);
ok("buttons stay disabled when hold_id is still absent", (await forCall("c-noid").locator("form").nth(0).locator("button[type=submit]").isDisabled()) && posts.length === 0);
ok("zero console errors or warnings", logs.length === 0, logs.join("|"));
await report();
