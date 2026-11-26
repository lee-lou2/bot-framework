// 승인·보류 탭. 승인 대기 카드(승인/거절), 최근 처리한 승인(접힘), 보류 목록과 판정 폼(운영자 전용).
// 위험한 버튼은 대상·인자 해시·결과를 화면에서 확인할 수 있게 하고, 응답이 올 때까지 다시 누를 수 없다.
// 보류 판정은 되돌릴 수 없어 두 번 눌러야 기록된다(인라인 확인, 대화상자 없음).
(function (root) {
  "use strict";
  const D = root.BotDom, A = root.BotApi, B = root.BotLabels, P = root.BotHoldPin, { h, icon, code } = D;
  const PAGE = 20, ARM_MS = 12000, ARM_GUARD_MS = 600;
  // 예약 전송 카드의 초안 행: 승인 지문이 이 글 전체의 SHA-256이라 한 글자도 고치지 않고 그대로(줄을 접어) 보여 준다.
  const DRAFT = "전송 초안";
  const LINE = /[\u0000-\u001f\u007f-\u009f]/;
  // 서버와 같은 규칙: 앞뒤 공백을 걷고 1~512자, 제어 문자 없음.
  const okText = (t) => t.length > 0 && Array.from(t).length <= 512 && !LINE.test(t);
  const flat = (rows) => [].concat.apply([], rows);

  function create(app, box) {
    const st = { pending: [], next: null, recent: [], holds: null, holdsErr: null, loaded: false, epoch: 0 };
    const ui = { busy: {}, notes: {}, forms: {} };
    let apMemo = {}, holdMemo = {}, recentMemo = {};

    const apCount = h("em"), holdCount = h("em"), recentCount = h("em");
    const apBox = h("div", { class: "cards", "aria-label": "승인 대기 목록" }), apNone = h("div");
    const moreBtn = h("button", { type: "button", class: "btn", text: "더 보기", onclick: () => more() });
    const moreBox = h("div", { class: "more" }, [moreBtn]);
    const holdBox = h("div", { class: "cards", "aria-label": "보류 목록" }), holdNone = h("div");
    const recentBody = h("div", { class: "recent-body" });
    const recent = h("details", { class: "recent" }, [h("summary", null, [h("span", { text: "최근 처리한 승인" }), recentCount]), recentBody]);
    const holdTitle = h("h3", { class: "sec-t", id: "hold-title", tabindex: "-1" }, [h("span", { text: "보류" }), holdCount]);
    const apTitle = h("h3", { class: "sec-t", id: "ap-title", tabindex: "-1" }, [h("span", { text: "승인 대기" }), apCount]);
    const holdSec = h("section", { class: "sec", "aria-labelledby": "hold-title" }, [holdTitle, h("p", { class: "sec-h", text: "외부 쓰기의 결과를 알 수 없어 멈춘 호출입니다. 수신처에서 확인한 뒤 판정을 기록합니다. 처리됨이면 영수증을 남기고, 미실행이면 같은 호출을 다시 이어 갑니다(민감 DM은 종료하고 새 요청이 필요합니다)." }), holdNone, holdBox]);
    const sub = h("p", { class: "sub" });
    const stack = h("div", { class: "stack" }, [
      h("section", { class: "sec", "aria-labelledby": "ap-title" }, [apTitle, apNone, apBox, moreBox]),
      holdSec, h("section", { class: "sec" }, [recent]),
    ]);
    box.appendChild(h("div", { class: "wrap" }, [h("header", { class: "ph" }, [h("div", null, [h("h2", { text: "승인·보류" }), sub])]), stack]));
    box.classList.add("decide");

    const opOnly = () => app.me().operator;
    // 처리한 카드가 사라지면서 포커스를 잃었을 때만, 다음 처리할 단추(없으면 구역 제목)로 포커스를 옮긴다.
    function settleFocus(title, scope) {
      const a = document.activeElement;
      if (a && a !== document.body) return;
      const next = scope.querySelector(".btn.primary:not(:disabled), .rform .btn:not(:disabled)");
      (next || title).focus();
    }

    // ---------- 불러오기 ----------
    function load(ctx) {
      const epoch = st.epoch, limit = D.clamp(st.pending.length || PAGE, PAGE, 200);
      const quiet = (p) => p.catch((e) => { if (e.name === "AbortError") throw e; return { error: e }; });
      const jobs = [A.get("/v1/admin/approvals" + A.q({ profile: ctx.profile, state: "pending", limit }), { signal: ctx.signal }), A.get("/v1/admin/approvals" + A.q({ profile: ctx.profile, state: "decided", limit: 20 }), { signal: ctx.signal })];
      if (opOnly()) jobs.push(quiet(A.get("/v1/admin/holds" + A.q({ profile: ctx.profile }), { signal: ctx.signal })));
      return Promise.all(jobs).then((r) => {
        if (!ctx.current() || epoch !== st.epoch) return;
        st.pending = r[0].items; st.next = r[0].next;
        st.recent = r[1].items; // 서버가 처리됨만, 최근 처리순으로 준다
        if (r[2]) { if (r[2].error) st.holdsErr = r[2].error; else { st.holds = r[2].items; st.holdsErr = null; } }
        st.loaded = true;
        app.noteProfiles(st.pending.concat(st.recent).map((a) => a.profile).concat((st.holds || []).map((x) => x.profile)));
        render();
      });
    }
    function more() {
      if (!st.next) return;
      const epoch = st.epoch;
      moreBtn.disabled = true;
      A.get("/v1/admin/approvals" + A.q({ profile: app.profile(), state: "pending", limit: PAGE, after: st.next })).then((d) => {
        if (epoch !== st.epoch) return;
        const have = {};
        st.pending.forEach((a) => { have[a.nonce] = true; });
        st.pending = st.pending.concat(d.items.filter((a) => !have[a.nonce]));
        st.next = d.next; moreBtn.disabled = false; render();
      }).catch((e) => { moreBtn.disabled = false; app.notice(A.explain(e)); });
    }

    // ---------- 승인 카드 ----------
    function runLink(profile, run) { return h("button", { type: "button", class: "linkbtn", title: "실행 상세 보기", onclick: () => app.go("runs", { profile, run }) }, [code(run)]); }
    function approvalCard(a, multi) {
      const mine = a.approver === app.me().principal, busy = ui.busy[a.nonce], note = ui.notes[a.nonce], over = a.expires_ms <= D.now();
      const why = !mine ? "지정된 승인자(" + a.approver + ")만 누를 수 있습니다." : over ? "만료됐습니다. 새 승인이 필요합니다." : "";
      // 볼 수 없는 본문은 승인할 수 없다(서버도 같은 조건으로 막는다). 거절은 할 수 있다.
      const bodyWhy = !why && a.approve_blocked ? "카드에서 " + (a.tool === "schedule.deliver" ? "전송 초안 전체" : "기억 ID와 본문 전체") + "를 확인할 수 없어 승인할 수 없습니다. 거절은 할 수 있습니다." : "", shown = why || bodyWhy;
      const off = !!busy || !!why, whyId = "ap-why-" + a.nonce, tid = "ap-t-" + a.nonce;
      const btn = (allow, cls, ic, text, busyText) => h("button", { type: "button", class: "btn " + cls, disabled: off || (allow && !!bodyWhy), "aria-describedby": shown ? whyId : null, onclick: () => decide(a, allow) }, [icon(ic, { size: 14 }), h("span", { text: busy === (allow ? "allow" : "deny") ? busyText : text })]);
      return h("article", { class: "acard", "aria-labelledby": tid }, [
        h("header", { class: "ac-h" }, [h("h4", { class: "ac-title", id: tid }, [code(a.tool)]), B.riskPill(a.risk), D.untilEl(a.expires_ms, { cls: "ac-left" })]),
        h("p", { class: "ac-who" }, ["요청 ", code(a.requester), " · 승인자 ", code(a.approver), mine ? h("span", { class: "me", text: " (나)" }) : null]),
        a.render.length ? h("dl", { class: "facts ac-render" }, flat(a.render.map((l) => [h("dt", { text: l.label }), h("dd", null, [l.label === DRAFT ? h("pre", { class: "ac-draft", tabindex: "0", "aria-label": "전송 초안 원문", text: l.value }) : l.value])]))) : h("p", { class: "sec-h", text: a.render_hidden ? "카드 본문은 요청한 본인에게만 보입니다. 요청자·승인자·인자 해시는 그대로 볼 수 있습니다." : "화면에 보일 대상·변경 내용이 없습니다." }),
        h("p", { class: "ac-meta" }, [multi ? "프로필 " + a.profile + " · " : null, "인자 해시 ", B.hash8(a.args_sha256), " · 실행 ", runLink(a.profile, a.run), " · 호출 ", code(a.call), " · 정책 판본 ", code(a.policy_revision), " · 도구 판본 ", code(a.tool_revision)]),
        h("div", { class: "ac-act" }, [btn(true, "primary", "check", "승인", "승인하는 중…"), btn(false, "danger", "close", "거절", "거절하는 중…"), shown ? h("span", { class: "ac-why", id: whyId, text: shown }) : null]),
        note ? h("p", { class: "ac-note is-" + note.tone, text: note.text }) : null,
      ]);
    }
    function decide(a, allow) {
      if (ui.busy[a.nonce]) return;
      ui.busy[a.nonce] = allow ? "allow" : "deny"; delete ui.notes[a.nonce];
      renderApprovals();
      // 화면이 본 인자 해시(seen)를 실어 보낸다: 그 사이 내용이 바뀌었으면 서버가 거절한다.
      A.post("/v1/admin/approvals/" + A.seg(a.nonce) + "/decide", { allow, seen: a.args_sha256 }).then(() => {
        delete ui.busy[a.nonce];
        app.toast((allow ? "승인했습니다" : "거절했습니다") + " · " + a.tool, "ok");
        app.refresh(true).then(() => settleFocus(apTitle, apBox));
      }, (e) => {
        delete ui.busy[a.nonce];
        const text = A.explain(e, A.DECIDE);
        ui.notes[a.nonce] = { text, tone: "bad" };
        app.toast(text, "bad");
        renderApprovals();
        if (e.word === "stale" || e.word === "changed" || e.word === "not_found") app.refresh(true);
      });
    }
    function renderApprovals() {
      const multi = !app.profile(), me = app.me().principal;
      D.sync(apBox, st.pending.map((a) => ({ key: a.nonce, sig: JSON.stringify([a, multi, me, ui.busy[a.nonce] || "", ui.notes[a.nonce] || null, a.expires_ms <= D.now()]), build: () => ({ node: approvalCard(a, multi) }) })), apMemo, { stagger: true });
      apCount.textContent = st.loaded ? String(st.pending.length) + (st.next ? "+" : "") : "";
      D.clear(apNone);
      if (!st.loaded) apNone.appendChild(D.skeleton(3));
      else if (!st.pending.length) apNone.appendChild(D.empty("승인을 기다리는 요청이 없습니다", "승인이 필요한 도구 호출이 생기면 여기에 카드로 보입니다.", "check"));
      moreBox.hidden = !st.next;
    }

    // ---------- 최근 처리한 승인 ----------
    function renderRecent() {
      // 때: 처리한 시각. 처리 없이 끝난 승인은 만료 시각만 의미가 있다(철회·무효화는 시각을 알 수 없다).
      const rows = st.recent.map((a) => [a.decided_ms || a.state === "expired" ? D.timeEl(a.decided_ms || a.expires_ms) : "-", code(a.tool), B.approvalPill(a.state), a.decided_by ? code(a.decided_by) : "-", code(a.requester), B.hash8(a.args_sha256)]);
      D.patch(recentBody, [{ key: "recent", sig: JSON.stringify([st.recent, st.loaded]), build: () => ({ node: D.table(["때", "도구", "결과", "처리한 사람", "요청자", "인자 해시"], rows, { narrow: [2, 5], label: "최근 처리한 승인 표", emptyTitle: "처리한 승인이 아직 없습니다", emptyIcon: "inbox" }) }) }], recentMemo, {});
      recentCount.textContent = st.recent.length ? String(st.recent.length) : "";
    }

    // ---------- 보류 카드와 판정 폼 ----------
    const FORMS = {
      confirmed: { title: "영수증 확인", label: "영수증", ph: "예: msg-9", field: "receipt", need: "영수증을 한 줄(1~512자)로 적어 주세요." },
      not_executed: { title: "미실행 확인", label: "근거", ph: "예: 수신처 조회에서 해당 요청 없음", field: "evidence", need: "근거를 한 줄(1~512자)로 적어 주세요." },
    };
    const FIELDS = { confirmed: FORMS.confirmed.field, not_executed: FORMS.not_executed.field };
    // «미실행»의 결과는 보류의 종류마다 다르다. 도구 호출은 옛 도구 종료와 수신처의 미실행 증거가 둘 다 확인돼야 같은 호출로 다시 이어 가고,
    // 회신은 같은 회신을 다시 보내며, 민감 DM은 본문을 보관하지 않아 다시 실행하지 못하고 종료한다(새 요청이 필요하다).
    const KIND = {
      final: {
        confirmed: { help: "기록된 단계의 실행은 확인됐습니다. 옛 도구가 끝났고 수신처에서 호출 전체의 처리를 확인했을 때 전체 영수증을 적으세요.", btn: "호출 결과 확인", then: "호출 전체의 결과로 확정합니다." },
      },
      tool: {
        confirmed: { help: "수신처에서 실제로 처리된 것을 확인했다면 영수증 번호를 적습니다.", btn: "처리됨으로 기록", then: "확정합니다." },
        not_executed: { help: "옛 도구가 완전히 끝났고 수신처에 처리 기록이 없음을 둘 다 확인했을 때만 근거를 적습니다. 그러면 같은 호출 ID로 다시 이어 갑니다.", btn: "미실행으로 기록", then: "실행되지 않은 것으로 기록하고 같은 호출을 다시 이어 갑니다(옛 도구 종료와 수신처 미실행 증거 확인 필요)." },
      },
      reply: {
        confirmed: { help: "수신처에 회신이 간 것을 확인했다면 영수증 번호를 적습니다.", btn: "처리됨으로 기록", then: "확정합니다." },
        not_executed: { help: "수신처에 회신이 가지 않은 것을 확인했다면 근거를 적습니다. 같은 회신을 처음부터 다시 보냅니다.", btn: "미실행으로 기록", then: "실행되지 않은 것으로 기록하고 같은 회신을 다시 보냅니다." },
      },
      private: {
        confirmed: { help: "수신처에서 처리된 것을 확인했다면 영수증 번호를 적습니다. 요청은 완료로 닫힙니다.", btn: "처리됨으로 기록", then: "확정하고 요청을 완료로 닫습니다." },
        not_executed: { help: "민감 DM은 본문을 보관하지 않아 다시 실행할 수 없습니다. 미실행이면 요청을 종료하고, 사용자가 새로 요청해야 합니다.", btn: "미실행으로 종료 · 새 요청 필요", then: "미실행으로 종료합니다. 다시 실행되지 않으니 사용자가 새로 요청해야 합니다." },
      },
    };
    const kindOf = (x) => P.receiptOnly(x) ? "final" : (KIND[x.kind] ? x.kind : "tool");
    const KIND_PILL = { final: "호출 최종 결과", reply: "회신", private: "민감 DM" };
    // 같은 프로필·run·호출 ID 아래 도구 보류와 회신 보류가 함께 있을 수 있다(구판 데이터): 카드와 폼 상태의 키에 보류 종류까지 넣는다.
    const holdKey = P.keyOf;
    function formState(x) { return ui.forms[holdKey(x)] || (ui.forms[holdKey(x)] = { arm: null, busy: false, err: "", errKind: "", text: { confirmed: "", not_executed: "" }, effect: null, holdId: null, timer: 0, pin: null }); }
    // 확인과 입력을 모두 거둔다(고정한 대상이 사라졌을 때). 왜 거뒀는지 해당 판정 폼에 알린다.
    function release(f, reason) {
      clearTimeout(f.timer);
      const was = f.pin ? f.pin.kind : "confirmed";
      f.arm = null; f.pin = null; f.text = { confirmed: "", not_executed: "" };
      if (reason) { f.err = P.WHY[reason] || P.WHY.gone; f.errKind = was; }
    }
    function stepsTable(x) {
      const notExecuted = P.receiptOnly(x) ? "미실행으로 판정됨" : "미실행으로 판정됨 · 재개 대기";
      if (!x.steps.length) return h("p", { class: "sec-h", text: "호출 하나가 통째로 보류됐습니다(단계 기록 없음)." });
      return D.table(["단계", "상태", "영수증", "사유"], x.steps.map((s) => ({ cls: P.open(s) ? "is-held" : s.receipt ? "is-kept" : "", cells: [s.step, B.effectPill(s.state), s.receipt ? h("span", { class: "kept" }, [code(s.receipt), h("span", { class: "kept-t", text: "보존됨" })]) : "-", s.resolution === "not_executed" ? notExecuted : s.state === "held" ? B.holdReason(s.reason) : "-"] })), { narrow: [1], label: "보류된 호출의 단계 표" });
    }
    function holdCard(x) {
      const f = formState(x), held = x.steps.filter(P.open), tid = "hold-t-" + holdKey(x).replace(/[^A-Za-z0-9_-]/g, "_"), kind = kindOf(x);
      // 첫 확인에 고정한 단계가 그 사이 다른 곳에서 처리됐으면 확인과 입력을 모두 해제한다(같은 영수증이 다른 단계에 기록되면 안 된다).
      const check = P.reconcile(f.pin, x);
      if (!check.ok) release(f, check.reason);
      // 고른 단계가 더 보류가 아니면 남은 첫 단계로 옮기고, 이전 단계용 입력은 지운다(다른 단계에 기록되면 안 된다).
      const sel = P.retarget(f.effect, f.holdId, x);
      if (sel.changed) f.text = { confirmed: "", not_executed: "" };
      f.effect = sel.effect; f.holdId = sel.holdId;
      const parts = {}, verdicts = Object.keys(KIND[kind]);
      // 판정할 보류 식별자가 없는 대상: 판정 단추를 끄고 목록을 다시 불러오게 한다(서버도 식별자 없는 판정을 받지 않는다).
      const nid = "hn-" + tid;
      const noid = h("div", { class: "rf-noid", id: nid, role: "status", hidden: true }, [h("p", { class: "warnbox", text: P.WHY.noid }), h("button", { type: "button", class: "btn", onclick: () => app.refresh(true) }, [icon("arrow", { size: 14 }), h("span", { text: "다시 불러오기" })])]);
      const at = () => (f.pin ? f.pin.effect : f.effect);
      const target = () => (at() ? x.tool + " · " + (held.filter((s) => s.effect === at())[0] || {}).step + " (" + at() + ")" : x.tool + " (호출 전체)");
      const said = () => (f.pin ? f.pin.text : f.text[f.arm]).trim();
      const summary = (k) => "되돌릴 수 없습니다. " + target() + "을(를) " + (k === "confirmed" ? "영수증 «" + said() + "»(으)로 " : "") + KIND[kind][k].then + (k === "not_executed" ? " 근거: " + said() : "");
      function sync() {
        const blocked = f.holdId === null || f.holdId === undefined;
        noid.hidden = !blocked;
        verdicts.forEach((k) => {
          const p = parts[k], armed = f.arm === k;
          p.input.readOnly = armed || f.busy;
          p.btn.disabled = f.busy || blocked || (!!f.arm && !armed);
          if (blocked) p.btn.setAttribute("aria-describedby", nid); else p.btn.removeAttribute("aria-describedby");
          p.btn.classList.toggle("is-armed", armed);
          p.btnText.textContent = f.busy && armed ? "기록하는 중…" : armed ? "한 번 더 눌러 기록" : KIND[kind][k].btn;
          p.cancel.hidden = !armed || f.busy;
          p.sum.hidden = !armed;
          if (armed) p.sum.textContent = summary(k);
          const err = f.err && f.errKind === k;
          p.err.hidden = !err; p.err.textContent = err ? f.err : "";
        });
        if (parts.select) parts.select.disabled = !!f.arm || f.busy;
      }
      // 카드가 다시 만들어져도(자료가 바뀌어도) 상태·타이머·응답은 가장 새 카드의 sync로 이어진다.
      f.sync = sync;
      function disarm() { clearTimeout(f.timer); f.arm = null; f.pin = null; f.sync(); }
      // 입력한 영수증·근거를 모두 지운다(상태와 화면 입력칸 둘 다).
      function wipe() { f.text = { confirmed: "", not_executed: "" }; f.err = ""; Object.keys(FORMS).forEach((k) => { if (parts[k]) parts[k].input.value = ""; }); }
      function submit(k, ev) {
        ev.preventDefault();
        if (f.busy) return;
        if (f.holdId === null || f.holdId === undefined) { sync(); return; } // 식별자 없는 대상은 확인도 전송도 하지 않는다(Enter 키로도)
        const p = parts[k], t = p.input.value.trim();
        f.text[k] = p.input.value;
        if (f.arm !== k) {
          if (!okText(t)) { f.err = FORMS[k].need; f.errKind = k; sync(); p.input.focus(); return; }
          f.err = ""; f.arm = k; f.pin = P.arm(k, f.effect, p.input.value, f.holdId); f.armedAt = Date.now(); clearTimeout(f.timer); f.timer = setTimeout(() => { if (f.arm && !f.busy) { f.arm = null; f.pin = null; f.sync(); } }, ARM_MS);
          sync(); p.btn.focus();
          return;
        }
        // 두 번 눌러야 기록된다: 첫 번째 누름 직후(더블클릭·키 반복)에 들어온 두 번째 누름은 무시한다.
        if (Date.now() - (f.armedAt || 0) < ARM_GUARD_MS) return;
        // 두 번째 누름은 첫 확인에 고정한 대상·판정·입력값만 보낸다(지금 고른 단계나 입력칸이 아니다).
        const body = P.payload(f.pin, FIELDS);
        if (!body || f.pin.kind !== k) { disarm(); return; }
        f.busy = true; f.err = ""; clearTimeout(f.timer); sync();
        A.post("/v1/admin/holds/" + A.seg(x.profile) + "/" + A.seg(x.run) + "/" + A.seg(x.call) + "/resolve", body).then((res) => {
          delete ui.forms[holdKey(x)];
          // 같은 보류 식별자·같은 판정을 다시 보낸 것이면 서버가 새로 적용하지 않고 기존 결과를 돌려준다(repeated).
          app.toast((res && res.repeated ? "이미 기록된 같은 판정입니다 · " : "판정을 기록했습니다 · ") + x.tool, "ok");
          app.refresh(true).then(() => settleFocus(holdTitle, holdBox));
        }, (e) => {
          f.busy = false; f.arm = null; f.pin = null;
          if (P.staleWord(e.word)) wipe(); // 이미 판정됐거나 없는 단계로 간 요청의 입력은 남은 다른 단계에 쓰지 않는다
          f.err = A.explain(e, A.RESOLVE); f.errKind = k; f.sync();
          app.toast(f.err, "bad");
          if (e.word === "conflict" || e.word === "not_found") app.refresh(true);
        });
      }
      const formNode = (k) => {
        const spec = FORMS[k], fid = "hf-" + k + "-" + tid;
        const input = h("input", { type: "text", id: fid, class: "in", maxlength: "512", autocomplete: "off", spellcheck: "false", placeholder: spec.ph, "aria-describedby": fid + "-h", oninput: (e) => { f.text[k] = e.target.value; if (f.err && f.errKind === k) { f.err = ""; sync(); } } });
        input.value = f.text[k];
        const btnText = h("span", { text: KIND[kind][k].btn }), btn = h("button", { type: "submit", class: "btn danger" }, [icon("check", { size: 14 }), btnText]);
        const cancel = h("button", { type: "button", class: "btn quiet", text: "취소", hidden: true, onclick: disarm });
        const sum = h("p", { class: "rf-sum", role: "status", hidden: true }), err = h("p", { class: "rf-err", role: "alert", hidden: true });
        parts[k] = { input, btn, btnText, cancel, sum, err };
        // 확인 단계에서 Esc를 누르면 취소한다.
        return h("form", { class: "rform", "aria-labelledby": fid + "-t", onsubmit: (ev) => submit(k, ev), onkeydown: (ev) => { if (ev.key === "Escape" && f.arm && !f.busy) { ev.stopPropagation(); disarm(); } } }, [h("h5", { id: fid + "-t", text: spec.title }), h("p", { class: "rf-help", id: fid + "-h", text: KIND[kind][k].help }), h("label", { class: "rf-field", for: fid }, [h("span", { text: spec.label }), input]), sum, err, h("div", { class: "rf-act" }, [btn, cancel])]);
      };
      let pick = null;
      if (held.length > 1) {
        pick = h("label", { class: "rf-pick" }, [h("span", { text: "판정할 단계" })]);
        parts.select = h("select", { class: "in", onchange: (e) => { f.effect = e.target.value; f.holdId = P.holdIdFor(x, f.effect); wipe(); disarm(); } }, held.map((s) => h("option", { value: s.effect, text: s.step + " (" + s.effect + ")" })));
        parts.select.value = f.effect;
        pick.appendChild(parts.select);
      }
      const node = h("article", { class: "hcard", "aria-labelledby": tid }, [
        h("header", { class: "ac-h" }, [h("h4", { class: "ac-title", id: tid }, [code(x.tool)]), KIND_PILL[kind] ? D.pill(KIND_PILL[kind], "off", null) : null, D.pill(B.holdReason(x.reason), "warn", "held"), h("span", { class: "ac-left" }, [D.timeEl(x.since_ms), "부터 멈춤"])]),
        h("p", { class: "ac-meta" }, [app.profile() ? null : "프로필 " + x.profile + " · ", "실행 ", runLink(x.profile, x.run), " · 호출 ", code(x.call), " · 보류 시각 ", D.when(x.since_ms)]),
        stepsTable(x), pick, noid,
        h("div", { class: "hc-forms" }, verdicts.map(formNode)),
      ]);
      sync();
      return node;
    }
    function renderHolds() {
      holdSec.hidden = !opOnly();
      if (!opOnly()) return;
      const items = st.holds || [];
      D.sync(holdBox, items.map((x) => ({ key: holdKey(x), sig: JSON.stringify(x), build: () => ({ node: holdCard(x) }) })), holdMemo, { stagger: true });
      Object.keys(ui.forms).forEach((k) => { if (!items.some((x) => holdKey(x) === k)) delete ui.forms[k]; });
      holdCount.textContent = st.holds ? String(items.length) : "";
      D.clear(holdNone);
      if (st.holdsErr) holdNone.appendChild(h("p", { class: "warnbox", text: "보류 목록을 읽지 못했습니다. 없다는 뜻이 아닙니다." }));
      else if (!st.holds) holdNone.appendChild(D.skeleton(3));
      else if (!items.length) holdNone.appendChild(D.empty("보류된 호출이 없습니다", "외부 쓰기가 결과 불명으로 멈춘 일이 없습니다.", "check"));
    }
    function render() {
      sub.textContent = opOnly() ? "승인 대기 요청과 보류된 호출을 한곳에서 처리합니다." : "내가 요청했거나 내게 지정된 승인만 보입니다.";
      renderApprovals(); renderHolds(); renderRecent();
    }
    render();

    return {
      load,
      reset() { st.epoch++; st.pending = []; st.next = null; st.recent = []; st.holds = null; st.holdsErr = null; st.loaded = false; apMemo = {}; holdMemo = {}; recentMemo = {}; D.clear(apBox); D.clear(holdBox); D.clear(recentBody); ui.notes = {}; render(); },
      // 확인 중(보류 폼이 무장했거나 전송 중)이거나 승인 요청을 보내는 중이면 자동 새로고침을 건너뛴다.
      busy() { return Object.keys(ui.busy).length > 0 || Object.keys(ui.forms).some((k) => ui.forms[k].arm || ui.forms[k].busy); },
      show() {}, hide() {},
    };
  }

  root.BotDecide = { create };
})(window);
