//! 합성 데이터로 관리 화면을 띄운다(화면 개발·스크린샷용). 실제 자격·데이터는 쓰지 않는다.
//! `cargo run -p bot-admin --example demo` — 주소와 합성 비밀이 표준 출력에 나온다. `DEMO_PORT`로 포트를 고른다.

use bot_admin::{
    AdminConfig, AdminDeps, StaticTokens,
    auth::viewer,
    fake::{Fake, approval_view, demo_catalog, pid},
    router,
};
use bot_core::command::memory_share_hash;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const OPERATOR: &str = "demo-operator-secret";
const GUEST: &str = "demo-guest-secret";

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("DEMO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let listener = bot_admin::bind(([127, 0, 0, 1], port).into(), false)
        .await
        .expect("바인드");
    let addr = listener.local_addr().expect("주소");
    let fake = Fake::demo();
    // 일반 주체(guest)가 자기 비공개 기억을 공유하려는 카드: 운영자에게는 본문이 보이지 않는다.
    fake.edit(|d| {
        let run = d
            .runs
            .iter()
            .find(|r| r.run.0 == "r-1")
            .cloned()
            .expect("run");
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-1")
            .cloned()
            .expect("호출");
        let mut card = approval_view(&call, &run, "ap-mem", "local:guest");
        card.tool = bot_core::id::ToolId("memory.share".into());
        card.requester = pid("local:guest");
        // 카드는 기억 ID와 본문 전체를 보이고, 승인 지문(args_sha256)이 그 둘과 일치한다.
        let share = |id: &str, body: &str| {
            memory_share_hash(
                &bot_core::id::MemoryId(id.into()),
                &Sha256::digest(body.as_bytes()).into(),
            )
        };
        card.args_sha256 = share("mem-1", "합성 비공개 기억 본문");
        card.render = vec![
            ("제목".into(), "기억 공유 승인".into()),
            ("기억 ID".into(), "mem-1".into()),
            ("공유 범위".into(), "프로필 전체".into()),
            ("본문".into(), "합성 비공개 기억 본문".into()),
        ];
        d.approvals.push(card);
        // 요청자와 지정 승인자가 다른 카드: 승인자(운영자)도 본문을 볼 수 없어 승인은 막히고 거절만 할 수 있다.
        let mut blind = approval_view(&call, &run, "ap-mem2", "local:owner");
        blind.tool = bot_core::id::ToolId("memory.share".into());
        blind.requester = pid("local:guest");
        blind.args_sha256 = share("mem-2", "합성 비공개 기억 본문 둘째");
        blind.render = vec![
            ("제목".into(), "기억 공유 승인".into()),
            ("기억 ID".into(), "mem-2".into()),
            ("본문".into(), "합성 비공개 기억 본문 둘째".into()),
        ];
        d.approvals.push(blind);
        // 예약 전송 카드: 예약을 만든 본인이 승인자이면 «전송 초안» 원문 전체를 보고 승인한다(실제 코어가 만든 문서·지문).
        // 요청자와 승인자가 다른 카드는 운영자에게도 초안이 보이지 않아 승인할 수 없다(거절만).
        let draft = |text: &str| bot_core::schedule::DeliveryDraft {
            activation: bot_core::id::Revision("demo-set".into()),
            target: bot_core::schedule::DeliveryTarget {
                binding: bot_core::id::BindingId("team".into()),
                conversation: bot_core::state::Conversation {
                    kind: bot_core::state::ConversationKind::Channel,
                    id: "demo-channel".into(),
                    thread: None,
                },
            },
            answers: vec![bot_core::command::JsonPayload(format!(
                r#"{{"text":"{text}"}}"#
            ))],
        };
        for (nonce, requester, text) in [
            (
                "ap-dlv",
                "local:owner",
                "합성 주간 요약: 새 일정 3건, 지연 1건",
            ),
            ("ap-dlv2", "local:guest", "합성 비공개 초안"),
        ] {
            let mut card = approval_view(&call, &run, nonce, "local:owner");
            card.tool = bot_core::id::ToolId(bot_core::schedule::DELIVER_TOOL.into());
            card.tool_revision =
                bot_core::id::Revision(bot_core::schedule::DELIVER_REVISION.into());
            card.risk = bot_core::state::Risk::Irreversible;
            card.requester = pid(requester);
            card.args_sha256 = draft(text).sha256().expect("지문");
            card.render = vec![
                ("제목".into(), "예약 전송 승인".into()),
                ("수신처".into(), "team · demo-channel".into()),
                (
                    bot_core::schedule::DELIVERY_CARD_LABEL.into(),
                    draft(text).approval_json().expect("승인 문서"),
                ),
            ];
            d.approvals.push(card);
        }
    });
    // 다단계 보류에 보류 단계 하나를 더한다(단계 선택·고정 확인을 화면에서 볼 수 있게).
    fake.edit(|d| {
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-9")
            .cloned()
            .expect("호출");
        d.effects.push(bot_admin::fake::effect_view(
            &call,
            "e-3",
            "archive",
            bot_core::state::EffectState::Held,
            None,
        ));
        if let Some(h) = d.holds.iter_mut().find(|h| h.call.0 == "c-9") {
            h.steps = d
                .effects
                .iter()
                .filter(|e| e.profile == h.profile && e.call.0 == "c-9")
                .map(|e| e.view.clone())
                .collect();
        }
    });
    // 민감 DM 보류와 회신 보류: «미실행» 판정의 결과가 종류마다 다르다(종료·새 요청 필요 / 같은 회신을 다시 보냄).
    fake.edit(|d| {
        use bot_admin::{
            fake::run_view,
            port::{HoldKind, HoldView, Waiting},
        };
        use bot_core::id::{CallId, ProfileId, RunId, ToolId};
        for (run, call, tool, kind) in [
            ("r-p1", "private:r-p1", "inbox.receive", HoldKind::Private),
            ("r-rp", "c-rp", "reply", HoldKind::Reply),
        ] {
            let mut r = run_view(
                "personal",
                run,
                "main",
                "local:owner",
                bot_core::state::RunState::Waiting,
            );
            r.waiting = Some(Waiting::Hold(CallId(call.into())));
            d.runs.push(r);
            d.holds.push(HoldView {
                kind,
                hold_id: Some(bot_core::id::HoldId(format!("h-{call}"))),
                profile: ProfileId("personal".into()),
                run: RunId(run.into()),
                call: CallId(call.into()),
                tool: ToolId(tool.into()),
                reason: "unknown_outcome".into(),
                since: std::time::SystemTime::now(),
                steps: Vec::new(),
            });
        }
    });
    // `DEMO_PAIR=1`: 구판 데이터처럼 같은 호출 ID 아래 도구 보류와 회신 보류가 함께 있고, 보류 식별자가 없는 보류도 하나 있다
    // (tests/browser/hold_pair.mjs가 쓴다). 기본 데모 데이터는 바꾸지 않는다.
    if std::env::var_os("DEMO_PAIR").is_some() {
        fake.edit(|d| {
            use bot_admin::{
                fake::{call_view, run_view},
                port::{HoldKind, HoldView, Waiting},
            };
            use bot_core::{
                id::{CallId, HoldId, ProfileId, RunId, ToolId},
                state::{Risk, RunState},
            };
            for (run, call, kinds) in [
                (
                    "r-pair",
                    "c-pair",
                    &[("h-pair-t", HoldKind::Tool), ("h-pair-r", HoldKind::Reply)][..],
                ),
                ("r-noid", "c-noid", &[("", HoldKind::Reply)][..]),
            ] {
                let mut r = run_view("personal", run, "main", "local:owner", RunState::Waiting);
                r.waiting = Some(Waiting::Hold(CallId(call.into())));
                d.calls.push(call_view(
                    &r,
                    call,
                    "report.publish",
                    Risk::Write,
                    bot_core::admin::CallState::Held,
                ));
                d.runs.push(r);
                for (id, kind) in kinds {
                    d.holds.push(HoldView {
                        kind: *kind,
                        hold_id: (!id.is_empty()).then(|| HoldId((*id).into())),
                        profile: ProfileId("personal".into()),
                        run: RunId(run.into()),
                        call: CallId(call.into()),
                        tool: ToolId(
                            if *kind == HoldKind::Reply {
                                "reply"
                            } else {
                                "report.publish"
                            }
                            .into(),
                        ),
                        reason: "unknown_outcome".into(),
                        since: std::time::SystemTime::now(),
                        steps: Vec::new(),
                    });
                }
            }
        });
    }
    let fake = Arc::new(fake);
    let auth = StaticTokens::new()
        .with(OPERATOR, viewer("local:owner", true))
        .with(GUEST, viewer("local:guest", false));
    let deps = AdminDeps {
        read: fake.clone(),
        act: fake.clone(),
        broker: fake,
        auth: Arc::new(auth),
        profiles: Arc::new(demo_catalog()),
    };
    let app = router(AdminConfig::new(addr), deps).expect("라우터");
    println!("관리 화면: http://{addr}/ui");
    println!("운영자 비밀번호: {OPERATOR} (사용자 이름은 아무거나)");
    println!("일반 주체 비밀번호: {GUEST}");
    bot_admin::serve(listener, app, std::future::pending())
        .await
        .expect("서버");
}
