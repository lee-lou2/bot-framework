//! PostgreSQL `CredentialStore` and same-connection advisory-locked `CredentialLease` for encrypted credentials.
//!
//! Sole narrow exception to invariant 4: `lock_credential` holds a session-level PG advisory lock on a
//! dedicated `close_on_drop` connection across token refresh HTTP so multi-worker refreshes serialize without
//! holding transaction locks or risking pool-starvation token loss.

use crate::{
    audit::{self, Entry},
    db::{R, Store},
};
use bot_core::{
    auth::{
        CredentialKind, CredentialLease, CredentialState, CredentialStore, CredentialSummary,
        CredentialUpsert, StoredCredential,
    },
    id::{ProfileId, SecretName},
    port::{Error, Reply},
};
use sqlx::{Connection, Postgres, Row, pool::PoolConnection, postgres::PgRow};
use std::time::Duration;
use tokio::time::{Instant, sleep};

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn decode_row(row: &PgRow) -> R<StoredCredential> {
    let profile = ProfileId(row.get::<String, _>("profile"));
    let name = SecretName::parse(&row.get::<String, _>("name")).ok_or(Error::Invalid)?;
    let kind = CredentialKind::parse(&row.get::<String, _>("kind")).ok_or(Error::Invalid)?;
    let state = CredentialState::parse(&row.get::<String, _>("state")).ok_or(Error::Invalid)?;
    Ok(StoredCredential {
        profile,
        name,
        kind,
        token_endpoint_host: row.get("token_endpoint_host"),
        nonce: row.get("nonce"),
        ciphertext: row.get("ciphertext"),
        version: row.get("version"),
        state,
        refresh_nonce: row.get("refresh_nonce"),
        refresh_until_ms: row.get("refresh_until_ms"),
        expires_ms: row.get("expires_ms"),
        updated_ms: row.get("updated_ms"),
    })
}

fn decode_summary(row: &PgRow) -> R<CredentialSummary> {
    let profile = ProfileId(row.get::<String, _>("profile"));
    let name = SecretName::parse(&row.get::<String, _>("name")).ok_or(Error::Invalid)?;
    let kind = CredentialKind::parse(&row.get::<String, _>("kind")).ok_or(Error::Invalid)?;
    let state = CredentialState::parse(&row.get::<String, _>("state")).ok_or(Error::Invalid)?;
    Ok(CredentialSummary {
        profile,
        name,
        kind,
        token_endpoint_host: row.get("token_endpoint_host"),
        version: row.get("version"),
        state,
        expires_ms: row.get("expires_ms"),
        updated_ms: row.get("updated_ms"),
    })
}

fn audit_target(name: &SecretName, kind: CredentialKind) -> String {
    format!("{}:{}", name.as_str(), kind.as_str())
}

const SELECT_COLS: &str = "profile, name, kind, token_endpoint_host, nonce, ciphertext, version, state, refresh_nonce, refresh_until_ms, expires_ms, updated_ms";
const SUMMARY_COLS: &str =
    "profile, name, kind, token_endpoint_host, version, state, expires_ms, updated_ms";

struct PgCredentialLease {
    conn: PoolConnection<Postgres>,
    profile: ProfileId,
    name: SecretName,
    unlocked: bool,
}

impl PgCredentialLease {
    async fn get_inner(&mut self) -> R<Option<StoredCredential>> {
        let query =
            format!("select {SELECT_COLS} from bot_credentials where profile = $1 and name = $2");
        let row = sqlx::query(&query)
            .bind(&self.profile.0)
            .bind(self.name.as_str())
            .fetch_optional(&mut *self.conn)
            .await?;
        row.as_ref().map(decode_row).transpose()
    }

    async fn now_ms_inner(&mut self) -> R<i64> {
        Ok(sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *self.conn)
            .await?)
    }

    async fn begin_refresh_inner(
        &mut self,
        expected_version: i64,
        expected_nonce: &[u8],
        refresh_nonce: &str,
        lease_ms: i64,
    ) -> R<i64> {
        let mut tx = self.conn.begin().await?;
        let row = sqlx::query(
            "update bot_credentials
             set state = 'refreshing',
                 refresh_nonce = $3,
                 refresh_until_ms = bot_now_ms() + $4,
                 version = version + 1,
                 updated_ms = bot_now_ms()
             where profile = $1 and name = $2 and version = $5 and nonce = $6
             returning version",
        )
        .bind(&self.profile.0)
        .bind(self.name.as_str())
        .bind(refresh_nonce)
        .bind(lease_ms)
        .bind(expected_version)
        .bind(expected_nonce)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(Error::Conflict.into());
        };
        let version: i64 = row.get("version");
        audit::append(
            &mut tx,
            &self.profile.0,
            &Entry::new(
                "system:auth",
                "credential.refresh_begin",
                self.name.as_str(),
                "refreshing",
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(version)
    }

    async fn commit_refresh_inner(
        &mut self,
        expected_version: i64,
        expected_nonce: &[u8],
        next: CredentialUpsert,
    ) -> R<i64> {
        if next.profile != self.profile || next.name != self.name {
            return Err(Error::Invalid.into());
        }
        let mut tx = self.conn.begin().await?;
        let row = sqlx::query(
            "update bot_credentials
             set kind = $3,
                 token_endpoint_host = $4,
                 nonce = $5,
                 ciphertext = $6,
                 expires_ms = $7,
                 state = 'active',
                 refresh_nonce = null,
                 refresh_until_ms = null,
                 version = version + 1,
                 updated_ms = bot_now_ms()
             where profile = $1 and name = $2 and version = $8 and nonce = $9
             returning version",
        )
        .bind(&self.profile.0)
        .bind(self.name.as_str())
        .bind(next.kind.as_str())
        .bind(&next.token_endpoint_host)
        .bind(&next.nonce)
        .bind(&next.ciphertext)
        .bind(next.expires_ms)
        .bind(expected_version)
        .bind(expected_nonce)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(Error::Conflict.into());
        };
        let version: i64 = row.get("version");
        let target = audit_target(&self.name, next.kind);
        audit::append(
            &mut tx,
            &self.profile.0,
            &Entry::new("system:auth", "credential.refresh", &target, "ok"),
        )
        .await?;
        tx.commit().await?;
        Ok(version)
    }

    async fn mark_needs_login_inner(
        &mut self,
        expected_version: i64,
        expected_nonce: &[u8],
        reason: &'static str,
    ) -> R<i64> {
        let mut tx = self.conn.begin().await?;
        let row = sqlx::query(
            "update bot_credentials
             set state = 'needs_login',
                 refresh_nonce = null,
                 refresh_until_ms = null,
                 version = version + 1,
                 updated_ms = bot_now_ms()
             where profile = $1 and name = $2 and version = $3 and nonce = $4
             returning version, kind",
        )
        .bind(&self.profile.0)
        .bind(self.name.as_str())
        .bind(expected_version)
        .bind(expected_nonce)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(Error::Conflict.into());
        };
        let version: i64 = row.get("version");
        let kind = CredentialKind::parse(&row.get::<String, _>("kind")).ok_or(Error::Invalid)?;
        let target = audit_target(&self.name, kind);
        audit::append(
            &mut tx,
            &self.profile.0,
            &Entry::new("system:auth", "credential.needs_login", &target, reason),
        )
        .await?;
        tx.commit().await?;
        Ok(version)
    }

    async fn abort_refresh_inner(
        &mut self,
        expected_version: i64,
        expected_nonce: &[u8],
        reason: &'static str,
    ) -> R<i64> {
        let mut tx = self.conn.begin().await?;
        let row = sqlx::query(
            "update bot_credentials
             set state = 'active',
                 refresh_nonce = null,
                 refresh_until_ms = null,
                 version = version + 1,
                 updated_ms = bot_now_ms()
             where profile = $1 and name = $2 and version = $3 and nonce = $4
             returning version, kind",
        )
        .bind(&self.profile.0)
        .bind(self.name.as_str())
        .bind(expected_version)
        .bind(expected_nonce)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(Error::Conflict.into());
        };
        let version: i64 = row.get("version");
        let kind = CredentialKind::parse(&row.get::<String, _>("kind")).ok_or(Error::Invalid)?;
        let target = audit_target(&self.name, kind);
        audit::append(
            &mut tx,
            &self.profile.0,
            &Entry::new("system:auth", "credential.refresh_aborted", &target, reason),
        )
        .await?;
        tx.commit().await?;
        Ok(version)
    }

    async fn release_inner(mut self) -> R<()> {
        if !self.unlocked {
            let _ = sqlx::query(
                "select pg_advisory_unlock(hashtextextended('bot.credential:' || $1 || ':' || $2, 0))",
            )
            .bind(&self.profile.0)
            .bind(self.name.as_str())
            .execute(&mut *self.conn)
            .await;
            self.unlocked = true;
        }
        Ok(())
    }
}

impl CredentialLease for PgCredentialLease {
    fn get(&mut self) -> Reply<'_, Option<StoredCredential>> {
        Box::pin(async move { self.get_inner().await.map_err(|e| e.into_core()) })
    }

    fn now_ms(&mut self) -> Reply<'_, i64> {
        Box::pin(async move { self.now_ms_inner().await.map_err(|e| e.into_core()) })
    }

    fn begin_refresh(
        &mut self,
        expected_version: i64,
        expected_nonce: Vec<u8>,
        refresh_nonce: String,
        lease_ms: i64,
    ) -> Reply<'_, i64> {
        Box::pin(async move {
            self.begin_refresh_inner(expected_version, &expected_nonce, &refresh_nonce, lease_ms)
                .await
                .map_err(|e| e.into_core())
        })
    }

    fn commit_refresh(
        &mut self,
        expected_version: i64,
        expected_nonce: Vec<u8>,
        next: CredentialUpsert,
    ) -> Reply<'_, i64> {
        Box::pin(async move {
            self.commit_refresh_inner(expected_version, &expected_nonce, next)
                .await
                .map_err(|e| e.into_core())
        })
    }

    fn mark_needs_login(
        &mut self,
        expected_version: i64,
        expected_nonce: Vec<u8>,
        reason: &'static str,
    ) -> Reply<'_, i64> {
        Box::pin(async move {
            self.mark_needs_login_inner(expected_version, &expected_nonce, reason)
                .await
                .map_err(|e| e.into_core())
        })
    }

    fn abort_refresh(
        &mut self,
        expected_version: i64,
        expected_nonce: Vec<u8>,
        reason: &'static str,
    ) -> Reply<'_, i64> {
        Box::pin(async move {
            self.abort_refresh_inner(expected_version, &expected_nonce, reason)
                .await
                .map_err(|e| e.into_core())
        })
    }

    fn release(self: Box<Self>) -> Reply<'static, ()> {
        Box::pin(async move { self.release_inner().await.map_err(|e| e.into_core()) })
    }
}

impl Store {
    async fn get_credential_inner(
        &self,
        profile: &ProfileId,
        name: &SecretName,
    ) -> R<Option<StoredCredential>> {
        let query =
            format!("select {SELECT_COLS} from bot_credentials where profile = $1 and name = $2");
        let row = sqlx::query(&query)
            .bind(&profile.0)
            .bind(name.as_str())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(decode_row).transpose()
    }

    async fn list_credentials_inner(
        &self,
        profile: Option<&ProfileId>,
    ) -> R<Vec<CredentialSummary>> {
        let rows = match profile {
            Some(profile) => {
                let query = format!(
                    "select {SUMMARY_COLS} from bot_credentials where profile = $1 order by profile, name"
                );
                sqlx::query(&query)
                    .bind(&profile.0)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                let query =
                    format!("select {SUMMARY_COLS} from bot_credentials order by profile, name");
                sqlx::query(&query).fetch_all(&self.pool).await?
            }
        };
        rows.iter().map(decode_summary).collect()
    }

    async fn upsert_credential_inner(&self, actor: &str, entry: CredentialUpsert) -> R<i64> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "insert into bot_credentials (profile, name, kind, token_endpoint_host, nonce, ciphertext, version, state, refresh_nonce, refresh_until_ms, expires_ms, updated_ms)
             values ($1, $2, $3, $4, $5, $6, 1, 'active', null, null, $7, bot_now_ms())
             on conflict (profile, name) do update
             set kind = excluded.kind,
                 token_endpoint_host = excluded.token_endpoint_host,
                 nonce = excluded.nonce,
                 ciphertext = excluded.ciphertext,
                 version = bot_credentials.version + 1,
                 state = 'active',
                 refresh_nonce = null,
                 refresh_until_ms = null,
                 expires_ms = excluded.expires_ms,
                 updated_ms = bot_now_ms()
             returning version",
        )
        .bind(&entry.profile.0)
        .bind(entry.name.as_str())
        .bind(entry.kind.as_str())
        .bind(&entry.token_endpoint_host)
        .bind(&entry.nonce)
        .bind(&entry.ciphertext)
        .bind(entry.expires_ms)
        .fetch_one(&mut *tx)
        .await?;
        let version: i64 = row.get("version");
        let target = audit_target(&entry.name, entry.kind);
        audit::append(
            &mut tx,
            &entry.profile.0,
            &Entry::new(actor, "credential.set", &target, "ok"),
        )
        .await?;
        tx.commit().await?;
        Ok(version)
    }

    async fn delete_credential_inner(
        &self,
        actor: &str,
        profile: &ProfileId,
        name: &SecretName,
    ) -> R<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "delete from bot_credentials where profile = $1 and name = $2 returning kind",
        )
        .bind(&profile.0)
        .bind(name.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(false);
        };
        let kind = CredentialKind::parse(&row.get::<String, _>("kind")).ok_or(Error::Invalid)?;
        let target = audit_target(name, kind);
        audit::append(
            &mut tx,
            &profile.0,
            &Entry::new(actor, "credential.logout", &target, "ok"),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn lock_credential_inner(
        &self,
        profile: ProfileId,
        name: SecretName,
        wait: Duration,
    ) -> R<Box<dyn CredentialLease>> {
        let deadline = Instant::now() + wait;
        let mut conn = self.pool.acquire().await?;
        // Always close the dedicated connection on drop so a cancelled or panicked lease never returns a locked session to the pool.
        conn.close_on_drop();
        loop {
            let acquired: bool = sqlx::query_scalar(
                "select pg_try_advisory_lock(hashtextextended('bot.credential:' || $1 || ':' || $2, 0))",
            )
            .bind(&profile.0)
            .bind(name.as_str())
            .fetch_one(&mut *conn)
            .await?;
            if acquired {
                return Ok(Box::new(PgCredentialLease {
                    conn,
                    profile,
                    name,
                    unlocked: false,
                }));
            }
            if Instant::now() >= deadline {
                return Err(Error::Unavailable.into());
            }
            sleep(LOCK_POLL_INTERVAL).await;
        }
    }
}

impl CredentialStore for Store {
    fn get_credential(
        &self,
        profile: ProfileId,
        name: SecretName,
    ) -> Reply<'_, Option<StoredCredential>> {
        Box::pin(async move {
            self.get_credential_inner(&profile, &name)
                .await
                .map_err(|e| e.into_core())
        })
    }

    fn list_credentials(&self, profile: Option<ProfileId>) -> Reply<'_, Vec<CredentialSummary>> {
        Box::pin(async move {
            self.list_credentials_inner(profile.as_ref())
                .await
                .map_err(|e| e.into_core())
        })
    }

    fn upsert_credential(&self, actor: String, entry: CredentialUpsert) -> Reply<'_, i64> {
        Box::pin(async move {
            self.upsert_credential_inner(&actor, entry)
                .await
                .map_err(|e| e.into_core())
        })
    }

    fn delete_credential(
        &self,
        actor: String,
        profile: ProfileId,
        name: SecretName,
    ) -> Reply<'_, bool> {
        Box::pin(async move {
            self.delete_credential_inner(&actor, &profile, &name)
                .await
                .map_err(|e| e.into_core())
        })
    }

    fn lock_credential(
        &self,
        profile: ProfileId,
        name: SecretName,
        wait: Duration,
    ) -> Reply<'_, Box<dyn CredentialLease>> {
        Box::pin(async move {
            self.lock_credential_inner(profile, name, wait)
                .await
                .map_err(|e| e.into_core())
        })
    }
}
