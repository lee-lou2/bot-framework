//! Scoped memory storage (`search`, `preview`, `change`, `seed`), committing root-run private-recall taints before returning private rows.

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

/// Expands `"main/research/deep"` to `["main", "main/research", "main/research/deep"]`.
pub(crate) fn lineage(agent: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut end = 0;
    for part in agent.split('/') {
        end += part.len() + usize::from(end > 0);
        out.push(agent[..end].to_owned());
    }
    out
}

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

/// Opens a read transaction locking root -> run before verifying lease/generation/cancel so private taint commits atomically before returning rows.
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
    // Lock root exclusively when `run == root` to avoid upgrading a shared lock to exclusive.
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
    // Read DB clock only after acquiring the lock.
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

/// Marks `private_memory_read` on the root run (already locked by `begin_read`).
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

    /// Fetches one visible memory by ID with its body SHA-256; commits root private taint before returning private content.
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
                // Reset visibility to private on edit so any prior share approval cannot cover modified text.
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

    /// Locks a memory row and verifies owner, ancestor agent lineage, and DM/CLI context for private rows.
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
        if row.get::<String, _>("owner") != principal
            || !lineage(agent).contains(&row.get::<String, _>("agent"))
            || (private && !matches!(kind, "direct" | "cli"))
        {
            return Err(Error::Denied.into());
        }
        Ok(row)
    }

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

/// Memory seed validation error; omits raw body content so it is safe to log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeedError {
    BadField(&'static str),
    EmptyText,
    TextTooLong { bytes: usize, max: usize },
    TextHasNul,
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedError::BadField(field) => write!(f, "invalid memory seed `{field}`"),
            SeedError::EmptyText => f.write_str("memory seed body is empty"),
            SeedError::TextTooLong { bytes, max } => {
                write!(f, "memory seed body is too long ({bytes} bytes, max {max})")
            }
            SeedError::TextHasNul => f.write_str("memory seed body contains a NUL byte"),
        }
    }
}

impl std::error::Error for SeedError {}

/// Shared seed validation used by both CLI preflight checks and DB publication; does not compare `sha256` against `text`.
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

/// Inserts a seed idempotently by `(profile, agent, seed_path, seed_sha256)` without resurrecting deleted seeds.
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
        assert_eq!(
            terms("Today schedule, please! Schedule"),
            ["today", "schedule", "please"]
        );
        assert_eq!(terms("  "), Vec::<String>::new());
        assert_eq!(terms("A b c d e f g h i j").len(), 8);
    }
}
