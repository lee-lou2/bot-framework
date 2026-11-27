// Manual check (skipped in public CI): exercises the Pre-approvals tab in a real browser —
// single-click pause, two-click resume, double-click dedup, stale 409 after concurrent change, and non-operator hiding.
// Run against the contract stub (`PORT=18791 node tests/browser/preapproval_stub.mjs &`) with Chrome + playwright-core:
//   PLAYWRIGHT_CORE=<path/to/playwright-core/index.mjs> PORT=18791 node tests/browser/preapproval_stop.mjs  (screenshots: SHOTS=<dir>)
// Stub state is in-memory; restart between runs. Pure rules and DOM logic are tested in tests/js via `cargo test`.
import { pathToFileURL } from "node:url";
const pw = await import(pathToFileURL(process.env.PLAYWRIGHT_CORE).href);
const chromium = pw.chromium ?? pw.default.chromium;
const BASE = "http://127.0.0.1:" + process.env.PORT, SHOTS = process.env.SHOTS;
const out = [];
const ok = (n, c, e = "") => out.push([c ? "PASS" : "FAIL", n, e]);
const b = await chromium.launch({ channel: "chrome", headless: true });
async function open(password, viewport) {
  const c = await b.newContext({ viewport: viewport || { width: 1280, height: 900 }, httpCredentials: { username: "u", password } });
  const p = await c.newPage(), logs = [], posts = [], gets = [];
  p.on("console", (m) => { if (["error", "warning"].includes(m.type())) logs.push(m.text()); });
  p.on("pageerror", (e) => logs.push("pageerror: " + e.message));
  p.on("request", (r) => { if (r.method() === "POST") posts.push({ url: new URL(r.url()).pathname, body: JSON.parse(r.postData() || "{}") }); else if (r.url().includes("/v1/admin/preapprovals")) gets.push(r.url()); });
  await p.goto(BASE + "/ui#preapprovals", { waitUntil: "load" });
  await p.waitForSelector("#tabs [role=tab]");
  return { c, p, logs, posts, gets };
}
const shot = async (p, name) => { await p.waitForTimeout(700); if (SHOTS) await p.screenshot({ path: SHOTS + "/" + name + ".png", fullPage: true }); };
// Chromium logs "Failed to load resource" for the intentional 409 check; ignore only that expected entry.
const unexpected = (logs) => logs.filter((l) => !/status of 409/.test(l));
const card = (p, name) => p.locator("article.pcard", { hasText: name });

// ---------- Operator ----------
{
  const { c, p, logs, posts } = await open("demo-operator-secret");
  await p.waitForSelector("article.pcard");
  ok("pre-approvals tab is present and selected", (await p.locator("#tab-preapprovals").getAttribute("aria-selected")) === "true");
  ok("renders 4 cards across all profiles", (await p.locator("article.pcard").count()) === 4);
  ok("paused card shows Paused and Resume", /Paused/i.test(await card(p, "weekly").textContent()) && (await card(p, "weekly").getByRole("button", { name: /^Resume$/i }).count()) === 1);
  ok("exhausted card shows daily limit indicator", /limit reached/i.test(await card(p, "weekly").textContent()));
  await shot(p, "01-list");
  // Pause: single click sends one request with the rendered values.
  const stop = card(p, "notify").getByRole("button", { name: /Emergency stop|Pause/i });
  const bg = () => stop.evaluate((el) => getComputedStyle(el).backgroundColor), rest = await bg();
  await stop.hover();
  await p.waitForTimeout(300);
  ok("emergency stop button keeps its filled background on hover", rest !== "rgba(0, 0, 0, 0)" && (await bg()) === rest, rest + " → " + (await bg()));
  await stop.dblclick();
  await p.waitForTimeout(600);
  ok("double-click sends only one request", posts.length === 1, JSON.stringify(posts));
  ok("request path and body match rendered row (agent slash encoded as %2F)", posts[0]?.url === "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/pause" && JSON.stringify(posts[0].body) === JSON.stringify({ grant_id: "grant-0001-aaaa", epoch: 4 }), JSON.stringify(posts[0]));
  ok("card switches to Paused after response", /Paused/i.test(await card(p, "notify").textContent()));
  await shot(p, "02-paused");
  // Resume: requires two clicks separated by the 600ms guard.
  posts.length = 0;
  const resume = card(p, "notify").getByRole("button", { name: /^Resume$/i });
  await resume.click();
  ok("first resume click arms confirmation without sending", posts.length === 0 && /again.*resume|resume.*again/i.test(await card(p, "notify").textContent()));
  await card(p, "notify").getByRole("button", { name: /again.*resume|resume.*again/i }).click(); // within 600ms guard: ignored
  ok("immediate second click within guard window is ignored", posts.length === 0);
  await shot(p, "03-confirm"); // waits 700ms so the guard window elapses
  await card(p, "notify").getByRole("button", { name: /again.*resume|resume.*again/i }).click();
  await p.waitForTimeout(600);
  ok("second resume click sends pinned grant_id and epoch", posts.length === 1 && posts[0].url.endsWith("/notify/resume") && posts[0].body.epoch === 5, JSON.stringify(posts));
  ok("card switches to Active after response", /Active|Running/i.test(await card(p, "notify").textContent()));
  // Stale request after another operator advances the epoch twice (6 -> 8).
  posts.length = 0;
  await p.evaluate(async () => {
    const me = await (await fetch("/v1/admin/me", { credentials: "same-origin" })).json();
    const send = (a) => fetch("/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/" + a, { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", "X-Bot-Csrf": me.csrf }, body: JSON.stringify({ grant_id: "grant-0001-aaaa", epoch: a === "pause" ? 6 : 7 }) });
    await send("pause"); await send("resume");
  });
  posts.length = 0;
  await card(p, "notify").getByRole("button", { name: /Emergency stop|Pause/i }).click();
  await p.waitForTimeout(900);
  const text = (await card(p, "notify").textContent()) || "";
  ok("stale request returns 409 and shows notice", /changed elsewhere|replaced|stale/i.test(text), text.slice(0, 160));
  ok("UI refreshes to show server epoch 8", /epoch\s*8/i.test(text), text.slice(0, 160));
  // Profile dropdown filters client-side.
  await p.selectOption("#profile", "other");
  await p.waitForTimeout(500);
  ok("profile filter shows only profile other", (await p.locator("article.pcard").count()) === 1 && /sweep/.test((await p.locator("article.pcard").textContent()) || ""));
  await shot(p, "04-filter");
  ok("console errors/warnings limited to the intentional 409", unexpected(logs).length === 0 && logs.length === 1, logs.join(" | "));
  await c.close();
}
// ---------- Narrow viewport ----------
{
  const { c, p } = await open("demo-operator-secret", { width: 390, height: 800 });
  await p.waitForSelector("article.pcard");
  ok("no horizontal overflow on narrow viewport", await p.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1));
  await shot(p, "05-narrow");
  await c.close();
}
// ---------- Non-operator ----------
{
  const { c, p, logs, gets } = await open("demo-guest-secret");
  await p.waitForTimeout(800);
  ok("non-operator does not see pre-approvals tab", (await p.locator("#tab-preapprovals").count()) === 0 && (await p.locator("#view-preapprovals").count()) === 0);
  ok("non-operator never requests pre-approvals list", gets.length === 0, gets.join(","));
  ok("zero console errors or warnings for non-operator", logs.length === 0, logs.join(" | "));
  await c.close();
}
await b.close();
for (const [s, n, e] of out) console.log(s, n, e ? "— " + e : "");
console.log(out.some(([s]) => s === "FAIL") ? "\nResult: failures found" : "\nResult: all passed");
process.exit(out.some(([s]) => s === "FAIL") ? 1 : 0);
