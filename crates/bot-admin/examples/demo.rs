//! Launches the admin UI with synthetic data for local UI development and screenshots.
//! Run `cargo run -p bot-admin --example demo` (optional `DEMO_PORT`); prints URL and synthetic passwords.

use bot_admin::{
    AdminConfig, AdminDeps, StaticTokens,
    auth::viewer,
    fake::{Fake, approval_view, demo_catalog, pid},
    router,
};
use bot_core::{
    cards::{LABEL_BODY, LABEL_MEMORY_ID, LABEL_RECIPIENT, LABEL_SHARE_SCOPE, LABEL_TITLE},
    command::memory_share_hash,
};
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
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let fake = Fake::demo();
    // Owner-only approval cards: operators cannot see another principal's body, and mismatched requester/approver cards can only be denied.
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
            .expect("call");
        let mut card = approval_view(&call, &run, "ap-mem", "local:guest");
        card.tool = bot_core::id::ToolId("memory.share".into());
        card.requester = pid("local:guest");
        let share = |id: &str, body: &str| {
            memory_share_hash(
                &bot_core::id::MemoryId(id.into()),
                &Sha256::digest(body.as_bytes()).into(),
            )
        };
        card.args_sha256 = share("mem-1", "Synthetic private memory body");
        card.render = vec![
            (LABEL_TITLE.into(), "Memory share approval".into()),
            (LABEL_MEMORY_ID.into(), "mem-1".into()),
            (LABEL_SHARE_SCOPE.into(), "Profile-wide".into()),
            (LABEL_BODY.into(), "Synthetic private memory body".into()),
        ];
        d.approvals.push(card);
        let mut blind = approval_view(&call, &run, "ap-mem2", "local:owner");
        blind.tool = bot_core::id::ToolId("memory.share".into());
        blind.requester = pid("local:guest");
        blind.args_sha256 = share("mem-2", "Second synthetic private memory body");
        blind.render = vec![
            (LABEL_TITLE.into(), "Memory share approval".into()),
            (LABEL_MEMORY_ID.into(), "mem-2".into()),
            (
                LABEL_BODY.into(),
                "Second synthetic private memory body".into(),
            ),
        ];
        d.approvals.push(blind);
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
                "Synthetic weekly summary: 3 new events, 1 delayed",
            ),
            ("ap-dlv2", "local:guest", "Synthetic private draft"),
        ] {
            let mut card = approval_view(&call, &run, nonce, "local:owner");
            card.tool = bot_core::id::ToolId(bot_core::schedule::DELIVER_TOOL.into());
            card.tool_revision =
                bot_core::id::Revision(bot_core::schedule::DELIVER_REVISION.into());
            card.risk = bot_core::state::Risk::Irreversible;
            card.requester = pid(requester);
            card.args_sha256 = draft(text).sha256().expect("digest");
            card.render = vec![
                (LABEL_TITLE.into(), "Scheduled delivery approval".into()),
                (LABEL_RECIPIENT.into(), "team · demo-channel".into()),
                (
                    bot_core::schedule::DELIVERY_CARD_LABEL.into(),
                    draft(text).approval_json().expect("approval json"),
                ),
            ];
            d.approvals.push(card);
        }
    });
    // Add a second held step to exercise multi-step hold selection and pinning in the UI.
    fake.edit(|d| {
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-9")
            .cloned()
            .expect("call");
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
    fake.edit(|d| {
        use bot_admin::fake::run_view;
        use bot_core::{
            admin::{HoldKind, HoldView, Waiting},
            id::{CallId, ProfileId, RunId, ToolId},
        };
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
    // `DEMO_PAIR=1` adds paired tool/reply holds under one call_id plus a legacy hold without `hold_id` for `tests/browser/hold_pair.mjs`.
    if std::env::var_os("DEMO_PAIR").is_some() {
        fake.edit(|d| {
            use bot_admin::fake::{call_view, run_view};
            use bot_core::{
                admin::{HoldKind, HoldView, Waiting},
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
    let app = router(AdminConfig::new(addr), deps).expect("router");
    println!("Admin UI: http://{addr}/ui");
    println!("Operator password: {OPERATOR} (any username)");
    println!("Member password: {GUEST}");
    bot_admin::serve(listener, app, std::future::pending())
        .await
        .expect("server");
}
