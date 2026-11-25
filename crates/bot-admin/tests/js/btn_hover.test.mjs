// 단추 변형(.btn.X)이 자기 배경을 정하면 마우스를 올려도 그대로여야 한다. 기본 `.btn:hover:not(:disabled)`가 더 구체적이라,
// 변형이 호버 규칙을 따로 갖지 않으면 올리는 순간 배경이 회색으로 덮인다(긴급 정지가 회색 바탕에 흰 글씨가 됐던 일).
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";

const dir = new URL("../../ui/", import.meta.url);
const sheets = fs.readdirSync(dir).filter((f) => f.endsWith(".css")).map((f) => fs.readFileSync(new URL(f, dir), "utf8"));
const everything = sheets.join("\n");

test("every button variant that paints its own background has a hover rule that keeps it", () => {
  const variants = new Set();
  for (const text of sheets) for (const m of text.matchAll(/(\.btn(?:\.[a-z][a-z-]*)+)\s*\{([^}]*)\}/g)) if (/(^|[;\s])background\s*:/.test(m[2])) variants.add(m[1]);
  for (const must of [".btn.stop", ".btn.primary", ".btn.danger"]) assert.ok(variants.has(must), "변형을 찾지 못했다: " + must + " (찾은 것: " + [...variants].join(" ") + ")");
  for (const v of variants) assert.ok(everything.includes(v + ":hover:not(:disabled)"), v + "에 호버 규칙(" + v + ":hover:not(:disabled))이 없다");
});
