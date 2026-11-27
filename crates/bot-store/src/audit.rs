//! Host-wide SHA-256 audit hash chain.
//! Hash input: `"bot-audit-v1" || profile || prev(32) || seq(u64 BE) || at_ms(i64 BE) || actor || action || target || args(0|1||32) || verdict` (u32-BE length-prefixed strings).

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
    pub run_command_id: Option<&'a str>,
}

impl<'a> Entry<'a> {
    pub(crate) fn new(actor: &'a str, action: &'a str, target: &'a str, verdict: &'a str) -> Self {
        Self {
            actor,
            action,
            target,
            args: None,
            verdict,
            run_command_id: None,
        }
    }
}

pub(crate) struct OwnedEntry {
    pub profile: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub args: Option<[u8; 32]>,
    pub verdict: String,
    pub run_command_id: Option<String>,
}

impl OwnedEntry {
    pub(crate) fn new(
        profile: impl Into<String>,
        actor: impl Into<String>,
        action: impl Into<String>,
        target: impl Into<String>,
        verdict: impl Into<String>,
    ) -> Self {
        Self {
            profile: profile.into(),
            actor: actor.into(),
            action: action.into(),
            target: target.into(),
            args: None,
            verdict: verdict.into(),
            run_command_id: None,
        }
    }

    pub(crate) fn from_entry(profile: &str, e: &Entry<'_>) -> Self {
        Self {
            profile: profile.to_owned(),
            actor: e.actor.to_owned(),
            action: e.action.to_owned(),
            target: e.target.to_owned(),
            args: e.args,
            verdict: e.verdict.to_owned(),
            run_command_id: e.run_command_id.map(str::to_owned),
        }
    }

    fn as_entry(&self) -> Entry<'_> {
        Entry {
            actor: &self.actor,
            action: &self.action,
            target: &self.target,
            args: self.args,
            verdict: &self.verdict,
            run_command_id: self.run_command_id.as_deref(),
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

/// Locks the audit head and appends one entry; call last in a transaction to keep head-lock duration short.
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
        "insert into bot_audit (seq, profile, at_ms, actor, action, target, args_sha256, verdict, prev_hash, hash, run_command_id)
         values ($1, nullif($2, ''), $3, $4, $5, $6, $7, $8, $9, $10, $11)",
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
    .bind(e.run_command_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("update bot_audit_head set seq = $1, hash = $2 where singleton")
        .bind(seq)
        .bind(hash.as_slice())
        .execute(&mut **tx)
        .await?;
    Ok(seq)
}

/// Locks the audit head once and appends multiple entries; call just before commit.
pub(crate) async fn append_many(tx: &mut Tx<'_>, entries: &[OwnedEntry]) -> R<()> {
    if entries.is_empty() {
        return Ok(());
    }
    sqlx::query("insert into bot_audit_head (singleton, seq, hash) values (true, 0, $1) on conflict do nothing")
        .bind(GENESIS.as_slice())
        .execute(&mut **tx)
        .await?;
    let head = sqlx::query(
        "select seq, hash, bot_now_ms() as now_ms from bot_audit_head where singleton for update",
    )
    .fetch_one(&mut **tx)
    .await?;
    let (mut seq, mut prev, now_ms): (i64, Vec<u8>, i64) =
        (head.get("seq"), head.get("hash"), head.get("now_ms"));
    let count = entries.len();
    let mut seqs = Vec::with_capacity(count);
    let mut profiles = Vec::with_capacity(count);
    let mut actors = Vec::with_capacity(count);
    let mut actions = Vec::with_capacity(count);
    let mut targets = Vec::with_capacity(count);
    let mut args = Vec::with_capacity(count);
    let mut verdicts = Vec::with_capacity(count);
    let mut prevs = Vec::with_capacity(count);
    let mut hashes = Vec::with_capacity(count);
    let mut commands = Vec::with_capacity(count);
    for item in entries {
        seq += 1;
        let entry = item.as_entry();
        let hash = link(&item.profile, &prev, seq, now_ms, &entry);
        let next = hash.to_vec();
        seqs.push(seq);
        profiles.push(item.profile.as_str());
        actors.push(item.actor.as_str());
        actions.push(item.action.as_str());
        targets.push(item.target.as_str());
        args.push(item.args.map(|a| a.to_vec()));
        verdicts.push(item.verdict.as_str());
        prevs.push(prev);
        hashes.push(next.clone());
        commands.push(item.run_command_id.as_deref());
        prev = next;
    }
    sqlx::query(
        "insert into bot_audit (seq, profile, at_ms, actor, action, target, args_sha256, verdict, prev_hash, hash, run_command_id)
         select s, nullif(p, ''), $3, actor, action, target, args_sha256, verdict, prev_hash, hash, run_command_id
         from unnest($1::bigint[], $2::text[], $4::text[], $5::text[], $6::text[], $7::bytea[], $8::text[], $9::bytea[], $10::bytea[], $11::text[])
           as t(s, p, actor, action, target, args_sha256, verdict, prev_hash, hash, run_command_id)",
    )
    .bind(&seqs)
    .bind(&profiles)
    .bind(now_ms)
    .bind(&actors)
    .bind(&actions)
    .bind(&targets)
    .bind(&args)
    .bind(&verdicts)
    .bind(&prevs)
    .bind(&hashes)
    .bind(&commands)
    .execute(&mut **tx)
    .await?;
    sqlx::query("update bot_audit_head set seq = $1, hash = $2 where singleton")
        .bind(seq)
        .bind(&prev)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Recomputes the chain and compares against `bot_audit_head` in one `REPEATABLE READ READ ONLY` snapshot so concurrent appends cannot look like tail truncation.
pub(crate) async fn verify_chain(store: &Store) -> R<ChainReport> {
    let mut tx = store.pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read read only")
        .execute(&mut *tx)
        .await?;
    let report = scan_chain(&mut tx).await;
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
                run_command_id: None,
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
