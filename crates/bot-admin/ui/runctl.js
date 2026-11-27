// Operator run resume/cancel controls pinning the first observed run state and command ID for safe retries.
(function (root) {
  "use strict";
  const ARM_MS = 12000, MIN_CONFIRM_MS = 600;
  const CANCEL_STATES = ["ready", "retry", "waiting", "leased"];
  const LABEL = { resume: "Resume", cancel: "Cancel run", held: "View in Approvals & Holds" };
  const count = (v) => Number.isSafeInteger(v) && v >= 0;

  function offered(d, operator) {
    const r = d.run, scope = d.control;
    if (!operator || r.run !== r.root || r.parent !== null || !scope) return null;
    if (scope.held === true) return "held";
    if (scope.held !== false || !count(r.generation) || !count(scope.claim_epoch)) return null;
    if (r.state === "dead") return "resume";
    return scope.cancel_requested === false && CANCEL_STATES.includes(r.state) ? "cancel" : null;
  }

  function payload(d, id) {
    return { command_id: id, state: d.run.state, generation: d.run.generation, claim_epoch: d.control.claim_epoch };
  }

  function canConfirm(elapsed) {
    return elapsed >= MIN_CONFIRM_MS && elapsed < ARM_MS;
  }

  function create(app) {
    const { h } = root.BotDom, A = root.BotApi;
    let active = null;

    function mount(d, action) {
      const r = d.run;
      let pending = null, armedAt = null, timer, sending = false, settled = false, blocked = action === "held";
      const note = h("span", { role: "status", text: blocked ? "This run tree has a hold awaiting resolution." : "" });
      const undo = h("button", { type: "button", class: "btn sm", onclick: dismiss });
      const button = h("button", { type: "button", class: "btn sm", onclick: choose });
      const node = h("div", { class: "rd-wait is-none", onkeydown: (e) => {
        if (e.key === "Escape" && armedAt !== null) { disarm(); button.focus(); }
      } }, [button, undo, note]);
      active = { node, disarm, busy: () => sending || (pending !== null && !settled) };
      sync();
      return node;

      function sync() {
        undo.hidden = pending === null || sending || settled;
        undo.textContent = armedAt === null ? "Reload latest state" : "Cancel confirmation";
        button.disabled = sending || settled;
        button.classList.toggle("danger", action === "cancel" && !blocked);
        button.textContent = sending ? "Sending…" : blocked ? LABEL.held
          : armedAt !== null ? "Confirm resume" : pending && !settled ? "Resend same command" : LABEL[action];
      }

      function disarm() {
        if (armedAt === null) return;
        clearTimeout(timer);
        armedAt = null;
        pending = null;
        note.textContent = "";
        sync();
      }

      function dismiss() {
        if (armedAt !== null) { disarm(); return; }
        pending = null;
        note.textContent = "";
        sync();
        app.refresh(true);
      }

      function choose() {
        if (sending || settled) return;
        if (blocked) { app.go("decide", { profile: r.profile }); return; }
        if (armedAt !== null) {
          const elapsed = Date.now() - armedAt;
          if (elapsed >= ARM_MS) { disarm(); return; }
          if (!canConfirm(elapsed)) return;
          clearTimeout(timer);
          armedAt = null;
        } else if (!pending) {
          if (action === "cancel" && !root.confirm("Request cancellation of this run and its child runs?")) return;
          const id = Array.from(root.crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
          pending = payload(d, id);
          if (action === "resume") {
            armedAt = Date.now();
            timer = setTimeout(disarm, ARM_MS);
            note.textContent = "Re-queues this run. Press again within 12 seconds to confirm.";
            sync();
            return;
          }
        }
        return send();
      }

      async function send() {
        sending = true;
        note.textContent = "";
        sync();
        const path = "/v1/admin/runs/" + A.seg(r.profile) + "/" + A.seg(r.run) + "/" + action;
        try {
          const response = await A.post(path, pending);
          if (!response || response.ok !== true) {
            note.textContent = "Could not confirm the response. You can resend the same command.";
            return;
          }
          settled = true;
          note.textContent = action === "resume" ? "Resume requested." : "Cancellation requested.";
          if (node.isConnected && !node.closest("[hidden]")) {
            app.toast(note.textContent);
            app.refresh(true);
          }
        } catch (e) {
          blocked = e.word === "held";
          if (blocked) {
            pending = null;
            note.textContent = "Resolve the pending hold first.";
          } else if (e.word === "network" || e.word === "unavailable") {
            note.textContent = "Could not confirm the response. You can resend the same command.";
          } else {
            pending = null;
            note.textContent = A.explain(e, { stale: "Run state changed. Refresh to see the latest state." });
            if (node.isConnected && !node.closest("[hidden]")) {
              app.toast(note.textContent, "bad");
              app.refresh(true);
            }
          }
        } finally {
          sending = false;
          sync();
        }
      }
    }

    return {
      mount,
      hide() { if (active) active.disarm(); },
      busy() { return !!(active && active.node.isConnected && active.busy()); },
    };
  }

  const api = { offered, payload, canConfirm, create };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.BotRunControl = api;
})(typeof window !== "undefined" ? window : globalThis);
