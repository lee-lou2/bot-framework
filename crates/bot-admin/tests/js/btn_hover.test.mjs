// Button variants that paint their own background must define :hover:not(:disabled) so the base rule cannot override them.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";

const dir = new URL("../../ui/", import.meta.url);
const sheets = fs.readdirSync(dir).filter((f) => f.endsWith(".css")).map((f) => fs.readFileSync(new URL(f, dir), "utf8"));
const everything = sheets.join("\n");

test("every button variant that paints its own background has a hover rule that keeps it", () => {
  const variants = new Set();
  for (const text of sheets) for (const m of text.matchAll(/(\.btn(?:\.[a-z][a-z-]*)+)\s*\{([^}]*)\}/g)) if (/(^|[;\s])background\s*:/.test(m[2])) variants.add(m[1]);
  for (const must of [".btn.stop", ".btn.primary", ".btn.danger"]) assert.ok(variants.has(must), "missing variant: " + must + " (found: " + [...variants].join(" ") + ")");
  for (const v of variants) assert.ok(everything.includes(v + ":hover:not(:disabled)"), v + " is missing hover rule (" + v + ":hover:not(:disabled))");
});
