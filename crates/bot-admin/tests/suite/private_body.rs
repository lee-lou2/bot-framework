//! Memory share card (`memory.share`): owner-only body visibility and exact digest approval binding.

use crate::support::*;
use bot_admin::fake::{Fake, approval_view, pid};
use bot_core::{
    cards::{LABEL_BODY, LABEL_MEMORY_ID, LABEL_SHARE_SCOPE, LABEL_TITLE},
    command::memory_share_hash,
    id::{MemoryId, ToolId},
};
use sha2::{Digest, Sha256};

const BODY: &str = "PRIVATE-MEMORY-BODY-synthetic-7q";
const MEM: &str = "mem-1";

fn share_hash(id: &str, body: &str) -> [u8; 32] {
    memory_share_hash(
        &MemoryId(id.into()),
        &Sha256::digest(body.as_bytes()).into(),
    )
}

fn seen() -> String {
    bot_admin::json::hex(&share_hash(MEM, BODY))
}

fn body_row(card: &serde_json::Value) -> Option<&str> {
    card["render"]
        .as_array()?
        .iter()
        .find(|r| r["label"] == LABEL_BODY)
        .and_then(|r| r["value"].as_str())
}

const PATH: &str = "/v1/admin/approvals/ap-mem/decide";

fn with_share_card(requester: &str, approver: &str) -> Fake {
    let fake = Fake::demo();
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
        let mut a = approval_view(&call, &run, "ap-mem", approver);
        a.tool = ToolId("memory.share".into());
        a.requester = pid(requester);
        a.args_sha256 = share_hash(MEM, BODY);
        a.render = vec![
            (LABEL_TITLE.into(), "Memory share approval".into()),
            (LABEL_MEMORY_ID.into(), MEM.into()),
            (LABEL_SHARE_SCOPE.into(), "Profile-wide".into()),
            (LABEL_BODY.into(), BODY.into()),
        ];
        d.approvals.push(a);
    });
    fake
}

fn card<'a>(list: &'a serde_json::Value, nonce: &str) -> &'a serde_json::Value {
    list["items"]
        .as_array()
        .and_then(|v| v.iter().find(|a| a["nonce"] == nonce))
        .unwrap_or_else(|| panic!("missing card {nonce}: {list}"))
}

#[tokio::test]
async fn the_operator_sees_the_cards_metadata_but_never_the_private_body() {
    let s = start(with_share_card("local:guest", "local:guest")).await;
    let r = get(&s, OPERATOR, "/v1/admin/approvals?state=all").await;
    assert_eq!(r.status, 200);
    assert!(!r.body.contains(BODY), "operator response leaked body");
    let a = card(&r.json(), "ap-mem").clone();
    assert_eq!(
        (
            a["tool"].as_str(),
            a["requester"].as_str(),
            a["approver"].as_str(),
            a["state"].as_str()
        ),
        (
            Some("memory.share"),
            Some("local:guest"),
            Some("local:guest"),
            Some("pending")
        )
    );
    assert_eq!(a["args_sha256"].as_str().map(str::len), Some(64));
    assert_eq!(
        (
            a["render"].as_array().map(Vec::len),
            a["render_hidden"].as_bool()
        ),
        (Some(0), Some(true))
    );
    let normal = card(&r.json(), "ap-1").clone();
    assert!(
        normal["render"].as_array().is_some_and(|v| !v.is_empty())
            && normal["render_hidden"] == false
    );
    let denied = post(
        &s,
        OPERATOR,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{}"}}"#, seen()),
    )
    .await;
    assert_eq!(denied.status, 403);
    assert!(!denied.body.contains(BODY));
}

#[tokio::test]
async fn the_owner_sees_the_exact_body_to_approve_and_the_decision_response_keeps_it() {
    let s = start(with_share_card("local:guest", "local:guest")).await;
    let list = get(&s, GUEST, "/v1/admin/approvals").await;
    let a = card(&list.json(), "ap-mem").clone();
    assert_eq!(a["render_hidden"], false);
    assert_eq!(body_row(&a), Some(BODY));
    let seen = a["args_sha256"].as_str().expect("digest").to_owned();
    let r = post(
        &s,
        GUEST,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["approval"]["state"], "approved");
    assert_eq!(body_row(&r.json()["approval"]), Some(BODY));
    let again = post(
        &s,
        GUEST,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    let op = get(&s, OPERATOR, "/v1/admin/approvals?state=all").await;
    assert!(!op.body.contains(BODY));
    assert_eq!(card(&op.json(), "ap-mem")["state"], "approved");
}

#[tokio::test]
async fn the_body_never_shows_up_in_runs_audit_status_or_the_map() {
    let s = start(with_share_card("local:guest", "local:guest")).await;
    let seen = seen();
    post(
        &s,
        GUEST,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    for path in [
        "/v1/admin/runs?limit=200",
        "/v1/admin/runs/personal/r-1",
        "/v1/admin/holds",
        "/v1/admin/audit?limit=200",
        "/v1/admin/status",
        "/v1/admin/map",
    ] {
        let r = get(&s, OPERATOR, path).await;
        assert_eq!(r.status, 200, "{path}");
        assert!(!r.body.contains(BODY), "body leaked on {path}");
    }
}

/// Defense-in-depth: the admin API strips the body even if the store fails to filter it.
#[tokio::test]
async fn the_api_hides_the_body_even_when_the_store_leaks_it() {
    let fake = with_share_card("local:guest", "local:guest");
    fake.leak_bodies(true);
    let s = start(fake).await;
    for path in ["/v1/admin/approvals?state=all", "/v1/admin/approvals"] {
        let op = get(&s, OPERATOR, path).await;
        assert!(
            !op.body.contains(BODY),
            "{path}: operator must not see body behind a leaky store"
        );
        assert_eq!(card(&op.json(), "ap-mem")["render_hidden"], true);
    }
    let own = get(&s, GUEST, "/v1/admin/approvals").await;
    assert_eq!(body_row(card(&own.json(), "ap-mem")), Some(BODY));
    let fake = with_share_card("local:guest", "local:owner");
    fake.leak_bodies(true);
    let s = start(fake).await;
    let seen = seen();
    let r = post(
        &s,
        OPERATOR,
        PATH,
        &format!(r#"{{"allow":false,"seen":"{seen}"}}"#),
    )
    .await;
    assert!(!r.body.contains(BODY), "{}", r.body);
}

#[tokio::test]
async fn a_card_whose_requester_and_approver_differ_hides_the_body_from_both() {
    let s = start(with_share_card("local:guest", "local:owner")).await;
    for token in [OPERATOR, GUEST] {
        let r = get(&s, token, "/v1/admin/approvals").await;
        assert!(
            !r.body.contains(BODY),
            "neither principal may view body when requester != approver"
        );
        assert_eq!(card(&r.json(), "ap-mem")["render_hidden"], true);
    }
}

#[tokio::test]
async fn the_operator_who_owns_the_memory_sees_their_own_card() {
    let s = start(with_share_card("local:owner", "local:owner")).await;
    let r = get(&s, OPERATOR, "/v1/admin/approvals").await;
    assert_eq!(body_row(card(&r.json(), "ap-mem")), Some(BODY));
    let guest = get(&s, GUEST, "/v1/admin/approvals").await;
    assert!(!guest.body.contains(BODY) && !guest.body.contains("ap-mem"));
}

#[tokio::test]
async fn an_approver_who_cannot_see_the_body_cannot_approve_but_can_deny() {
    let s = start(with_share_card("local:guest", "local:owner")).await;
    let list = get(&s, OPERATOR, "/v1/admin/approvals").await.json();
    let a = card(&list, "ap-mem");
    assert_eq!(
        (a["render_hidden"].as_bool(), a["approve_blocked"].as_str()),
        (Some(true), Some("no_body"))
    );
    let seen = a["args_sha256"].as_str().expect("digest").to_owned();
    let allow = post(
        &s,
        OPERATOR,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(
        (allow.status, allow.json()["error"].as_str()),
        (403, Some("no_body"))
    );
    assert!(s.fake.decisions().is_empty(), "must not reach broker");
    assert!(!allow.body.contains(BODY));
    let audits = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json()["items"]
        .as_array()
        .map(Vec::len);
    assert_eq!(audits, Some(2), "blocked approval must not append audit");
    let deny = post(
        &s,
        OPERATOR,
        PATH,
        &format!(r#"{{"allow":false,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(deny.status, 200, "{}", deny.body);
    assert_eq!(deny.json()["approval"]["state"], "denied");
    assert_eq!(s.fake.decisions().len(), 1);
    assert!(!deny.body.contains(BODY));
}

#[tokio::test]
async fn an_owner_whose_card_arrives_without_a_body_cannot_approve_blind_either() {
    let fake = with_share_card("local:guest", "local:guest");
    fake.edit(|d| {
        if let Some(a) = d.approvals.iter_mut().find(|a| a.nonce.0 == "ap-mem") {
            a.render.clear();
        }
    });
    let s = start(fake).await;
    let a = card(
        &get(&s, GUEST, "/v1/admin/approvals").await.json(),
        "ap-mem",
    )
    .clone();
    assert_eq!(
        (a["render_hidden"].as_bool(), a["approve_blocked"].as_str()),
        (Some(false), Some("no_body")),
        "owner cannot approve without rendered body"
    );
    let seen = a["args_sha256"].as_str().expect("digest").to_owned();
    let allow = post(
        &s,
        GUEST,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(
        (allow.status, allow.json()["error"].as_str()),
        (403, Some("no_body"))
    );
    assert!(s.fake.decisions().is_empty());
    assert_eq!(
        post(
            &s,
            GUEST,
            PATH,
            &format!(r#"{{"allow":false,"seen":"{seen}"}}"#)
        )
        .await
        .status,
        200
    );
}

#[tokio::test]
async fn the_block_holds_even_when_the_store_leaks_and_never_touches_ordinary_cards() {
    let fake = with_share_card("local:guest", "local:owner");
    fake.leak_bodies(true);
    let s = start(fake).await;
    let list = get(&s, OPERATOR, "/v1/admin/approvals").await;
    assert!(!list.body.contains(BODY));
    let seen = card(&list.json(), "ap-mem")["args_sha256"]
        .as_str()
        .expect("digest")
        .to_owned();
    let allow = post(
        &s,
        OPERATOR,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(
        (allow.status, allow.json()["error"].as_str()),
        (403, Some("no_body"))
    );
    assert!(s.fake.decisions().is_empty());
    assert!(card(&list.json(), "ap-1")["approve_blocked"].is_null());
    // Non-designated approver is rejected on approver identity before the body check.
    let guest = post(
        &s,
        GUEST,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(guest.status, 403);
    assert_ne!(guest.json()["error"], "no_body");
}

type Attempt = (u16, Option<String>, Option<String>, usize);

/// Attempts an owner approval with the given rendered rows and digest.
async fn owner_attempt(render: &[(&str, &str)], hash: [u8; 32]) -> Attempt {
    let fake = with_share_card("local:guest", "local:guest");
    fake.edit(|d| {
        if let Some(a) = d.approvals.iter_mut().find(|a| a.nonce.0 == "ap-mem") {
            a.render = render
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            a.args_sha256 = hash;
        }
    });
    let s = start(fake).await;
    let list = get(&s, GUEST, "/v1/admin/approvals").await.json();
    let blocked = card(&list, "ap-mem")["approve_blocked"]
        .as_str()
        .map(str::to_owned);
    let seen = bot_admin::json::hex(&hash);
    let r = post(
        &s,
        GUEST,
        PATH,
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    (
        r.status,
        r.json()["error"].as_str().map(str::to_owned),
        blocked,
        s.fake.decisions().len(),
    )
}

const REFUSED: (u16, Option<&str>, Option<&str>, usize) =
    (403, Some("no_body"), Some("no_body"), 0);

#[tokio::test]
async fn a_card_with_only_a_title_or_a_scope_line_cannot_be_approved() {
    let good = share_hash(MEM, BODY);
    let cases: [&[(&str, &str)]; 4] = [
        &[(LABEL_TITLE, "Memory share approval")],
        &[
            (LABEL_TITLE, "Memory share approval"),
            (LABEL_SHARE_SCOPE, "Profile-wide"),
        ],
        &[(LABEL_MEMORY_ID, MEM)],
        &[(LABEL_BODY, BODY)],
    ];
    for render in cases {
        let (status, word, blocked, decisions) = owner_attempt(render, good).await;
        assert_eq!(
            (status, word.as_deref(), blocked.as_deref(), decisions),
            REFUSED,
            "{render:?}"
        );
    }
}

type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, [u8; 32]);

#[tokio::test]
async fn a_card_with_a_missing_changed_or_foreign_body_cannot_be_approved() {
    let good = share_hash(MEM, BODY);
    let half = &BODY[..BODY.len() / 2];
    let changed = BODY.replace("synthetic", "synthetiX");
    let bad: Vec<Case> = vec![
        (
            "truncated body",
            vec![(LABEL_MEMORY_ID, MEM), (LABEL_BODY, half)],
            good,
        ),
        (
            "single-char body mismatch",
            vec![(LABEL_MEMORY_ID, MEM), (LABEL_BODY, &changed)],
            good,
        ),
        (
            "empty body",
            vec![(LABEL_MEMORY_ID, MEM), (LABEL_BODY, "")],
            good,
        ),
        (
            "different memory id",
            vec![(LABEL_MEMORY_ID, "mem-2"), (LABEL_BODY, BODY)],
            good,
        ),
        (
            "digest for different body",
            vec![(LABEL_MEMORY_ID, MEM), (LABEL_BODY, BODY)],
            share_hash(MEM, "other body"),
        ),
        (
            "duplicate body rows",
            vec![
                (LABEL_MEMORY_ID, MEM),
                (LABEL_BODY, BODY),
                (LABEL_BODY, "fake body"),
            ],
            good,
        ),
        (
            "duplicate memory id rows",
            vec![
                (LABEL_MEMORY_ID, MEM),
                (LABEL_MEMORY_ID, "mem-2"),
                (LABEL_BODY, BODY),
            ],
            good,
        ),
        (
            "similar but non-matching labels",
            vec![("Memory", MEM), ("Body excerpt", BODY)],
            good,
        ),
    ];
    for (name, render, hash) in bad {
        let (status, word, blocked, decisions) = owner_attempt(&render, hash).await;
        assert_eq!(
            (status, word.as_deref(), blocked.as_deref(), decisions),
            REFUSED,
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_card_showing_the_exact_id_and_whole_body_can_be_approved_in_either_form() {
    let (status, word, blocked, decisions) = owner_attempt(
        &[(LABEL_MEMORY_ID, MEM), (LABEL_BODY, BODY)],
        share_hash(MEM, BODY),
    )
    .await;
    assert_eq!((status, word, blocked, decisions), (200, None, None, 1));
    // Accepts both raw body and JSON-quoted string (as rendered by broker cards).
    let long = format!(
        "first line\nsecond line \"quote\" \\ end {}",
        "€★◆ ".repeat(400)
    );
    let quoted = serde_json::to_string(&long).expect("JSON");
    for shown in [long.as_str(), quoted.as_str()] {
        let (status, word, blocked, decisions) = owner_attempt(
            &[(LABEL_MEMORY_ID, MEM), (LABEL_BODY, shown)],
            share_hash(MEM, &long),
        )
        .await;
        assert_eq!(
            (status, word, blocked, decisions),
            (200, None, None, 1),
            "{}",
            &shown[..20]
        );
    }
    let other = serde_json::to_string("other text").expect("JSON");
    let (status, word, _, decisions) = owner_attempt(
        &[(LABEL_MEMORY_ID, MEM), (LABEL_BODY, &other)],
        share_hash(MEM, BODY),
    )
    .await;
    assert_eq!(
        (status, word.as_deref(), decisions),
        (403, Some("no_body"), 0)
    );
}

#[tokio::test]
async fn refusing_stays_allowed_for_every_incomplete_card() {
    for render in [
        vec![(LABEL_TITLE, "Memory share approval")],
        vec![(LABEL_MEMORY_ID, MEM), (LABEL_BODY, "partial")],
    ] {
        let fake = with_share_card("local:guest", "local:guest");
        fake.edit(|d| {
            if let Some(a) = d.approvals.iter_mut().find(|a| a.nonce.0 == "ap-mem") {
                a.render = render
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect();
            }
        });
        let s = start(fake).await;
        let deny = post(
            &s,
            GUEST,
            PATH,
            &format!(r#"{{"allow":false,"seen":"{}"}}"#, seen()),
        )
        .await;
        assert_eq!(deny.status, 200, "{}", deny.body);
        assert_eq!(deny.json()["approval"]["state"], "denied");
    }
}

#[tokio::test]
async fn memory_save_cards_hide_body_and_set_render_hidden_for_non_owners() {
    let fake = with_share_card("local:guest", "local:guest");
    fake.edit(|d| {
        if let Some(a) = d.approvals.iter_mut().find(|a| a.nonce.0 == "ap-mem") {
            a.tool = ToolId("memory.save".into());
            a.render = vec![
                (LABEL_TITLE.into(), "Memory save approval".into()),
                (LABEL_BODY.into(), BODY.into()),
            ];
        }
    });
    let s = start(fake).await;

    let op_resp = get(&s, OPERATOR, "/v1/admin/approvals").await;
    assert_eq!(op_resp.status, 200);
    assert!(!op_resp.body.contains(BODY));
    let op_card = card(&op_resp.json(), "ap-mem").clone();
    assert_eq!(op_card["render"].as_array().map(Vec::len), Some(0));
    assert_eq!(op_card["render_hidden"], true);

    let owner_list = get(&s, GUEST, "/v1/admin/approvals").await.json();
    let owner_card = card(&owner_list, "ap-mem");
    assert_eq!(owner_card["render_hidden"], false);
    assert_eq!(body_row(owner_card), Some(BODY));
}
