// 수동 점검(공개 CI는 돌리지 않는다): 보류 판정은 «첫 확인에 고정»된다. 단계 A에 영수증을 적고 첫 확인을 누른 뒤 다른 곳에서 A가 처리되면,
// 둘째 누름이 같은 영수증을 B에 기록하면 안 된다.
// 실행: 데모 서버를 띄우고(`DEMO_PORT=18790 cargo run -p bot-admin --example demo`), 시스템 Chrome과 playwright-core가 있는 곳에서
//   PLAYWRIGHT_CORE=<playwright-core/index.mjs 경로> PORT=18790 node tests/browser/hold_pin.mjs
// 데모 데이터는 메모리라 한 번 돌린 뒤에는 서버를 다시 시작해야 한다. 순수 규칙은 tests/js(cargo test가 노드로 돌린다)가 맡는다.
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
ok("보류 단계 두 개를 고를 수 있다", (await select.locator("option").count()) === 2);
const first = await select.inputValue();
ok("기본 선택은 첫 보류 단계 e-2", first === "e-2", first);
const form = card.locator("form").first();
await form.locator("input").fill("RCPT-FOR-A");
await form.locator("button[type=submit]").click(); // 첫 확인: 단계 e-2에 고정
await p.waitForTimeout(700);
ok("첫 확인 요약이 단계 e-2를 가리킨다", /notify|e-2/.test((await form.textContent()) || ""));
// 다른 운영자가 e-2를 먼저 처리한다(같은 서버에 직접 요청).
const other = await p.evaluate(async () => {
  const me = await (await fetch("/v1/admin/me", { credentials: "same-origin" })).json();
  const r = await fetch("/v1/admin/holds/personal/r-4/c-9/resolve", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", "X-Bot-Csrf": me.csrf }, body: JSON.stringify({ verdict: "confirmed", receipt: "OTHER-OPERATOR", effect: "e-2", hold_id: "h-e-2" }) });
  return r.status;
});
ok("다른 운영자의 e-2 처리가 성공한다", other === 200, String(other));
posts.length = 0; // 이 시점까지의 POST는 «다른 운영자»의 요청이다. 이후 이 화면이 보내는 것만 센다.
// 이 화면의 목록을 다시 불러오게 한다: 승인 카드를 처리하면 app.refresh(true)가 돈다.
await p.getByRole("button", { name: /^승인$/ }).first().click();
await p.waitForTimeout(1500);
const card2 = p.locator("article.hcard").first();
const text = (await card2.textContent()) || "";
ok("갱신 뒤 e-3만 남아 있다(선택 목록에 e-2가 없다)", (await card2.locator("select").count()) === 0 && /archive/.test(text), text.replace(/\s+/g, " ").slice(0, 100));
const stillArmed = await card2.locator("button[type=submit]").first().textContent();
ok("확인이 해제되고 버튼이 처음 문구로 돌아간다", !/한 번 더/.test(stillArmed || ""), stillArmed);
const typed = await card2.locator("form").first().locator("input").inputValue();
ok("입력한 영수증도 지워진다", typed === "", JSON.stringify(typed));
ok("다시 확인하라는 안내가 보인다", /이미 다른 곳에서 처리/.test(text));
// 운영자가 무심코 한 번 더 누르는 경우: 입력이 비어 있어 기록되지 않는다.
await card2.locator("form").first().locator("button[type=submit]").click();
await p.waitForTimeout(500);
ok("둘째 누름은 아무것도 보내지 않는다(빈 입력 거절)", posts.length === 0, JSON.stringify(posts));
const state = await p.evaluate(async () => ((await (await fetch("/v1/admin/holds", { credentials: "same-origin" })).json()).items[0]?.steps ?? []).map((s) => s.effect + ":" + s.state + ":" + (s.receipt || "")));
ok("e-3에는 어떤 영수증도 기록되지 않았다(보류가 그대로 남아 있다)", state.includes("e-3:held:"), state.length ? state.join(" ") : "보류가 닫혔다 — e-3이 판정됐다");
// 마지막 단계에서 호출 전체 보류로 바뀌면 기존 영수증을 보존하고 호출 ID에 한 번만 판정한다.
async function submitReceipt(form, receipt) {
  await form.locator("input").fill(receipt);
  const send = form.locator("button[type=submit]");
  await send.click();
  await p.waitForTimeout(650); // 화면의 두 번 누름 보호 시간(600ms)
  await Promise.all([p.waitForResponse((r) => r.url().includes("/resolve") && r.request().method() === "POST" && r.ok()), send.click()]);
}
await submitReceipt(card2.locator("form").first(), "ARCHIVE-RECEIPT");
const final = p.locator("article.hcard").filter({ hasText: "호출 최종 결과" });
await final.waitFor();
ok("최종 호출에는 전체 영수증 폼 하나만 있다", await final.locator("form").count() === 1 && await final.getByText("미실행으로 판정", { exact: true }).count() === 0);
ok("호출 카드에 기존 단계 영수증이 남는다", await final.getByText("file-17", { exact: true }).count() === 1 && await final.getByText("ARCHIVE-RECEIPT", { exact: true }).count() === 1);
await submitReceipt(final.locator("form"), "CALL-RECEIPT");
await final.waitFor({ state: "detached" });
const whole = posts.at(-1);
ok("호출 전체 식별자로 최종 영수증을 보낸다", !whole.effect && whole.hold_id !== "h-e-3" && !!whole.hold_id && whole.receipt === "CALL-RECEIPT");
ok("콘솔 오류·경고 0건", logs.length === 0, logs.join("|"));
await b.close();
for (const [s, n, e] of out) console.log(s, n, e ? "— " + e : "");
const failed = out.some(([s]) => s === "FAIL");
console.log(failed ? "\n결과: 실패 있음" : "\n결과: 모두 통과");
if (failed) process.exitCode = 1;
