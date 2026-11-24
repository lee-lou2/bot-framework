//! 감사 해시 체인. 호스트 전체에 순번이 1부터 하나로 이어지고 각 행은 이전 해시를 포함해 SHA-256으로 묶인다(프로필은 해시에 들어가는 표지다).
//! 해시 입력: "bot-audit-v1" ‖ profile ‖ prev(32) ‖ seq(u64 BE) ‖ at_ms(i64 BE) ‖ actor ‖ action ‖ target ‖ args(0|1‖32) ‖ verdict.
//! 문자열은 길이(u32 BE)를 앞에 붙인다. DB 전체를 다시 쓸 수 있는 관리자까지 검증하지는 못한다(security.md).

use crate::db::{R, Store, Tx};
use bot_core::command::ChainReport;
use sha2::{Digest, Sha256};
use sqlx::Row;

const GENESIS: [u8; 32] = [0; 32];

pub(crate) struct Entry<'a> {
    pub actor: &'a str,
    pub action: &'a str,
    pub target: &'a str,
    pub args: Option<[u8; 32]>,
    pub verdict: &'a str,
}

impl<'a> Entry<'a> {
    pub(crate) fn new(actor: &'a str, action: &'a str, target: &'a str, verdict: &'a str) -> Self {
        Self {
            actor,
            action,
            target,
            args: None,
            verdict,
        }
    }
}

fn link(profile: &str, prev: &[u8], seq: i64, at_ms: i64, e: &Entry<'_>) -> [u8; 32] {
    let mut h = Sha256::new();
    let mut text = |s: &str| {
        h.update((s.len() as u32).to_be_bytes());
        h.update(s.as_bytes());
    };
    text("bot-audit-v1");
    text(profile);
    h.update(prev);
    h.update(seq.to_be_bytes());
    h.update(at_ms.to_be_bytes());
    for field in [e.actor, e.action, e.target] {
        h.update((field.len() as u32).to_be_bytes());
        h.update(field.as_bytes());
    }
    match e.args {
        Some(args) => {
            h.update([1]);
            h.update(args);
        }
        None => h.update([0]),
    }
    h.update((e.verdict.len() as u32).to_be_bytes());
    h.update(e.verdict.as_bytes());
    h.finalize().into()
}

/// 같은 트랜잭션에서 head를 잠그고 한 줄을 잇는다. 상태 변경 뒤 **마지막에** 부른다(잠금을 짧게 쥐려고).
pub(crate) async fn append(tx: &mut Tx<'_>, profile: &str, e: &Entry<'_>) -> R<i64> {
    sqlx::query("insert into bot_audit_head (singleton, seq, hash) values (true, 0, $1) on conflict do nothing")
        .bind(GENESIS.as_slice())
        .execute(&mut **tx)
        .await?;
    let head = sqlx::query(
        "select seq, hash, bot_now_ms() as now_ms from bot_audit_head where singleton for update",
    )
    .fetch_one(&mut **tx)
    .await?;
    let (prev_seq, prev, now_ms): (i64, Vec<u8>, i64) =
        (head.get("seq"), head.get("hash"), head.get("now_ms"));
    let seq = prev_seq + 1;
    let hash = link(profile, &prev, seq, now_ms, e);
    sqlx::query(
        "insert into bot_audit (seq, profile, at_ms, actor, action, target, args_sha256, verdict, prev_hash, hash)
         values ($1, nullif($2, ''), $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(seq)
    .bind(profile)
    .bind(now_ms)
    .bind(e.actor)
    .bind(e.action)
    .bind(e.target)
    .bind(e.args.as_ref().map(|a| a.as_slice()))
    .bind(e.verdict)
    .bind(&prev)
    .bind(hash.as_slice())
    .execute(&mut **tx)
    .await?;
    sqlx::query("update bot_audit_head set seq = $1, hash = $2 where singleton")
        .bind(seq)
        .bind(hash.as_slice())
        .execute(&mut **tx)
        .await?;
    Ok(seq)
}

/// 체인을 처음부터 다시 계산한다: 순번 공백·이전 해시 불일치·내용 변조·꼬리 삭제(head와 불일치)를 찾는다.
///
/// 행(쪽마다)과 head를 **한 스냅숏**에서 읽는다(`REPEATABLE READ`·읽기 전용 트랜잭션 하나). 읽는 동안 다른 트랜잭션이 항목을 이어 붙이면(행과 head는 같은 커밋으로
/// 움직인다) 따로 읽은 head만 그 항목을 보고 «꼬리가 지워졌다»로 오인하기 때문이다. 스냅숏이 하나면 이어 붙는 중에도 변조 없는 체인은 늘 정상이다.
pub(crate) async fn verify_chain(store: &Store) -> R<ChainReport> {
    let mut tx = store.pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read read only")
        .execute(&mut *tx)
        .await?;
    let report = scan_chain(&mut tx).await;
    // 읽기뿐인 트랜잭션이다: 결과와 상관없이 끝낸다(오류면 롤백과 같다)
    tx.rollback().await?;
    report
}

async fn scan_chain(tx: &mut Tx<'_>) -> R<ChainReport> {
    let mut prev = GENESIS.to_vec();
    let mut expected: i64 = 1;
    let mut bad: Option<i64> = None;
    'scan: loop {
        let rows = sqlx::query(
            "select seq, coalesce(profile, '') as profile, at_ms, actor, action, target, args_sha256, verdict, prev_hash, hash
             from bot_audit where seq >= $1 order by seq limit 500",
        )
        .bind(expected)
        .fetch_all(&mut **tx)
        .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let seq: i64 = row.get("seq");
            let stored_prev: Vec<u8> = row.get("prev_hash");
            let args: Option<Vec<u8>> = row.get("args_sha256");
            let args = args.and_then(|a| <[u8; 32]>::try_from(a).ok());
            let (actor, action, target, verdict): (String, String, String, String) = (
                row.get("actor"),
                row.get("action"),
                row.get("target"),
                row.get("verdict"),
            );
            let entry = Entry {
                actor: &actor,
                action: &action,
                target: &target,
                args,
                verdict: &verdict,
            };
            let computed = link(
                &row.get::<String, _>("profile"),
                &prev,
                seq,
                row.get("at_ms"),
                &entry,
            );
            let stored: Vec<u8> = row.get("hash");
            if seq != expected || stored_prev != prev || stored.as_slice() != computed.as_slice() {
                bad = Some(expected);
                break 'scan;
            }
            prev = stored;
            expected += 1;
        }
    }
    let entries = (expected - 1) as u64;
    if bad.is_none() {
        let head = sqlx::query("select seq, hash from bot_audit_head where singleton")
            .fetch_optional(&mut **tx)
            .await?;
        let (head_seq, head_hash): (i64, Vec<u8>) = match head {
            Some(row) => (row.get("seq"), row.get("hash")),
            None => (0, GENESIS.to_vec()),
        };
        if head_seq != expected - 1 || head_hash != prev {
            bad = Some(expected);
        }
    }
    let mut head = GENESIS;
    head.copy_from_slice(&prev);
    Ok(ChainReport {
        entries,
        head,
        first_bad: bad.map(|b| b as u64),
    })
}
