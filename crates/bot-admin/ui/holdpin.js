// Pure hold-resolution pinning rules: freeze target step, hold ID, verdict, and input on first press so concurrent refreshes cannot misapply a receipt.
(function (root) {
  "use strict";

  // A step already marked not_executed (resolution set) is waiting for resume and is no longer selectable.
  const open = (s) => s.state === "held" && !s.resolution;
  const held = (steps) => (steps || []).filter(open).map((s) => s.effect);

  function holdIdFor(hold, effect) {
    if (effect === null || effect === undefined) return hold.hold_id || null;
    const step = (hold.steps || []).find((s) => s.effect === effect);
    return (step && step.hold_id) || null;
  }

  // Call-wide hold with any confirmed step accepts only a final call receipt, never a whole-call retry.
  const receiptOnly = (hold) => !!hold.hold_id && (hold.steps || []).some((s) => s.state === "confirmed");

  // Include hold kind so legacy data with both a tool hold and a reply hold under the same call ID renders two distinct cards.
  const keyOf = (hold) => [hold.profile, hold.run, hold.call, hold.kind || ""].join("|");

  const decidable = (hold, effect) => holdIdFor(hold, effect) !== null;

  function arm(kind, effect, text, holdId) {
    if (holdId === undefined || holdId === null || holdId === "") return null;
    return { kind, effect: effect === undefined ? null : effect, text, hold: holdId };
  }

  // Signals `changed` when the selected step moves or receives a new hold ID so callers wipe stale input.
  function retarget(current, currentHold, hold) {
    const cur = current === undefined ? null : current;
    const curHold = currentHold === undefined ? null : currentHold;
    const ids = held(hold.steps);
    const next = ids.indexOf(cur) >= 0 ? cur : ids.length ? ids[0] : null;
    const nextHold = holdIdFor(hold, next);
    const moved = cur !== null && next !== cur;
    const renewed = curHold !== null && nextHold !== curHold;
    return { effect: next, holdId: nextHold, changed: moved || renewed };
  }

  const STALE = { conflict: true, not_found: true };
  const staleWord = (word) => STALE[word] === true;

  function reconcile(pin, hold) {
    if (!pin) return { ok: true };
    const steps = hold.steps || [];
    if (pin.effect === null) {
      if (steps.some(open) || (steps.length && !hold.hold_id) || (pin.kind === "not_executed" && receiptOnly(hold))) return { ok: false, reason: "stepped" };
      return (hold.hold_id || null) === pin.hold ? { ok: true } : { ok: false, reason: "renewed" };
    }
    const now = steps.find((s) => s.effect === pin.effect && open(s));
    if (!now) return { ok: false, reason: "gone" };
    return (now.hold_id || null) === pin.hold ? { ok: true } : { ok: false, reason: "renewed" };
  }

  function payload(pin, fields) {
    if (!pin || pin.hold === null) return null;
    const body = { verdict: pin.kind };
    body[fields[pin.kind]] = pin.text.trim();
    if (pin.effect !== null) body.effect = pin.effect;
    body.hold_id = pin.hold;
    return body;
  }

  const WHY = {
    gone: "The step you were confirming was already resolved elsewhere. Inputs were cleared; please review the remaining steps.",
    stepped: "The steps for this call have changed. Inputs were cleared; please review and confirm again.",
    renewed: "The same target was placed on a new hold. Inputs were cleared; please confirm the new hold.",
    noid: "This hold has no hold ID and cannot be resolved. Reload the list and try again.",
  };

  const api = { open, held, receiptOnly, holdIdFor, keyOf, decidable, arm, retarget, staleWord, reconcile, payload, WHY };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotHoldPin = api;
})(typeof window !== "undefined" ? window : globalThis);
