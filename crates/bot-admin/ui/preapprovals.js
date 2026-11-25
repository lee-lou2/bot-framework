// 사전 승인 탭(운영자 전용): 운영자가 미리 허가한 정적 트리거마다 현재 상태·오늘 사용량과 «긴급 정지»·«재개» 버튼.
// 정지는 한 번 누르면 곧바로 요청하고, 재개(사람의 카드 없이 외부 쓰기를 다시 여는 쪽)만 두 번 눌러야 한다. 누르는 순간 화면이 본 승인 ID·정지 세대를 그대로 보낸다.
// 응답을 받기 전에는 상태를 바꾸지 않고(서버가 준 행으로만 바꾼다), 한 행에는 요청을 동시에 하나만 보낸다. 409·응답 유실·시간 초과 뒤에는 목록을 다시 읽는다.
// 글자는 모두 textContent로만 넣는다(HTML 문자열 삽입 없음). 규칙은 `preapproval.js`(순수 함수)에 있다.
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, R = root.BotPreapproval, { h, icon, code } = D;
  const ARM_MS = 12000, ARM_GUARD_MS = 600, SEND_MS = 15000;
  const HELP = {
    pause: "정지하면 새 발생과 새 외부 쓰기 허가가 바로 멈춥니다. 이미 시작된 쓰기의 결과 기록과 보류 복구는 계속됩니다. 정지 중 발생은 건너뛰고, 재개한 뒤의 새 발생부터 받습니다.",
    resume: "정지돼 있습니다. 재개하면 다음 발생부터 사람의 카드 없이 외부 쓰기를 다시 실행합니다.",
  };

  function create(app, box) {
    const st = { rows: null, skipped: 0, err: null, epoch: 0 };
    const ui = { busy: {}, notes: {}, arm: {}, btn: {}, ids: {}, seq: 0 };
    let memo = {};

    const count = h("em"), sub = h("p", { class: "sub" }), warn = h("div"), none = h("div");
    const cards = h("div", { class: "cards pcards", "aria-label": "사전 승인 목록" });
    const title = h("h3", { class: "sec-t", id: "pa-title", tabindex: "-1" }, [h("span", { text: "사전 승인한 트리거" }), count]);
    box.appendChild(h("div", { class: "wrap" }, [
      h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "사전 승인" }), sub])]),
      h("div", { class: "stack" }, [h("section", { class: "sec", "aria-labelledby": "pa-title" }, [title, h("p", { class: "sec-h", text: "운영자가 미리 허가한 정적 트리거입니다. 사람의 카드 없이, 정해진 대상에만, 하루 상한 안에서 외부 쓰기를 실행합니다. 이상하면 트리거 단위로 바로 멈출 수 있습니다." }), warn, none, cards])]),
    ]));
    box.classList.add("preapprovals");

    const idOf = (k) => ui.ids[k] || (ui.ids[k] = "pa-" + ++ui.seq);
    const armed = () => Object.keys(ui.arm);

    function disarm(k) {
      const a = ui.arm[k];
      if (a) clearTimeout(a.timer);
      delete ui.arm[k];
    }
    // 응답·목록 갱신으로 카드가 다시 만들어져 포커스를 잃었으면 그 행의 단추(없으면 구역 제목)로 돌려준다.
    function refocus(k) {
      const now = document.activeElement;
      if (now && now !== document.body) return;
      const b = ui.btn[k];
      (b && box.contains(b) ? b : title).focus();
    }

    // ---------- 카드 ----------
    function card(r, multi) {
      const k = R.key(r), id = idOf(k), u = R.usage(r), busy = ui.busy[k], pending = ui.arm[k], note = ui.notes[k];
      const tid = id + "-t", hid = id + "-h", sid = id + "-s";
      let main;
      if (!r.paused) {
        main = h("button", { type: "button", class: "btn stop", disabled: !!busy, "aria-describedby": hid, onclick: () => send(R.arm(r, "pause")) }, [icon("pause", { size: 14 }), h("span", { text: busy === "pause" ? "정지하는 중…" : "긴급 정지" })]);
      } else {
        const label = busy === "resume" ? "재개하는 중…" : pending ? "한 번 더 눌러 재개" : "재개";
        main = h("button", { type: "button", class: "btn danger" + (pending ? " is-armed" : ""), disabled: !!busy, "aria-describedby": pending ? sid : hid, onclick: () => (pending ? confirmResume(k) : armResume(r)) }, [icon("play", { size: 14 }), h("span", { text: label })]);
      }
      ui.btn[k] = main;
      const cancel = pending && !busy ? h("button", { type: "button", class: "btn quiet", text: "취소", onclick: () => { disarm(k); render(); refocus(k); } }) : null;
      const fill = h("i");
      fill.style.setProperty("--pct", String(u.pct));
      const sum = pending ? h("p", { class: "pc-sum", id: sid, role: "status", text: "재개하면 " + r.trigger + "의 다음 발생부터 사람의 카드 없이 외부 쓰기를 다시 실행합니다(오늘 남은 허가 " + u.left + "회, 하루 " + u.max + "회). 정지 동안 건너뛴 발생은 다시 실행되지 않습니다. 한 번 더 누르면 재개합니다." }) : null;
      return h("article", { class: "acard pcard" + (r.paused ? " is-paused" : ""), "aria-labelledby": tid, "data-pkey": id }, [
        h("header", { class: "ac-h" }, [
          h("h4", { class: "ac-title", id: tid }, [code(r.trigger)]),
          r.paused ? D.pill("정지됨", "warn", "held") : D.pill("동작 중", "ok", "on"),
          u.full ? D.pill("오늘 상한 도달", "bad", "error") : null,
          h("span", { class: "ac-left", text: "오늘 " + u.used + " / " + u.max + "회" }),
        ]),
        h("p", { class: "ac-meta" }, [multi ? "프로필 " + r.profile + " · " : null, "에이전트 ", code(r.agent), " · 승인 ID ", code(r.grant_id.slice(0, 12), r.grant_id), " · 정지 세대 ", code(String(r.epoch))]),
        h("div", { class: "pc-use" }, [
          h("div", { class: "meter" + (u.full ? " is-full" : ""), role: "meter", "aria-label": "오늘 허가한 외부 쓰기 횟수", "aria-valuemin": "0", "aria-valuemax": String(u.max), "aria-valuenow": String(Math.min(u.used, u.max)), "aria-valuetext": u.used + " / " + u.max + "회" }, [fill]),
          h("p", { class: "pc-use-t", text: "오늘(UTC 기준) 새로 허가한 외부 쓰기 " + u.used + "회 · 남은 허가 " + u.left + "회" }),
        ]),
        h("p", { class: "pc-help", id: hid, text: HELP[R.offered(r)] }),
        sum,
        h("div", { class: "ac-act" }, [main, cancel]),
        note ? h("p", { class: "ac-note is-" + note.tone, role: "status", text: note.text }) : null,
      ]);
    }

    // ---------- 동작 ----------
    // 재개의 첫 누름: 지금 화면이 본 승인 ID·정지 세대를 고정하고 두 번째 누름을 기다린다(시간이 지나면 거둔다).
    function armResume(r) {
      const k = R.key(r), pin = R.arm(r, "resume");
      if (!pin || ui.busy[k]) return;
      disarm(k);
      delete ui.notes[k];
      const a = ui.arm[k] = { pin, at: Date.now(), timer: setTimeout(() => { if (ui.arm[k] === a) { delete ui.arm[k]; render(); } }, ARM_MS) };
      render();
      refocus(k);
    }
    // 재개의 두 번째 누름: 첫 누름 직후(더블클릭·키 반복)는 무시하고, 고정한 값만 보낸다.
    function confirmResume(k) {
      const a = ui.arm[k];
      if (!a || ui.busy[k] || Date.now() - a.at < ARM_GUARD_MS) return;
      send(a.pin);
    }
    // 요청 하나: 응답이 오기 전에는 상태를 바꾸지 않는다. 한 행에는 동시에 하나만 보낸다. 끝나면(성공이든 실패든) 목록을 다시 읽어 서버의 현재 상태를 보인다.
    function send(pin) {
      if (!pin) return;
      const k = pin.key;
      if (ui.busy[k]) return;
      const epoch = st.epoch, ctl = new AbortController(), timer = setTimeout(() => ctl.abort(), SEND_MS);
      disarm(k);
      delete ui.notes[k];
      ui.busy[k] = pin.action;
      render();
      A.post(R.path(pin), R.payload(pin), { signal: ctl.signal }).then((reply) => {
        clearTimeout(timer);
        delete ui.busy[k];
        const row = R.settle(pin, reply);
        if (row && epoch === st.epoch && st.rows) st.rows = R.replace(st.rows, row);
        const text = row ? (pin.action === "pause" ? "정지했습니다 · " : "재개했습니다 · ") + pin.trigger : R.ERR.mismatch;
        ui.notes[k] = { tone: row ? "ok" : "bad", text };
        app.toast(text, row ? "ok" : "bad");
        render();
        app.refresh(true).then(() => refocus(k));
      }, (e) => {
        clearTimeout(timer);
        delete ui.busy[k];
        const text = e && e.name === "AbortError" ? R.ERR.timeout : A.explain(e, R.ERR);
        ui.notes[k] = { tone: "bad", text };
        app.toast(text, "bad");
        render();
        app.refresh(true).then(() => refocus(k));
      });
    }

    // ---------- 그리기·불러오기 ----------
    function render() {
      const p = app.profile(), all = st.rows || [], rows = all.filter((r) => !p || r.profile === p), multi = !p, n = R.counts(rows);
      sub.textContent = st.rows && n.total ? "사전 승인 " + n.total + "개 · 정지 " + n.paused + "개" : "";
      D.sync(cards, rows.map((r) => { const k = R.key(r); return { key: k, sig: JSON.stringify([r, multi, ui.busy[k] || "", ui.notes[k] || null, !!ui.arm[k]]), build: () => ({ node: card(r, multi) }) }; }), memo, { stagger: true });
      count.textContent = st.rows ? String(n.total) : "";
      D.clear(warn);
      if (st.skipped) warn.appendChild(h("p", { class: "warnbox", text: "읽을 수 없는 행 " + st.skipped + "개는 버튼 없이 건너뛰었습니다. 서버와 화면의 판본이 다를 수 있으니 화면을 새로 여세요." }));
      if (st.err) warn.appendChild(h("p", { class: "warnbox", text: st.err }));
      D.clear(none);
      if (!st.rows) { if (!st.err) none.appendChild(D.skeleton(3)); }
      else if (!rows.length) none.appendChild(D.empty(all.length ? "이 프로필에는 사전 승인한 트리거가 없습니다" : "사전 승인한 트리거가 없습니다", "triggers.yaml에 preapproved를 선언하고 운영자가 명시적으로 활성화하면 여기에 보입니다.", "check"));
    }
    function load(ctx) {
      const epoch = st.epoch;
      return A.get("/v1/admin/preapprovals", { signal: ctx.signal }).then((d) => {
        if (!ctx.current() || epoch !== st.epoch) return;
        const got = R.parse(d);
        if (!got) { st.err = "목록 응답의 모양이 올바르지 않아 읽지 않았습니다. 아래 상태는 이전에 불러온 값일 수 있습니다."; render(); return; }
        st.rows = got.rows; st.skipped = got.skipped; st.err = null;
        app.noteProfiles(got.rows.map((r) => r.profile));
        // 재개 확인을 건 행이 아직 같은 대상·승인·세대·상태인가. 아니면 확인을 거두고 이유를 알린다.
        armed().forEach((k) => {
          const check = R.reconcile(ui.arm[k].pin, st.rows);
          if (check.ok) return;
          disarm(k);
          ui.notes[k] = { tone: "bad", text: R.WHY[check.reason] || R.WHY.changed };
        });
        render();
      }, (e) => {
        if (ctx.current() && epoch === st.epoch && !(e && e.name === "AbortError")) { st.err = "목록을 새로 읽지 못했습니다. 아래 상태는 이전에 불러온 값일 수 있습니다."; render(); }
        throw e;
      });
    }
    // 확인 중에 Esc를 누르면 거둔다.
    box.addEventListener("keydown", (ev) => {
      if (ev.key !== "Escape" || !armed().length) return;
      ev.stopPropagation();
      const keys = armed();
      keys.forEach(disarm);
      render();
      refocus(keys[0]);
    });
    render();

    return {
      load,
      reset() { st.epoch++; st.rows = null; st.skipped = 0; st.err = null; armed().forEach(disarm); ui.notes = {}; memo = {}; D.clear(cards); render(); },
      // 확인 중이거나 요청을 보내는 중이면 자동 새로고침을 건너뛴다(카드가 다시 만들어져 확인이 풀리지 않게).
      busy() { return Object.keys(ui.busy).length > 0 || armed().length > 0; },
      show() {},
      // 다른 탭으로 가면 걸어 둔 확인을 거둔다.
      hide() { if (armed().length) { armed().forEach(disarm); render(); } },
    };
  }

  root.BotPreapprovals = { create };
})(window);
