//! 기억 저장. 프로필·에이전트 범위에서 **현재 요청자에게 보이는 행만** 읽는다.
//! - 읽기: 프로필 공용(profile)은 어디서나, private은 소유자 본인의 DM·CLI에서만. 위임한 자식도 같은 요청자·대화 종류를 이어받는다.
//!   private 행이 한 건이라도 나가면 모델에 전달하기 전에 요청 트리(루트 run)에 플래그를 커밋한다.
//! - 쓰기: owner는 인증된 요청자로 고정한다. 수정·삭제는 소유자만, 수정하면 공유가 풀려 다시 승인받아야 한다.
//! - 미리보기(`preview`): 카드에 쓸 기억 한 건을 ID로 읽는다(검색과 같은 가시성). 본문 SHA-256을 함께 준다.
//! - 공유(`share`, share.rs): 소유자의 승인 카드를 소비하는 한 커밋 안에서만 하고, 카드에 보인 본문 지문이 그대로일 때만 한다.
//!
//! 범위는 기억을 만든 에이전트와 그 아래 에이전트다(조상 → 자손 방향으로 읽는다). 삭제는 본문을 비운 표지로 남긴다.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store},
    finish::lock_leased,
};
use bot_core::{
    command::{MemoryChange, MemoryHit, MemoryPreview, MemorySeed},
    id::*,
    port::Error,
    state::Visibility,
};
use sha2::{Digest, Sha256};
use sqlx::Row;

const MAX_TEXT_BYTES: usize = 8 * 1024;
const MAX_HITS: i64 = 20;
const MAX_TERMS: usize = 8;

fn visibility_str(v: Visibility) -> &'static str {
    match v {
        Visibility::Private => "private",
        Visibility::Profile => "profile",
    }
}

fn visibility_of(s: &str) -> Visibility {
    if s == "profile" {
        Visibility::Profile
    } else {
        Visibility::Private
    }
}

/// `main/research/deep` → [`main`, `main/research`, `main/research/deep`]
pub(crate) fn lineage(agent: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut end = 0;
    for part in agent.split('/') {
        end += part.len() + usize::from(end > 0);
        out.push(agent[..end].to_owned());
    }
    out
}

/// 검색어를 글자·숫자 단위 낱말로 쪼개 소문자로 맞춘다. 낱말은 부분 일치로 찾는다(조사가 붙은 낱말도 찾도록).
fn terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_lowercase();
        if !out.contains(&word) {
            out.push(word);
        }
        if out.len() == MAX_TERMS {
            break;
        }
    }
    out
}

fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

fn text_ok(text: &str) -> R<()> {
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES || text.contains('\0') {
        invalid()
    } else {
        Ok(())
    }
}

/// 읽기 결과를 확정하는 트랜잭션을 연다: 루트(위임 자식이면)와 이 run을 잠근 **뒤에** 현재 claim 토큰·세대·리스·취소 요청을 확인한다.
/// 결과를 돌려주기 전에 같은 트랜잭션에서 private 표시를 커밋하므로, 확인한 소유자가 결과를 내보내는 순간에도 소유자다.
/// 잠금 순서: 루트 → 이 run(다른 경로와 같다). 취소·인계는 이 잠금이 풀린 뒤에 반영된다(결과는 이미 나간 효과 앞이다).
pub(crate) async fn begin_read<'a>(
    store: &'a Store,
    at: &Attempt,
) -> R<(crate::db::Tx<'a>, sqlx::postgres::PgRow)> {
    let mut tx = store.pool.begin().await?;
    let root: String =
        sqlx::query_scalar("select root from bot_runs where profile = $1 and id = $2")
            .bind(&at.profile.0)
            .bind(&at.run.0)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Stale)?;
    if root != at.run.0 {
        sqlx::query("select 1 from bot_runs where profile = $1 and id = $2 for update")
            .bind(&at.profile.0)
            .bind(&root)
            .fetch_optional(&mut *tx)
            .await?;
    }
    // 이 run이 루트면 아래 잠금이 곧 루트 잠금이다(같은 행을 공유→배타로 올리지 않는다)
    let me = sqlx::query(if root == at.run.0 {
        "select profile, id, root, agent, principal, conv_kind, binding, conv_id, thread, created_ms, trigger,
                state, generation, claim_epoch, lease_until_ms, private, cancel_requested
         from bot_runs where profile = $1 and id = $2 for update"
    } else {
        "select profile, id, root, agent, principal, conv_kind, binding, conv_id, thread, created_ms, trigger,
                state, generation, claim_epoch, lease_until_ms, private, cancel_requested
         from bot_runs where profile = $1 and id = $2 for share"
    })
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::Stale)?;
    // 시각은 잠금을 얻은 다음 문장에서 읽는다
    let now: i64 = sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut *tx)
        .await?;
    let live = me.get::<String, _>("state") == "leased"
        && me.get::<i64, _>("generation") as u64 == at.generation.0
        && at.claim == Some(ClaimEpoch(me.get::<i64, _>("claim_epoch") as u64))
        && me
            .get::<Option<i64>, _>("lease_until_ms")
            .is_some_and(|until| until >= now)
        && !me.get::<bool, _>("private")
        && !me.get::<bool, _>("cancel_requested");
    if !live {
        return Err(Error::Stale.into());
    }
    Ok((tx, me))
}

/// private 행이 나간다: 요청 트리(루트 run)에 표시를 커밋한다(루트 행은 `begin_read`가 이미 잠갔다).
pub(crate) async fn flag_private(tx: &mut crate::db::Tx<'_>, at: &Attempt, root: &str) -> R<()> {
    sqlx::query(
        "update bot_runs set private_memory_read = true, updated_ms = bot_now_ms() where profile = $1 and id = $2",
    )
    .bind(&at.profile.0)
    .bind(root)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    pub(crate) async fn memory_search(&self, at: &Attempt, query: &str) -> R<Vec<MemoryHit>> {
        let (mut tx, me) = begin_read(self, at).await?;
        let (principal, kind, agent): (String, String, String) =
            (me.get("principal"), me.get("conv_kind"), me.get("agent"));
        let private_ok = matches!(kind.as_str(), "direct" | "cli");
        let patterns: Vec<String> = terms(query).iter().map(|t| like_pattern(t)).collect();
        let rows = sqlx::query(
            "select id, visibility, body,
                    (select count(*) from unnest($5::text[]) p where lower(m.body) like p escape '\\') as score
             from bot_memories m
             where m.profile = $1 and m.deleted_ms is null and m.agent = any($2)
               and (m.visibility = 'profile' or ($4 and m.owner = $3))
               and (cardinality($5::text[]) = 0 or exists (select 1 from unnest($5::text[]) p where lower(m.body) like p escape '\\'))
             order by score desc, m.updated_ms desc, m.id
             limit $6",
        )
        .bind(&at.profile.0)
        .bind(lineage(&agent))
        .bind(&principal)
        .bind(private_ok)
        .bind(&patterns)
        .bind(MAX_HITS)
        .fetch_all(&mut *tx)
        .await?;
        let hits: Vec<MemoryHit> = rows
            .iter()
            .map(|r| MemoryHit {
                id: MemoryId(r.get("id")),
                visibility: visibility_of(&r.get::<String, _>("visibility")),
                text: r.get("body"),
            })
            .collect();
        if hits.iter().any(|h| h.visibility == Visibility::Private) {
            flag_private(&mut tx, at, &me.get::<String, _>("root")).await?;
        }
        tx.commit().await?;
        Ok(hits)
    }

    /// 승인 카드에 보일 기억 한 건을 ID로 읽는다(스캔 없음). 가시성은 검색과 같다: 프로필 공용이거나, 소유자 본인의 DM·CLI에서 본 private.
    /// 보이지 않는 항목은 없는 것과 같다(`Missing`). private 본문이 나가면 같은 트랜잭션에서 요청 트리에 표시를 커밋한 뒤에 돌려준다.
    pub(crate) async fn memory_preview(&self, at: &Attempt, id: &MemoryId) -> R<MemoryPreview> {
        ids_ok(&[&id.0])?;
        let (mut tx, me) = begin_read(self, at).await?;
        let (principal, kind, agent): (String, String, String) =
            (me.get("principal"), me.get("conv_kind"), me.get("agent"));
        let private_ok = matches!(kind.as_str(), "direct" | "cli");
        let row = sqlx::query(
            "select visibility, body from bot_memories m
             where m.profile = $1 and m.id = $2 and m.deleted_ms is null and m.agent = any($3)
               and (m.visibility = 'profile' or ($5 and m.owner = $4))",
        )
        .bind(&at.profile.0)
        .bind(&id.0)
        .bind(lineage(&agent))
        .bind(&principal)
        .bind(private_ok)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let visibility = visibility_of(&row.get::<String, _>("visibility"));
        let text: String = row.get("body");
        if visibility == Visibility::Private {
            flag_private(&mut tx, at, &me.get::<String, _>("root")).await?;
        }
        tx.commit().await?;
        Ok(MemoryPreview {
            id: id.clone(),
            visibility,
            sha256: Sha256::digest(text.as_bytes()).into(),
            text,
        })
    }

    pub(crate) async fn memory_change(&self, at: &Attempt, change: MemoryChange) -> R<MemoryId> {
        let mut tx = self.pool.begin().await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private || run.cancel_requested {
            return Err(Error::Denied.into());
        }
        let me = sqlx::query("select agent, principal, conv_kind, generation from bot_runs where profile = $1 and id = $2")
            .bind(&run.profile)
            .bind(&run.id)
            .fetch_one(&mut *tx)
            .await?;
        let (agent, principal, kind): (String, String, String) =
            (me.get("agent"), me.get("principal"), me.get("conv_kind"));
        let (id, action, digest) = match change {
            MemoryChange::Create { text } => {
                text_ok(&text)?;
                let id: String = sqlx::query_scalar(
                    "insert into bot_memories (profile, agent, owner, body) values ($1, $2, $3, $4) returning id",
                )
                .bind(&run.profile)
                .bind(&agent)
                .bind(&principal)
                .bind(&text)
                .fetch_one(&mut *tx)
                .await?;
                (id, "memory.create", Some(sha256_text(&text)))
            }
            MemoryChange::Edit { id, text } => {
                text_ok(&text)?;
                self.owned(&mut tx, &run.profile, &id.0, &principal, &kind, &agent)
                    .await?;
                // 본문이 바뀌면 이미 한 공유 승인은 이 내용에 대한 것이 아니다: 공유를 풀어 다시 승인받게 한다
                sqlx::query(
                    "update bot_memories set body = $3, visibility = 'private', updated_ms = bot_now_ms() where profile = $1 and id = $2",
                )
                .bind(&run.profile)
                .bind(&id.0)
                .bind(&text)
                .execute(&mut *tx)
                .await?;
                (id.0, "memory.edit", Some(sha256_text(&text)))
            }
            MemoryChange::Delete { id } => {
                self.owned(&mut tx, &run.profile, &id.0, &principal, &kind, &agent)
                    .await?;
                sqlx::query("update bot_memories set body = '', deleted_ms = bot_now_ms(), updated_ms = bot_now_ms() where profile = $1 and id = $2")
                    .bind(&run.profile)
                    .bind(&id.0)
                    .execute(&mut *tx)
                    .await?;
                (id.0, "memory.delete", None)
            }
        };
        let mut entry = Entry::new(&principal, action, &id, "ok");
        entry.args = digest;
        audit::append(&mut tx, &run.profile, &entry).await?;
        tx.commit().await?;
        Ok(MemoryId(id))
    }

    /// 소유자만 고칠 수 있다. 없는(지운) 기억은 `Missing`, 남의 기억은 `Denied`.
    /// private 기억은 자기 DM·CLI에서만 다룬다(채널에서는 읽지도 못하므로 고치지도 못한다).
    async fn owned(
        &self,
        tx: &mut crate::db::Tx<'_>,
        profile: &str,
        id: &str,
        principal: &str,
        kind: &str,
        agent: &str,
    ) -> R<sqlx::postgres::PgRow> {
        let row = sqlx::query(&format!("select {} from bot_memories where profile = $1 and id = $2 and deleted_ms is null for update", crate::cols::MEMORIES))
            .bind(profile)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::Missing)?;
        let private = row.get::<String, _>("visibility") == "private";
        // 소유자 본인의 기억이어도 이 에이전트와 그 조상이 만든 것만 고칠 수 있다(형제 에이전트의 기억은 보이지 않는 것과 같다)
        if row.get::<String, _>("owner") != principal
            || !lineage(agent).contains(&row.get::<String, _>("agent"))
            || (private && !matches!(kind, "direct" | "cli"))
        {
            return Err(Error::Denied.into());
        }
        Ok(row)
    }

    /// 시드 하나를 등록한다(단독). 활성 집합 게시는 같은 트랜잭션 안에서 `seed_in`을 쓴다.
    pub(crate) async fn memory_seed(&self, seed: MemorySeed) -> R<()> {
        check_seed(&seed).map_err(|_| Error::Invalid)?;
        let mut tx = self.pool.begin().await?;
        if seed_in(&mut tx, &seed).await? {
            audit::append(&mut tx, &seed.profile.0, &seed_audit(&seed)).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// 기억 시드가 저장소 규칙을 어겼다. 본문은 싣지 않는다: 고정된 이유와 길이만 있다(진단·로그에 그대로 써도 안전하다).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeedError {
    /// 식별자 칸(`profile`·`agent`·`path`·`owner`)이 비었거나 너무 길거나 제어 문자를 담았다.
    BadField(&'static str),
    EmptyText,
    TextTooLong {
        bytes: usize,
        max: usize,
    },
    TextHasNul,
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedError::BadField(field) => write!(f, "기억 시드의 `{field}` 값이 올바르지 않아요"),
            SeedError::EmptyText => f.write_str("기억 시드 본문이 비어 있어요"),
            SeedError::TextTooLong { bytes, max } => {
                write!(
                    f,
                    "기억 시드 본문이 너무 길어요({bytes}바이트, 최대 {max}바이트)"
                )
            }
            SeedError::TextHasNul => f.write_str("기억 시드 본문에 NUL 문자가 있어요"),
        }
    }
}

impl std::error::Error for SeedError {}

/// 시드 검증. CLI(활성화 전 점검)와 DB(게시·등록)가 같은 규칙을 쓴다. `sha256`은 호출자가 정하는 내용 식별값이라 본문과 대조하지 않는다.
pub fn check_seed(seed: &MemorySeed) -> Result<(), SeedError> {
    for (name, value) in [
        ("profile", &seed.profile.0),
        ("agent", &seed.agent.0),
        ("path", &seed.path),
        ("owner", &seed.owner.0),
    ] {
        if !id_ok(value) {
            return Err(SeedError::BadField(name));
        }
    }
    if seed.text.trim().is_empty() {
        return Err(SeedError::EmptyText);
    }
    if seed.text.len() > MAX_TEXT_BYTES {
        return Err(SeedError::TextTooLong {
            bytes: seed.text.len(),
            max: MAX_TEXT_BYTES,
        });
    }
    if seed.text.contains('\0') {
        return Err(SeedError::TextHasNul);
    }
    Ok(())
}

/// 시드 한 건을 넣는다. (profile, agent, 경로, 내용 해시)가 같으면 다시 만들지 않고, 지운 시드도 되살리지 않는다. 새로 넣었으면 `true`.
pub(crate) async fn seed_in(tx: &mut crate::db::Tx<'_>, seed: &MemorySeed) -> R<bool> {
    Ok(sqlx::query(
        "insert into bot_memories (profile, agent, owner, visibility, body, seed_path, seed_sha256)
         values ($1, $2, $3, $4, $5, $6, $7) on conflict do nothing",
    )
    .bind(&seed.profile.0)
    .bind(&seed.agent.0)
    .bind(&seed.owner.0)
    .bind(visibility_str(seed.visibility))
    .bind(&seed.text)
    .bind(&seed.path)
    .bind(seed.sha256.as_slice())
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1)
}

/// 새로 등록한 시드의 감사 한 줄(본문 없이 경로·가시성·내용 해시만).
pub(crate) fn seed_audit(seed: &MemorySeed) -> Entry<'_> {
    let mut entry = Entry::new(
        "system",
        "memory.seed",
        &seed.path,
        visibility_str(seed.visibility),
    );
    entry.args = Some(seed.sha256);
    entry
}

#[cfg(test)]
mod tests {
    use super::{like_pattern, lineage, terms};

    #[test]
    fn lineage_lists_self_and_ancestors() {
        assert_eq!(lineage("main"), ["main"]);
        assert_eq!(
            lineage("main/research/deep"),
            ["main", "main/research", "main/research/deep"]
        );
    }

    #[test]
    fn like_patterns_escape_wildcards() {
        assert_eq!(like_pattern("a%b_c\\d"), "%a\\%b\\_c\\\\d%");
    }

    #[test]
    fn terms_split_words_dedupe_and_cap() {
        assert_eq!(terms("오늘 일정, 알려줘! 일정"), ["오늘", "일정", "알려줘"]);
        assert_eq!(terms("  "), Vec::<String>::new());
        assert_eq!(terms("A b c d e f g h i j").len(), 8);
    }
}
