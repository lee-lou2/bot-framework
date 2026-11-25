// 수동 점검(공개 CI는 돌리지 않는다): «사전 승인» 탭을 실제 브라우저에서 눌러 본다 — 정지는 한 번, 재개는 두 번, 중복 클릭은 한 요청, 다른 운영자가 먼저 바꾼 뒤의 낡은 요청(409), 일반 주체에게는 탭이 없음.
// 실행: `PORT=18791 node tests/browser/preapproval_stub.mjs &` 로 계약 대역을 띄우고, 시스템 Chrome과 playwright-core가 있는 곳에서
//   PLAYWRIGHT_CORE=<playwright-core/index.mjs 경로> PORT=18791 node tests/browser/preapproval_stop.mjs     (스크린샷: SHOTS=<폴더>)
// 데이터는 메모리라 한 번 돌린 뒤에는 대역 서버를 다시 시작해야 한다. 순수 규칙·화면 동작은 tests/js(cargo test가 노드로 돌린다)가 맡는다.
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
const shot = async (p, name) => { await p.waitForTimeout(700); if (SHOTS) await p.screenshot({ path: SHOTS + "/" + name + ".png", fullPage: true }); }; // 카드가 나타나는 움직임이 끝난 뒤에 찍는다
// 브라우저는 409 응답을 «Failed to load resource»로 로그한다. 이 점검은 낡은 요청을 일부러 한 번 보내므로 그 한 건만 정상이다.
const unexpected = (logs) => logs.filter((l) => !/status of 409/.test(l));
const card = (p, name) => p.locator("article.pcard", { hasText: name });

// ---------- 운영자 ----------
{
  const { c, p, logs, posts } = await open("demo-operator-secret");
  await p.waitForSelector("article.pcard");
  ok("사전 승인 탭이 있고 선택돼 있다", (await p.locator("#tab-preapprovals").getAttribute("aria-selected")) === "true");
  ok("카드 4장(모든 프로필)", (await p.locator("article.pcard").count()) === 4);
  ok("정지된 카드는 «정지됨»과 «재개»", /정지됨/.test(await card(p, "weekly").textContent()) && (await card(p, "weekly").getByRole("button", { name: /^재개$/ }).count()) === 1);
  ok("상한에 닿은 카드는 알려 준다", /오늘 상한 도달/.test(await card(p, "weekly").textContent()));
  await shot(p, "01-list");
  // 정지: 한 번 누르면 한 요청, 그 요청은 화면이 본 값
  const stop = card(p, "notify").getByRole("button", { name: "긴급 정지" });
  const bg = () => stop.evaluate((el) => getComputedStyle(el).backgroundColor), rest = await bg();
  await stop.hover();
  await p.waitForTimeout(300);
  ok("긴급 정지 단추는 마우스를 올려도 채운 빨강이다(회색으로 덮이지 않는다)", rest !== "rgba(0, 0, 0, 0)" && (await bg()) === rest, rest + " → " + (await bg()));
  await stop.dblclick(); // 중복 클릭
  await p.waitForTimeout(600);
  ok("두 번 눌러도 요청은 하나", posts.length === 1, JSON.stringify(posts));
  ok("요청 주소·본문은 화면이 본 값(agent의 /는 %2F 한 조각)", posts[0]?.url === "/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/pause" && JSON.stringify(posts[0].body) === JSON.stringify({ grant_id: "grant-0001-aaaa", epoch: 4 }), JSON.stringify(posts[0]));
  ok("응답 뒤 «정지됨»", /정지됨/.test(await card(p, "notify").textContent()) && /정지했습니다/.test(await card(p, "notify").textContent()));
  await shot(p, "02-paused");
  // 재개: 두 번 눌러야 한다
  posts.length = 0;
  const resume = card(p, "notify").getByRole("button", { name: /^재개$/ });
  await resume.click();
  ok("재개 첫 누름은 요청을 보내지 않고 확인을 요구한다", posts.length === 0 && /한 번 더 눌러 재개/.test(await card(p, "notify").textContent()));
  await card(p, "notify").getByRole("button", { name: /한 번 더 눌러 재개/ }).click(); // 0.6초 보호 안: 무시
  ok("첫 누름 직후의 누름은 무시된다", posts.length === 0);
  await shot(p, "03-confirm"); // (움직임이 끝나길 700ms 기다린다 — 보호 시간이 지난 뒤)
  await card(p, "notify").getByRole("button", { name: /한 번 더 눌러 재개/ }).click();
  await p.waitForTimeout(600);
  ok("둘째 누름은 고정한 값을 보낸다", posts.length === 1 && posts[0].url.endsWith("/notify/resume") && posts[0].body.epoch === 5, JSON.stringify(posts));
  ok("응답 뒤 «동작 중»", /동작 중/.test(await card(p, "notify").textContent()));
  // 다른 운영자가 먼저 바꾼 뒤의 낡은 요청: 화면은 epoch 6을 보고 있고, 서버는 그 사이 두 번 바뀌었다
  posts.length = 0;
  await p.evaluate(async () => {
    const me = await (await fetch("/v1/admin/me", { credentials: "same-origin" })).json();
    const send = (a) => fetch("/v1/admin/preapprovals/personal/main%2Fdeveloper/notify/" + a, { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", "X-Bot-Csrf": me.csrf }, body: JSON.stringify({ grant_id: "grant-0001-aaaa", epoch: a === "pause" ? 6 : 7 }) });
    await send("pause"); await send("resume");
  });
  posts.length = 0;
  await card(p, "notify").getByRole("button", { name: "긴급 정지" }).click();
  await p.waitForTimeout(900);
  const text = (await card(p, "notify").textContent()) || "";
  ok("낡은 요청은 409로 거절되고 안내가 보인다", /다른 곳에서 먼저 바뀌었거나/.test(text), text.slice(0, 160));
  ok("그 뒤 화면은 서버의 현재 세대를 보인다(정지 세대 8)", /정지 세대 8/.test(text), text.slice(0, 160));
  // 다른 프로필 선택은 화면 필터다
  await p.selectOption("#profile", "other");
  await p.waitForTimeout(500);
  ok("프로필 other만 보인다", (await p.locator("article.pcard").count()) === 1 && /sweep/.test((await p.locator("article.pcard").textContent()) || ""));
  await shot(p, "04-filter");
  ok("콘솔 오류·경고는 일부러 낸 409 한 건뿐(CSP 위반 포함)", unexpected(logs).length === 0 && logs.length === 1, logs.join(" | "));
  await c.close();
}
// ---------- 좁은 화면 ----------
{
  const { c, p } = await open("demo-operator-secret", { width: 390, height: 800 });
  await p.waitForSelector("article.pcard");
  ok("좁은 화면에서도 가로로 넘치지 않는다", await p.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1));
  await shot(p, "05-narrow");
  await c.close();
}
// ---------- 일반 주체 ----------
{
  const { c, p, logs, gets } = await open("demo-guest-secret");
  await p.waitForTimeout(800);
  ok("일반 주체에게는 사전 승인 탭이 없다", (await p.locator("#tab-preapprovals").count()) === 0 && (await p.locator("#view-preapprovals").count()) === 0);
  ok("사전 승인 목록을 요청하지도 않는다", gets.length === 0, gets.join(","));
  ok("콘솔 오류·경고 0건", logs.length === 0, logs.join(" | "));
  await c.close();
}
await b.close();
for (const [s, n, e] of out) console.log(s, n, e ? "— " + e : "");
console.log(out.some(([s]) => s === "FAIL") ? "\n결과: 실패 있음" : "\n결과: 모두 통과");
process.exit(out.some(([s]) => s === "FAIL") ? 1 : 0);
