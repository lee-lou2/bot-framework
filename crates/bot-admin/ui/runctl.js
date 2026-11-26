// 실행 상세의 운영자 명령. 처음 본 조건과 명령 ID를 붙들고 확인·재전송한다.
(function (root) {
  "use strict";
  const ARM_MS = 12000, MIN_CONFIRM_MS = 600;
  const CANCEL_STATES = ["ready", "retry", "waiting", "leased"];
  const LABEL = { resume: "재개", cancel: "실행 취소", held: "승인·보류에서 보기" };
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
      const note = h("span", { role: "status", text: blocked ? "이 실행 트리에 판정을 기다리는 보류가 있습니다." : "" });
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
        undo.textContent = armedAt === null ? "최신 상태 불러오기" : "확인 닫기";
        button.disabled = sending || settled;
        button.classList.toggle("danger", action === "cancel" && !blocked);
        button.textContent = sending ? "전송 중…" : blocked ? LABEL.held
          : armedAt !== null ? "재개 확인" : pending && !settled ? "같은 명령 다시 보내기" : LABEL[action];
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
          if (action === "cancel" && !root.confirm("이 실행과 하위 실행의 취소를 요청할까요?")) return;
          const id = Array.from(root.crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
          pending = payload(d, id);
          if (action === "resume") {
            armedAt = Date.now();
            timer = setTimeout(disarm, ARM_MS);
            note.textContent = "같은 실행을 다시 대기열에 넣습니다. 12초 안에 한 번 더 누르세요.";
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
            note.textContent = "응답을 확인하지 못했습니다. 같은 명령을 다시 보낼 수 있습니다.";
            return;
          }
          settled = true;
          note.textContent = action === "resume" ? "재개를 요청했습니다." : "취소를 요청했습니다.";
          if (node.isConnected && !node.closest("[hidden]")) {
            app.toast(note.textContent);
            app.refresh(true);
          }
        } catch (e) {
          blocked = e.word === "held";
          if (blocked) {
            pending = null;
            note.textContent = "먼저 보류 판정을 완료하세요.";
          } else if (e.word === "network" || e.word === "unavailable") {
            note.textContent = "응답을 확인하지 못했습니다. 같은 명령을 다시 보낼 수 있습니다.";
          } else {
            pending = null;
            note.textContent = A.explain(e, { stale: "실행 상태가 바뀌었습니다. 새 상태를 확인하세요." });
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
