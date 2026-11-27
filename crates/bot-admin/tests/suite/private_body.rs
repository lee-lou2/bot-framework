//! 기억 공유 카드의 본문은 그 기억의 소유자(요청자이자 승인자)가 인증된 경우에만 나간다.
//! 운영자도 예외가 아니다. 메타데이터(도구·요청자·승인자·상태·인자 해시)와 감사는 그대로 보인다.

use crate::support::*;
use bot_admin::fake::{Fake, approval_view, pid};
use bot_core::{
    command::memory_share_hash,
    id::{MemoryId, ToolId},
};
use sha2::{Digest, Sha256};

const BODY: &str = "PRIVATE-MEMORY-BODY-synthetic-7q";
const MEM: &str = "mem-1";

/// 승인 지문: 기억 ID와 본문 전체(원문 UTF-8의 SHA-256)를 묶은 값. 브로커·저장소와 같은 공통 함수다.
fn share_hash(id: &str, body: &str) -> [u8; 32] {
    memory_share_hash(
        &MemoryId(id.into()),
        &Sha256::digest(body.as_bytes()).into(),
    )
}

/// 화면이 «본 해시»로 보내는 값(카드의 지문).
fn seen() -> String {
    bot_admin::json::hex(&share_hash(MEM, BODY))
}

/// 카드의 `본문` 행 값.
fn body_row(card: &serde_json::Value) -> Option<&str> {
    card["render"]
        .as_array()?
        .iter()
        .find(|r| r["label"] == "본문")
        .and_then(|r| r["value"].as_str())
}
const PATH: &str = "/v1/admin/approvals/ap-mem/decide";

/// 소유자 `local:guest`가 자기 비공개 기억을 profile로 공유하려는 승인 카드.
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
            .expect("호출");
        let mut a = approval_view(&call, &run, "ap-mem", approver);
        a.tool = ToolId("memory.share".into());
        a.requester = pid(requester);
        a.args_sha256 = share_hash(MEM, BODY);
        a.render = vec![
            ("제목".into(), "기억 공유 승인".into()),
            ("기억 ID".into(), MEM.into()),
            ("공유 범위".into(), "프로필 전체".into()),
            ("본문".into(), BODY.into()),
        ];
        d.approvals.push(a);
    });
    fake
}

fn card<'a>(list: &'a serde_json::Value, nonce: &str) -> &'a serde_json::Value {
    list["items"]
        .as_array()
        .and_then(|v| v.iter().find(|a| a["nonce"] == nonce))
        .unwrap_or_else(|| panic!("카드 없음: {nonce}: {list}"))
}

#[tokio::test]
async fn the_operator_sees_the_cards_metadata_but_never_the_private_body() {
    let s = start(with_share_card("local:guest", "local:guest")).await;
    let r = get(&s, OPERATOR, "/v1/admin/approvals?state=all").await;
    assert_eq!(r.status, 200);
    assert!(!r.body.contains(BODY), "운영자 응답에 본문이 있다");
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
    assert_eq!(
        a["args_sha256"].as_str().map(str::len),
        Some(64),
        "인자 해시는 그대로 본다"
    );
    assert_eq!(
        (
            a["render"].as_array().map(Vec::len),
            a["render_hidden"].as_bool()
        ),
        (Some(0), Some(true))
    );
    // 같은 목록의 일반 승인 카드는 운영자에게 그대로 보인다.
    let normal = card(&r.json(), "ap-1").clone();
    assert!(
        normal["render"].as_array().is_some_and(|v| !v.is_empty())
            && normal["render_hidden"] == false
    );
    // 운영자는 대신 누를 수도 없다(승인자가 아니다).
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
    assert_eq!(
        body_row(&a),
        Some(BODY),
        "소유자는 승인할 본문을 그대로 본다"
    );
    let seen = a["args_sha256"].as_str().expect("해시").to_owned();
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
    // 같은 소유자의 재전송도 본문을 보여 주지만, 운영자가 처리된 목록을 봐도 본문은 없다.
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
    assert_eq!(
        card(&op.json(), "ap-mem")["state"],
        "approved",
        "처리 결과(메타데이터)는 보인다"
    );
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
        assert!(!r.body.contains(BODY), "{path}에 본문이 있다");
    }
}

/// 저장소가 본문을 거르지 못해도(버그) 관리 API가 스스로 막는다.
#[tokio::test]
async fn the_api_hides_the_body_even_when_the_store_leaks_it() {
    let fake = with_share_card("local:guest", "local:guest");
    fake.leak_bodies(true);
    let s = start(fake).await;
    for path in ["/v1/admin/approvals?state=all", "/v1/admin/approvals"] {
        let op = get(&s, OPERATOR, path).await;
        assert!(
            !op.body.contains(BODY),
            "{path}: 새는 저장소 뒤에서도 운영자에게는 본문이 없어야 한다"
        );
        assert_eq!(card(&op.json(), "ap-mem")["render_hidden"], true);
    }
    // 소유자에게는 그대로 나간다.
    let own = get(&s, GUEST, "/v1/admin/approvals").await;
    assert_eq!(body_row(card(&own.json(), "ap-mem")), Some(BODY));
    // 운영자가 처리 응답을 받는 경우(운영자가 승인자인 카드)도 소유자가 아니면 본문은 없다.
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
            "요청자와 승인자가 다르면 아무도 못 본다"
        );
        assert_eq!(card(&r.json(), "ap-mem")["render_hidden"], true);
    }
}

#[tokio::test]
async fn the_operator_who_owns_the_memory_sees_their_own_card() {
    let s = start(with_share_card("local:owner", "local:owner")).await;
    let r = get(&s, OPERATOR, "/v1/admin/approvals").await;
    assert_eq!(body_row(card(&r.json(), "ap-mem")), Some(BODY));
    // 다른 주체(소유자가 아닌 일반 주체)는 그 카드를 아예 못 본다.
    let guest = get(&s, GUEST, "/v1/admin/approvals").await;
    assert!(!guest.body.contains(BODY) && !guest.body.contains("ap-mem"));
}

/// 승인(긍정 판정)은 카드 본문을 실제로 볼 수 있을 때만 받는다. 볼 수 없는 내용을 승인하게 되면 안 된다. 거절은 늘 허용한다.
#[tokio::test]
async fn an_approver_who_cannot_see_the_body_cannot_approve_but_can_deny() {
    // 요청자 local:guest, 지정 승인자 local:owner(운영자): 본문은 둘 다 못 본다.
    let s = start(with_share_card("local:guest", "local:owner")).await;
    let list = get(&s, OPERATOR, "/v1/admin/approvals").await.json();
    let a = card(&list, "ap-mem");
    assert_eq!(
        (a["render_hidden"].as_bool(), a["approve_blocked"].as_str()),
        (Some(true), Some("no_body"))
    );
    let seen = a["args_sha256"].as_str().expect("해시").to_owned();
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
    assert!(s.fake.decisions().is_empty(), "브로커까지 가지 않는다");
    assert!(!allow.body.contains(BODY));
    let audits = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json()["items"]
        .as_array()
        .map(Vec::len);
    assert_eq!(audits, Some(2), "막힌 승인은 감사 기록을 만들지 않는다");
    // 거절은 허용된다.
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
            a.render.clear(); // 저장소가 본문을 채우지 못했다
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
        "소유자라도 본문이 없으면 승인할 수 없다"
    );
    let seen = a["args_sha256"].as_str().expect("해시").to_owned();
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
    fake.leak_bodies(true); // 저장소가 본문을 거르지 못해도
    let s = start(fake).await;
    let list = get(&s, OPERATOR, "/v1/admin/approvals").await;
    assert!(!list.body.contains(BODY));
    let seen = card(&list.json(), "ap-mem")["args_sha256"]
        .as_str()
        .expect("해시")
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
    // 일반 승인 카드는 영향이 없다.
    assert!(card(&list.json(), "ap-1")["approve_blocked"].is_null());
    // 지정 승인자가 아닌 사람은 본문 때문이 아니라 먼저 «지정된 승인자만»으로 거절된다.
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

/// 승인 시도의 결과: (상태, 오류 낱말, 목록의 approve_blocked, 브로커가 받은 판정 수).
type Attempt = (u16, Option<String>, Option<String>, usize);

/// 소유자(요청자이자 승인자)가 지금 이 카드 행·지문으로 승인을 시도한다. (상태, 오류 낱말, 목록의 approve_blocked, 브로커가 받은 판정 수)
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

/// 승인은 «승인 지문과 일치하는 기억 ID·본문 전체»가 카드에 보일 때만 받는다. 아래는 모두 소유자가 승인을 시도한 경우다.
#[tokio::test]
async fn a_card_with_only_a_title_or_a_scope_line_cannot_be_approved() {
    let good = share_hash(MEM, BODY);
    let cases: [&[(&str, &str)]; 4] = [
        &[("제목", "기억 공유 승인")],
        &[("제목", "기억 공유 승인"), ("공유 범위", "프로필 전체")],
        &[("기억 ID", MEM)],
        &[("본문", BODY)],
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

/// 시험 사례: (이름, 카드의 보이는 행, 승인 지문).
type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, [u8; 32]);

#[tokio::test]
async fn a_card_with_a_missing_changed_or_foreign_body_cannot_be_approved() {
    let good = share_hash(MEM, BODY);
    let half = &BODY[..BODY.len() / 2];
    let changed = BODY.replace("synthetic", "synthetiX");
    let bad: Vec<Case> = vec![
        (
            "본문 일부 누락",
            vec![("기억 ID", MEM), ("본문", half)],
            good,
        ),
        (
            "본문이 한 글자 다름",
            vec![("기억 ID", MEM), ("본문", &changed)],
            good,
        ),
        ("빈 본문", vec![("기억 ID", MEM), ("본문", "")], good),
        (
            "다른 기억 ID",
            vec![("기억 ID", "mem-2"), ("본문", BODY)],
            good,
        ),
        (
            "지문이 다른 본문의 것",
            vec![("기억 ID", MEM), ("본문", BODY)],
            share_hash(MEM, "다른 본문"),
        ),
        (
            "본문 행이 둘(모호)",
            vec![("기억 ID", MEM), ("본문", BODY), ("본문", "가짜 본문")],
            good,
        ),
        (
            "기억 ID 행이 둘(모호)",
            vec![("기억 ID", MEM), ("기억 ID", "mem-2"), ("본문", BODY)],
            good,
        ),
        (
            "라벨이 비슷하지만 다름",
            vec![("기억", MEM), ("본문 일부", BODY)],
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
    // 원문 그대로.
    let (status, word, blocked, decisions) =
        owner_attempt(&[("기억 ID", MEM), ("본문", BODY)], share_hash(MEM, BODY)).await;
    assert_eq!((status, word, blocked, decisions), (200, None, None, 1));
    // 줄바꿈·따옴표·역슬래시가 든 긴 본문과, 그 본문을 JSON 문자열(따옴표 포함)로 보인 카드(브로커 카드의 형식).
    let long = format!("첫 줄\n둘째 줄 \"인용\" \\ 끝 {}", "가나다 ".repeat(400));
    let quoted = serde_json::to_string(&long).expect("JSON");
    for shown in [long.as_str(), quoted.as_str()] {
        let (status, word, blocked, decisions) =
            owner_attempt(&[("기억 ID", MEM), ("본문", shown)], share_hash(MEM, &long)).await;
        assert_eq!(
            (status, word, blocked, decisions),
            (200, None, None, 1),
            "{}",
            &shown[..20]
        );
    }
    // 보이는 글이 JSON 문자열이어도 풀었을 때 본문과 다르면 막힌다.
    let other = serde_json::to_string("다른 글").expect("JSON");
    let (status, word, _, decisions) =
        owner_attempt(&[("기억 ID", MEM), ("본문", &other)], share_hash(MEM, BODY)).await;
    assert_eq!(
        (status, word.as_deref(), decisions),
        (403, Some("no_body"), 0)
    );
}

#[tokio::test]
async fn refusing_stays_allowed_for_every_incomplete_card() {
    for render in [
        vec![("제목", "기억 공유 승인")],
        vec![("기억 ID", MEM), ("본문", "일부")],
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
