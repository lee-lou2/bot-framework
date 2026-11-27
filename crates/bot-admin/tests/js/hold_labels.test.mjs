import test from "node:test";
import assert from "node:assert/strict";
import { all, button, install, load, textOf } from "./support/fakedom.mjs";

install();
load("dom.js", "labels.js");
const { holdReason } = window.BotLabels;

test("the known call recovery reasons have operator-facing labels", () => {
  assert.equal(holdReason("response_unknown"), "Final response unknown");
  assert.equal(holdReason("effects_unresolved"), "Unresolved steps remain");
  assert.equal(holdReason("previous_process_unconfirmed"), "Previous runner exit unconfirmed");
  assert.equal(holdReason("missing_transport"), "Delivery adapter not configured");
  assert.equal(holdReason("other"), "Other");
});

test("the last pending step warns that the final call needs its own receipt", async () => {
  const tool = { kind: "tool", profile: "personal", run: "run", call: "call", tool: "fixture.post", reason: "response_unknown", since_ms: 0 };
  const step = { effect: "step", step: "post", state: "held", hold_id: "hold-1" };
  const items = [
    { ...tool, steps: [step] },
    { ...tool, run: "run-other", hold_id: "hold-2", steps: [] },
  ];
  window.BotApi = { q: () => "", get: async (path) => ({ items: path.endsWith("/holds") ? items : [] }) };
  load("holdpin.js", "decide.js");
  const box = document.createElement("section");
  const view = window.BotDecide.create({ me: () => ({ operator: true }), profile: () => "personal", noteProfiles() {} }, box);
  await view.load({ profile: "personal", current: () => true });
  const hint = "Once the receipt for the last step is recorded, a final call outcome card opens";
  const cards = all(box, (node) => node.classList.contains("hcard"));
  assert.equal(cards.length, 2);
  assert.ok(textOf(cards[0]).includes(hint));
  assert.ok(!textOf(cards[1]).includes(hint));
});

test("the not-executed form explains what will happen to the call", async () => {
  const tool = { kind: "tool", profile: "personal", call: "call", tool: "fixture.post", reason: "effects_unresolved", since_ms: 0 };
  const step = { effect: "step", step: "post", state: "held", hold_id: "hold-2" };
  const confirmed = { effect: "first", step: "prepare", state: "confirmed", receipt: "receipt-1" };
  const started = { effect: "other", step: "other", state: "started" };
  const cases = [
    { steps: [confirmed, step], next: "A final call outcome card opens." },
    { steps: [confirmed, step, started], next: "Waits until the remaining steps finish." },
    { steps: [step], next: "Resumes with the same call ID." },
  ];
  const items = cases.map(({ steps }, i) => ({ ...tool, run: "run-" + i, steps }));
  window.BotApi = { q: () => "", get: async (path) => ({ items: path.endsWith("/holds") ? items : [] }) };
  load("holdpin.js", "decide.js");
  const box = document.createElement("section");
  const view = window.BotDecide.create({ me: () => ({ operator: true }), profile: () => "personal", noteProfiles() {} }, box);
  await view.load({ profile: "personal", current: () => true });

  const cards = all(box, (node) => node.classList.contains("hcard"));
  assert.equal(cards.length, cases.length);
  cards.forEach((card, i) => {
    const form = all(card, (node) => node.localName === "form" && node.getAttribute("aria-labelledby").startsWith("hf-not_executed-"))[0];
    const help = textOf(all(form, (node) => node.classList.contains("rf-help"))[0]);
    const input = all(form, (node) => node.localName === "input")[0];
    input.value = "No matching request found at destination";
    form.dispatch("submit");
    const sum = all(form, (node) => node.classList.contains("rf-sum"))[0];
    const summary = textOf(sum);
    assert.equal(sum.hidden, false);
    button(form, "Cancel").click();
    const { next } = cases[i];
    assert.ok(help.includes(next), help);
    assert.ok(summary.includes(next), summary);
    assert.ok(summary.includes(input.value), summary);
    assert.equal(help.includes("with the same call ID"), i === 2);
    assert.equal(summary.includes("with the same call ID"), i === 2);
  });
});

test("a missing_transport reply hold hides the retry form and explains recovery", async () => {
  const items = [
    { kind: "reply", hold_id: "hold-1", profile: "personal", run: "r1", call: "reply:r1:0", tool: "reply", reason: "missing_transport", since_ms: 0, steps: [] },
    { kind: "reply", hold_id: "hold-2", profile: "personal", run: "r2", call: "reply:r2:0", tool: "reply", reason: "owner_lost", since_ms: 0, steps: [] },
  ];
  window.BotApi = { q: () => "", get: async (path) => ({ items: path.endsWith("/holds") ? items : [] }) };
  load("holdpin.js", "decide.js");
  const box = document.createElement("section");
  const view = window.BotDecide.create({ me: () => ({ operator: true }), profile: () => "personal", noteProfiles() {} }, box);
  await view.load({ profile: "personal", current: () => true });

  const cards = all(box, (node) => node.classList.contains("hcard"));
  assert.equal(cards.length, 2);

  const missingForms = all(cards[0], (node) => node.localName === "form");
  assert.equal(missingForms.length, 1);
  assert.ok(missingForms[0].getAttribute("aria-labelledby").startsWith("hf-confirmed-"));
  const missingText = textOf(cards[0]);
  assert.ok(!missingText.includes("Record as not executed"), missingText);
  assert.ok(missingText.includes("Cannot automatically resend"), missingText);

  const normalForms = all(cards[1], (node) => node.localName === "form");
  assert.equal(normalForms.length, 2);
});
