// 수동 점검(공개 CI는 돌리지 않는다): 같은 프로필·run·호출 ID 아래 도구 보류와 회신 보류가 함께 있으면(구판 데이터) 카드도 둘이고 따로 판정된다.
// 보류 식별자가 없는 대상은 판정 단추가 꺼지고 «다시 불러오기»를 안내한다.
// 실행: 쌍 데이터를 켠 데모 서버를 띄우고(`DEMO_PAIR=1 DEMO_PORT=18790 cargo run -p bot-admin --example demo`), 시스템 Chrome과 playwright-core가 있는 곳에서
//   PLAYWRIGHT_CORE=<playwright-core/index.mjs 경로> PORT=18790 node tests/browser/hold_pair.mjs
// 데모 데이터는 메모리라 한 번 돌린 뒤에는 서버를 다시 시작해야 한다. 순수 규칙은 tests/js, 서버 쪽은 tests/hold_pairs.rs가 맡는다.
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
ok("서버의 보류는 6개다(쌍 둘 포함)", serverCount === 6, String(serverCount));
ok("목록 숫자와 카드 수가 같다", (await cards().count()) === 6 && (await badge()) === "6", (await cards().count()) + " / " + (await badge()));

// ---- 한 호출 아래 도구 보류와 회신 보류: 카드 둘, 입력·확인 상태가 섞이지 않는다 ----
const report = async () => {
  await b.close();
  for (const [s, n, e] of out) console.log(s, n, e ? "— " + e : "");
  const failed = out.some(([s]) => s === "FAIL");
  console.log(failed ? "\n결과: 실패 있음" : "\n결과: 모두 통과");
  process.exit(failed ? 1 : 0);
};
const pair = forCall("c-pair");
ok("같은 호출의 보류가 카드 둘로 보인다", (await pair.count()) === 2, String(await pair.count()));
if ((await pair.count()) !== 2) await report(); // 카드가 합쳐졌으면 이어서 누를 대상이 없다
const tool = pair.filter({ hasNotText: "회신" }), reply = pair.filter({ hasText: "회신" });
ok("도구 카드와 회신 카드가 하나씩이다", (await tool.count()) === 1 && (await reply.count()) === 1);
const form = (card, n) => card.locator("form").nth(n); // 0 영수증 확인, 1 미실행 확인
await form(tool, 0).locator("input").fill("RCPT-T");
await form(reply, 1).locator("input").fill("NO-RECORD-R");
ok("한 카드에 적은 입력이 다른 카드에 보이지 않는다", (await form(tool, 1).locator("input").inputValue()) === "" && (await form(reply, 0).locator("input").inputValue()) === "");
await form(tool, 0).locator("button[type=submit]").click(); // 도구 카드 첫 확인
await p.waitForTimeout(300);
const toolBtn = (await form(tool, 0).locator("button[type=submit]").textContent()) || "";
const replyBtn0 = (await form(reply, 0).locator("button[type=submit]").textContent()) || "";
const replyBtn1 = await form(reply, 1).locator("button[type=submit]").textContent();
ok("도구 카드만 확인 중이다", /한 번 더/.test(toolBtn) && !/한 번 더/.test(replyBtn0 + replyBtn1), [toolBtn, replyBtn0, replyBtn1].join(" | "));
ok("회신 카드의 단추는 눌려 있지 않다", !(await form(reply, 0).locator("button[type=submit]").isDisabled()) && !(await form(reply, 1).locator("button[type=submit]").isDisabled()));
await p.waitForTimeout(700); // 더블클릭 방지 간격
posts.length = 0;
await form(tool, 0).locator("button[type=submit]").click(); // 도구 카드 기록
await p.waitForTimeout(1500);
ok("도구 보류만 판정을 보냈다(도구의 식별자)", posts.length === 1 && posts[0].url === "/v1/admin/holds/personal/r-pair/c-pair/resolve" && posts[0].body.hold_id === "h-pair-t" && posts[0].body.verdict === "confirmed" && posts[0].body.receipt === "RCPT-T" && !("effect" in posts[0].body), JSON.stringify(posts));
ok("판정한 도구 카드만 사라지고 회신 카드가 남는다", (await forCall("c-pair").count()) === 1 && (await forCall("c-pair").filter({ hasText: "회신" }).count()) === 1, String(await forCall("c-pair").count()));
ok("남은 회신 카드가 입력을 그대로 가진다", (await form(forCall("c-pair"), 1).locator("input").inputValue()) === "NO-RECORD-R");
await form(forCall("c-pair"), 1).locator("button[type=submit]").click();
await p.waitForTimeout(800);
posts.length = 0;
await form(forCall("c-pair"), 1).locator("button[type=submit]").click();
await p.waitForTimeout(1500);
ok("회신 보류도 자기 식별자로 판정된다", posts.length === 1 && posts[0].body.hold_id === "h-pair-r" && posts[0].body.verdict === "not_executed" && posts[0].body.evidence === "NO-RECORD-R", JSON.stringify(posts));
ok("쌍이 모두 사라지고 나머지 네 장이 남는다", (await forCall("c-pair").count()) === 0 && (await cards().count()) === 4 && (await badge()) === "4", (await cards().count()) + " / " + (await badge()));

// ---- 보류 식별자가 없는 대상: 판정 단추가 꺼지고 다시 불러오기를 안내한다 ----
const noid = forCall("c-noid");
const notice = noid.locator(".rf-noid");
ok("식별자 없는 보류에 안내와 다시 불러오기가 보인다", (await notice.isVisible()) && /식별자가 없어 판정할 수 없습니다/.test((await notice.textContent()) || "") && (await notice.getByRole("button", { name: "다시 불러오기" }).isVisible()));
ok("두 판정 단추가 모두 꺼져 있다", (await form(noid, 0).locator("button[type=submit]").isDisabled()) && (await form(noid, 1).locator("button[type=submit]").isDisabled()));
ok("다른 보류 카드에는 안내가 보이지 않는다(보이는 안내는 하나뿐)", (await p.locator(".rf-noid:visible").count()) === 1, String(await p.locator(".rf-noid:visible").count()));
posts.length = 0;
await form(noid, 0).locator("input").fill("RCPT-X");
await form(noid, 0).locator("input").press("Enter"); // 단추가 꺼져 있어 Enter로도 제출되지 않는다
await p.waitForTimeout(600);
ok("Enter로도 아무것도 보내지 않는다", posts.length === 0 && !/한 번 더/.test((await form(noid, 0).locator("button[type=submit]").textContent()) || ""), JSON.stringify(posts));
const before = listReads.length;
await noid.getByRole("button", { name: "다시 불러오기" }).click();
await p.waitForTimeout(1000);
ok("다시 불러오기가 목록을 다시 읽는다", listReads.length > before, before + " → " + listReads.length);
ok("그래도 식별자가 없으면 안내와 꺼진 단추가 그대로다", (await forCall("c-noid").locator("form").nth(0).locator("button[type=submit]").isDisabled()) && posts.length === 0);
ok("콘솔 오류·경고 0건", logs.length === 0, logs.join("|"));
await report();
