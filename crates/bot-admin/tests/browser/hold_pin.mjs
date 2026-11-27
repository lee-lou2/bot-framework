// Manual check (skipped in public CI): hold resolution pins to the first confirmation.
// If step A is resolved elsewhere after arming a receipt, the second press must not write it to step B.
// Run against the demo server (`DEMO_PORT=18790 cargo run -p bot-admin --example demo`) with Chrome + playwright-core:
//   PLAYWRIGHT_CORE=<path/to/playwright-core/index.mjs> PORT=18790 node tests/browser/hold_pin.mjs
// Demo state is in-memory; restart the server between runs. Pure rules are tested in tests/js via `cargo test`.
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
p.on("console", (m) => { if (["error", "warning"].includes(m.type())) logs.push(m.text()); });
const posts = [];
p.on("request", (r) => { if (r.method() === "POST" && r.url().includes("/resolve")) posts.push(JSON.parse(r.postData() || "{}")); });
await p.goto(BASE + "/ui", { waitUntil: "load" });
await p.waitForSelector("#tabs [role=tab]");
await p.click("#tab-decide"); await p.waitForTimeout(900);
const card = p.locator("article.hcard").first();
const select = card.locator("select");
ok("two held steps are selectable", (await select.locator("option").count()) === 2);
const first = await select.inputValue();
ok("default selection is first held step e-2", first === "e-2", first);
const form = card.locator("form").first();
await form.locator("input").fill("RCPT-FOR-A");
await form.locator("button[type=submit]").click(); // first press pins step e-2
await p.waitForTimeout(700);
ok("confirmation summary points to step e-2", /notify|e-2/.test((await form.textContent()) || ""));
// Another operator resolves e-2 directly on the same server.
const other = await p.evaluate(async () => {
  const me = await (await fetch("/v1/admin/me", { credentials: "same-origin" })).json();
  const r = await fetch("/v1/admin/holds/personal/r-4/c-9/resolve", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", "X-Bot-Csrf": me.csrf }, body: JSON.stringify({ verdict: "confirmed", receipt: "OTHER-OPERATOR", effect: "e-2", hold_id: "h-e-2" }) });
  return r.status;
});
ok("other operator resolves e-2", other === 200, String(other));
posts.length = 0; // exclude the other operator's POST; count only requests from this UI
// Trigger app.refresh(true) by deciding an approval card.
await p.getByRole("button", { name: /^Approve$/ }).first().click();
await p.waitForTimeout(1500);
const card2 = p.locator("article.hcard").first();
const text = (await card2.textContent()) || "";
ok("after refresh only e-3 remains (no select for e-2)", (await card2.locator("select").count()) === 0 && /archive/.test(text), text.replace(/\s+/g, " ").slice(0, 100));
const stillArmed = await card2.locator("button[type=submit]").first().textContent();
ok("arming disarms and button returns to initial label", !/again/i.test(stillArmed || ""), stillArmed);
const typed = await card2.locator("form").first().locator("input").inputValue();
ok("entered receipt is cleared", typed === "", JSON.stringify(typed));
ok("re-check notice is visible", /already.*elsewhere|cleared/i.test(text));
// Accidental second click sends nothing because the input was cleared.
await card2.locator("form").first().locator("button[type=submit]").click();
await p.waitForTimeout(500);
ok("second click sends nothing (empty input rejected)", posts.length === 0, JSON.stringify(posts));
const state = await p.evaluate(async () => ((await (await fetch("/v1/admin/holds", { credentials: "same-origin" })).json()).items[0]?.steps ?? []).map((s) => s.effect + ":" + s.state + ":" + (s.receipt || "")));
ok("e-3 remains held with no receipt recorded", state.includes("e-3:held:"), state.length ? state.join(" ") : "hold closed — e-3 was resolved");
// Transitioning from the last step to the call-wide hold keeps prior receipts and settles once on the call hold ID.
async function submitReceipt(form, receipt) {
  await form.locator("input").fill(receipt);
  const send = form.locator("button[type=submit]");
  await send.click();
  await p.waitForTimeout(650); // past the UI's 600ms double-click guard
  await Promise.all([p.waitForResponse((r) => r.url().includes("/resolve") && r.request().method() === "POST" && r.ok()), send.click()]);
}
await submitReceipt(card2.locator("form").first(), "ARCHIVE-RECEIPT");
const final = p.locator("article.hcard").filter({ hasText: /final.*result/i });
await final.waitFor();
ok("final call card has only the whole-call receipt form", await final.locator("form").count() === 1 && await final.getByText(/not executed/i).count() === 0);
ok("final call card preserves prior step receipts", await final.getByText("file-17", { exact: true }).count() === 1 && await final.getByText("ARCHIVE-RECEIPT", { exact: true }).count() === 1);
await submitReceipt(final.locator("form"), "CALL-RECEIPT");
await final.waitFor({ state: "detached" });
const whole = posts.at(-1);
ok("final receipt posts against the call-wide hold ID", !whole.effect && whole.hold_id !== "h-e-3" && !!whole.hold_id && whole.receipt === "CALL-RECEIPT");
ok("zero console errors or warnings", logs.length === 0, logs.join("|"));
await b.close();
for (const [s, n, e] of out) console.log(s, n, e ? "— " + e : "");
const failed = out.some(([s]) => s === "FAIL");
console.log(failed ? "\nResult: failures found" : "\nResult: all passed");
if (failed) process.exitCode = 1;
