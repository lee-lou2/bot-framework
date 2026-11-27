// Vocabulary tables mapping server status, risk, reason, and fact codes to UI labels and pills.
(function (root) {
  "use strict";
  const D = root.BotDom, { h, pill, code, icon } = D;

  const RUN_STATE = { ready: ["Ready", "info", "off"], leased: ["Running", "info", "on"], waiting: ["Waiting", "warn", "held"], retry: ["Retrying", "warn", "held"], succeeded: ["Succeeded", "ok", "on"], dead: ["Dead", "bad", "error"], cancelled: ["Cancelled", "off", "off"] };
  const RUN_ORDER = ["ready", "leased", "waiting", "retry", "succeeded", "dead", "cancelled"];
  const CALL_STATE = { running: ["Running", "info", "on"], awaiting: ["Awaiting approval", "warn", "held"], done: ["Done", "ok", "on"], held: ["Held", "warn", "held"], denied: ["Denied", "off", "off"], failed: ["Failed", "bad", "error"] };
  const EFFECT_STATE = { started: ["Started", "info", "off"], confirmed: ["Confirmed", "ok", "on"], rejected: ["Rejected", "bad", "error"], held: ["Held", "warn", "held"] };
  const APPROVAL_STATE = { pending: ["Pending", "warn", "held"], approved: ["Approved", "ok", "on"], consumed: ["Consumed", "ok", "on"], denied: ["Denied", "off", "off"], expired: ["Expired", "off", "off"], superseded: ["Superseded", "warn", "error"], revoked: ["Revoked", "off", "off"] };
  const RISK = { read: ["Read", "off", null], write: ["Write", "warn", null], irreversible: ["Irreversible", "bad", "error"] };
  const CIRCUIT = { closed: ["Healthy", "ok", "on"], open: ["Open", "bad", "error"], half_open: ["Recovering", "warn", "held"] };
  const APPROVAL_REQ = { always: ["Always require approval", "bad"], policy: ["Approval by policy", "warn"], none: ["No approval required", "off"] };
  const CALL_ERROR = { denied: "Denied", timeout: "Timed out", invalid_result: "Invalid result format", outcome_unknown: "Outcome unknown", execution_failed: "Execution failed" };
  const HOLD_REASON = { outcome_unknown: "Outcome unknown", unknown_outcome: "Outcome unknown", owner_lost: "Previous execution outcome unconfirmed", no_evidence: "No evidence of execution", response_unknown: "Final response unknown", effects_unresolved: "Unresolved steps remain", previous_process_unconfirmed: "Previous runner exit unconfirmed", missing_transport: "Delivery adapter not configured" };
  const CONVERSATION = { direct: "Direct message", channel: "Channel", cli: "CLI", http: "HTTP", scheduled: "Scheduled" };
  const AUDIT_ACTION = {
    "approval.decide": "Approval decision", "hold.resolve": "Hold resolution", "profile.activate": "Profile activation",
    "tool.call": "Tool call", "preapproval.pause": "Pause pre-approval", "preapproval.resume": "Resume pre-approval",
    "run.resume": "Resume run", "run.cancel": "Cancel run", "run.cancelled": "Run cancelled",
  };
  const AUDIT_VERDICT = {
    allow: ["Allowed", "ok", "on"], deny: ["Denied", "off", "off"], ok: ["OK", "ok", "on"],
    confirmed: ["Confirmed executed", "ok", "on"], not_executed: ["Confirmed not executed", "warn", "held"],
    operator_requested: ["Operator requested", "info", "on"],
  };

  const fromTable = (t) => (key) => { const m = t[key]; return m ? pill(m[0], m[1], m[2]) : pill(key === null || key === undefined || key === "" ? "-" : "Other", "off"); };
  const runPill = fromTable(RUN_STATE), callPill = fromTable(CALL_STATE), effectPill = fromTable(EFFECT_STATE);
  const approvalPill = fromTable(APPROVAL_STATE), riskPill = fromTable(RISK), circuitPill = fromTable(CIRCUIT);
  const verdictPill = (v) => { const m = AUDIT_VERDICT[v]; return m ? pill(m[0], m[1], m[2]) : pill(v || "-", "off"); };
  const holdReason = (r) => HOLD_REASON[r] || "Other";
  const callError = (r) => (r ? CALL_ERROR[r] || "Other" : "-");
  const conversation = (k) => CONVERSATION[k] || "Other";

  function approvalReq(v) {
    const m = APPROVAL_REQ[v];
    if (!m) return pill("Other", "off");
    return h("span", { class: "pill pill-" + m[1] }, [v === "none" ? null : icon("lock", { size: 12 }), h("span", { text: m[0] })]);
  }
  const hash8 = (hex) => h("code", { text: String(hex || "").slice(0, 8) || "-", title: hex || null });

  function waiting(w) {
    if (!w) return null;
    if (w.on === "approval") return h("span", null, ["Waiting for approval."]);
    if (w.on === "child") return h("span", null, ["Waiting for child run ", code(w.run), "."]);
    if (w.on === "hold") return h("span", null, ["Waiting for hold resolution on call ", code(w.call), "."]);
    return h("span", null, ["Waiting."]);
  }
  function waitingShort(w) {
    if (!w) return "";
    return w.on === "approval" ? "Awaiting approval" : w.on === "child" ? "Waiting on child " + w.run : w.on === "hold" ? "Held on " + w.call : "Waiting";
  }

  const MORE = 6;
  function tags(names, mono) {
    if (!names.length) return "-";
    const row = h("div", { class: "ctags" });
    const add = (n) => row.appendChild(h("span", { class: "ctag" + (mono ? " mono" : ""), text: n }));
    names.slice(0, MORE).forEach(add);
    if (names.length > MORE) {
      const more = h("button", { type: "button", class: "ctag more", "aria-expanded": "false", text: "+" + (names.length - MORE), onclick: () => { names.slice(MORE).forEach(add); row.removeChild(more); } });
      row.appendChild(more);
    }
    return row;
  }
  const list = (v) => (!v || v === "-" ? [] : String(v).split(", "));

  const FACT = {
    profile: { revision: "Revision", policy_revision: "Policy revision", timezone: "Timezone", model: "Model", agents: "Agents", root: "Root agent" },
    agent: { model: "Model", scopes: "Scopes", builtin: "Built-in tools", skills: "Skills", triggers: "Triggers", root: "Root" },
    skill: { server: "Tool server", tools: "Tools" },
    tool: { class: "Risk", scopes: "Scopes", approval: "Approval", server: "Tool server", revision: "Revision", external: "Third-party egress", effects: "Step ledger", egress: "Allowed egress hosts", destination: "Destination" },
    trigger: { on: "Kind", when: "Condition", order: "Eval order", private: "Private input", principal: "Principal", beat: "Coalesce", approver: "Approver", response: "Response mode", target: "Target" },
    binding: { adapter: "Adapter", detail: "Detail", agent: "Target agent" },
    scope: { granted: "Policy allowed", approval: "Requires approval", roles: "Roles" },
    secret: { declared: "Declared" },
  };
  const TRIGGER_ON = { cron: "Scheduled (cron)", event: "Event", http: "HTTP request" };
  const ADAPTER = { cli: "CLI", slack: "Slack", http: "HTTP", openai: "OpenAI API" };
  const RESPONSE = { accepted: "Accept immediately (202)", sync: "Return tool result" };
  const factLabel = (kind, key) => (FACT[kind] && FACT[kind][key]) || key;

  function destination(v) {
    if (v === "reply") return "Reply to origin conversation";
    const m = /^(.+) <- (.+)$/.exec(v);
    return m ? "Recipient " + m[1] + " · Content " + m[2] : v;
  }
  function factNode(card, f) {
    const k = f.key, v = f.val, kind = card.kind;
    if (v === "" || v === null || v === undefined) return "-";
    if (v === "true") return "Yes";
    if (v === "false") return "No";
    if (k === "scopes" || k === "roles" || k === "egress") return tags(list(v), true);
    if (kind === "tool" && k === "class") return riskPill(v);
    if (kind === "tool" && k === "approval") return approvalReq(v);
    if (k === "revision" || k === "policy_revision" || k === "server" || k === "model" || k === "principal" || k === "approver" || k === "timezone") return code(v);
    if (kind === "tool" && k === "destination") return destination(v);
    if (kind === "trigger" && k === "on") return TRIGGER_ON[v] || v;
    if (kind === "trigger" && k === "response") return RESPONSE[v] || v;
    if (kind === "trigger" && k === "target") return v === "agent" ? "Instruct agent" : /^tool /.test(v) ? "Run tool " + v.slice(5) : v;
    if (kind === "binding" && k === "adapter") return ADAPTER[v] || v;
    return v;
  }

  root.BotLabels = {
    RUN_STATE, RUN_ORDER, CALL_STATE, CALL_ERROR, EFFECT_STATE, APPROVAL_STATE, RISK, HOLD_REASON, CONVERSATION, AUDIT_ACTION, AUDIT_VERDICT,
    runPill, callPill, effectPill, approvalPill, riskPill, circuitPill, verdictPill, approvalReq, holdReason, callError, conversation,
    hash8, waiting, waitingShort, tags, list, factLabel, factNode, destination,
  };
})(window);
