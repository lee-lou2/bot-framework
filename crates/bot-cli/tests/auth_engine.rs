//! End-to-end tests for host-managed OAuth PG refresh races, crash quarantine, cancellation, and redaction.
mod auth_support;
mod process_support;

use auth_support::{MockTokenServer, make_resolver, open_store, seed_cred};
use bot_admin::trace::Redactor;
use bot_cli::auth::payload::{BodyFormat, CredentialPayload};
use bot_core::{
    auth::{
        AuthError, AuthResolver, AuthSource, CallerSurface, CredentialKind, CredentialState,
        CredentialStore,
    },
    id::{ProfileId, SecretName},
};
use bot_gateway::testing::ports::MemoryActive;
use serde_json::json;
use std::{collections::BTreeMap, sync::atomic::Ordering, time::Duration};

#[tokio::test]
async fn concurrent_two_worker_oauth_refresh_executes_exactly_once_and_redacts_tokens() {
    let Some((_schema, store)) = open_store().await else {
        return;
    };
    let mock = MockTokenServer::start(|idx, body| {
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=rt-initial-1"), "{body}");
        (
            200,
            json!({
                "access_token": format!("at-rotated-{}", idx + 1),
                "refresh_token": format!("rt-rotated-{}", idx + 1),
                "expires_in": 3600
            })
            .to_string(),
            Duration::from_millis(80),
        )
    })
    .await;
    seed_cred(
        &store,
        "personal",
        "mcp-oauth",
        &CredentialPayload::OAuth {
            token_endpoint: mock.token_url(),
            client_id: "client-1".into(),
            client_secret: None,
            access_token: "at-expired-0".into(),
            refresh_token: Some("rt-initial-1".into()),
            expires_ms: Some(1_000),
            scope: Some("read".into()),
            headers: Vec::new(),
            body_format: BodyFormat::Form,
            subscription_provider: None,
            resource: None,
            issuer: None,
        },
    )
    .await;

    let active = MemoryActive::new(Some("rev-1")).check("rev-1");
    let redactor_a = Redactor::default();
    let redactor_b = Redactor::default();
    let worker_a = make_resolver(
        store.clone(),
        active.clone(),
        BTreeMap::new(),
        redactor_a.clone(),
        &[],
    );
    let worker_b = make_resolver(store.clone(), active, BTreeMap::new(), redactor_b, &[]);

    let src = AuthSource::Credential {
        name: SecretName::parse("mcp-oauth").unwrap(),
        expected_kind: CredentialKind::OAuth,
    };
    let (res_a, res_b) = tokio::join!(
        worker_a.resolve(
            ProfileId("personal".into()),
            src.clone(),
            CallerSurface::RemoteMcp,
            false
        ),
        worker_b.resolve(
            ProfileId("personal".into()),
            src,
            CallerSurface::RemoteMcp,
            false
        ),
    );
    assert_eq!(res_a.unwrap().secret.0, b"at-rotated-1");
    assert_eq!(res_b.unwrap().secret.0, b"at-rotated-1");
    assert_eq!(
        mock.hits.load(Ordering::SeqCst),
        1,
        "PG advisory lock + re-read must collapse 2-worker refresh race into 1 upstream POST"
    );
    let masked = redactor_a.mask("leaked at-rotated-1 and rt-rotated-1 and rt-initial-1");
    assert!(!masked.contains("at-rotated-1"), "{masked}");
    assert!(!masked.contains("rt-rotated-1"), "{masked}");
    assert!(!masked.contains("rt-initial-1"), "{masked}");
    worker_a.shutdown().await;
    worker_b.shutdown().await;
}

#[tokio::test]
async fn interrupted_refresh_quarantines_to_needs_login_and_transient_restores_active() {
    let Some((_schema, store)) = open_store().await else {
        return;
    };
    let mock = MockTokenServer::start(|idx, _| match idx {
        0 => (
            503,
            r#"{"error":"temporarily_unavailable"}"#.into(),
            Duration::ZERO,
        ),
        _ => (400, r#"{"error":"invalid_grant"}"#.into(), Duration::ZERO),
    })
    .await;

    let oauth_row = |at: &str, rt: &str| CredentialPayload::OAuth {
        token_endpoint: mock.token_url(),
        client_id: "c1".into(),
        client_secret: None,
        access_token: at.into(),
        refresh_token: Some(rt.into()),
        expires_ms: Some(1_000),
        scope: None,
        headers: Vec::new(),
        body_format: BodyFormat::Form,
        subscription_provider: None,
        resource: None,
        issuer: None,
    };
    seed_cred(
        &store,
        "personal",
        "transient-cred",
        &oauth_row("at-0", "rt-0"),
    )
    .await;
    let active = MemoryActive::new(Some("rev-1")).check("rev-1");
    let resolver = make_resolver(
        store.clone(),
        active.clone(),
        BTreeMap::new(),
        Redactor::default(),
        &[],
    );
    // 1. Transient 503 restores state=active via abort_refresh without poisoning.
    assert_eq!(
        resolver
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("transient-cred").unwrap(),
                CredentialKind::OAuth,
                CallerSurface::RemoteMcp,
                false,
            )
            .await
            .err(),
        Some(AuthError::Unavailable)
    );
    let row = store
        .get_credential(
            ProfileId("personal".into()),
            SecretName::parse("transient-cred").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, CredentialState::Active);

    // 2. invalid_grant marks NeedsLogin.
    let resolver2 = make_resolver(
        store.clone(),
        active,
        BTreeMap::new(),
        Redactor::default(),
        &[],
    );
    assert_eq!(
        resolver2
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("transient-cred").unwrap(),
                CredentialKind::OAuth,
                CallerSurface::RemoteMcp,
                false,
            )
            .await
            .err(),
        Some(AuthError::NeedsLogin)
    );
    let row = store
        .get_credential(
            ProfileId("personal".into()),
            SecretName::parse("transient-cred").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, CredentialState::NeedsLogin);

    // 3. Simulated process crash mid-refresh (lease dropped while state='refreshing').
    seed_cred(
        &store,
        "personal",
        "crashed-cred",
        &oauth_row("at-crash", "rt-crash"),
    )
    .await;
    let hits_before = mock.hits.load(Ordering::SeqCst);
    {
        let mut lease = store
            .lock_credential(
                ProfileId("personal".into()),
                SecretName::parse("crashed-cred").unwrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        let cur = lease.get().await.unwrap().unwrap();
        lease
            .begin_refresh(cur.version, cur.nonce, "nonce-dead".into(), 30_000)
            .await
            .unwrap();
    }
    assert_eq!(
        resolver2
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("crashed-cred").unwrap(),
                CredentialKind::OAuth,
                CallerSurface::RemoteMcp,
                false,
            )
            .await
            .err(),
        Some(AuthError::NeedsLogin)
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), hits_before);
    let quarantined = store
        .get_credential(
            ProfileId("personal".into()),
            SecretName::parse("crashed-cred").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(quarantined.state, CredentialState::NeedsLogin);
}

#[tokio::test]
async fn caller_cancellation_and_stale_activation_fence_behave_safely() {
    let Some((_schema, store)) = open_store().await else {
        return;
    };
    let mock = MockTokenServer::start(|_, _| {
        (
            200,
            json!({"access_token": "at-detached-ok", "refresh_token": "rt-detached-ok", "expires_in": 3600})
                .to_string(),
            Duration::from_millis(150),
        )
    })
    .await;
    seed_cred(
        &store,
        "personal",
        "cancel-cred",
        &CredentialPayload::OAuth {
            token_endpoint: mock.token_url(),
            client_id: "c1".into(),
            client_secret: None,
            access_token: "at-old".into(),
            refresh_token: Some("rt-old".into()),
            expires_ms: Some(1_000),
            scope: None,
            headers: Vec::new(),
            body_format: BodyFormat::Form,
            subscription_provider: None,
            resource: None,
            issuer: None,
        },
    )
    .await;
    let mem_active = MemoryActive::new(Some("rev-2"));
    let stale_resolver = make_resolver(
        store.clone(),
        mem_active.check("rev-1"),
        BTreeMap::new(),
        Redactor::default(),
        &[],
    );
    assert_eq!(
        stale_resolver
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("cancel-cred").unwrap(),
                CredentialKind::OAuth,
                CallerSurface::RemoteMcp,
                false,
            )
            .await
            .err(),
        Some(AuthError::Stale)
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 0);

    let resolver = make_resolver(
        store.clone(),
        mem_active.check("rev-2"),
        BTreeMap::new(),
        Redactor::default(),
        &[],
    );
    let _ = tokio::time::timeout(
        Duration::from_millis(30),
        resolver.resolve_managed(
            ProfileId("personal".into()),
            SecretName::parse("cancel-cred").unwrap(),
            CredentialKind::OAuth,
            CallerSurface::RemoteMcp,
            false,
        ),
    )
    .await;
    resolver.shutdown().await;
    let resolved = resolver
        .resolve_managed(
            ProfileId("personal".into()),
            SecretName::parse("cancel-cred").unwrap(),
            CredentialKind::OAuth,
            CallerSurface::RemoteMcp,
            false,
        )
        .await
        .unwrap();
    assert_eq!(resolved.secret.0, b"at-detached-ok");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn second_fence_blocks_post_after_begin_refresh_and_cache_revalidates_logout() {
    use bot_core::{
        active::ActiveCheck,
        port::{Error, Reply},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct FailSecondCheck(AtomicUsize);
    impl ActiveCheck for FailSecondCheck {
        fn verify(&self) -> Reply<'_, ()> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { if n == 0 { Ok(()) } else { Err(Error::Stale) } })
        }
    }

    let Some((_schema, store)) = open_store().await else {
        return;
    };
    let mock = MockTokenServer::start(|idx, _| {
        if idx == 0 {
            (
                400,
                json!({"error": "invalid_client"}).to_string(),
                Duration::ZERO,
            )
        } else {
            (
                200,
                json!({
                    "access_token": format!("at-ok-{idx}"),
                    "refresh_token": format!("rt-ok-{idx}"),
                    "expires_in": 3600
                })
                .to_string(),
                Duration::ZERO,
            )
        }
    })
    .await;

    // 1. Gap activation-fence regression: revision turns stale between begin_refresh and token POST.
    seed_cred(
        &store,
        "personal",
        "gap-cred",
        &CredentialPayload::OAuth {
            token_endpoint: mock.token_url(),
            client_id: "cid".into(),
            client_secret: None,
            access_token: "at-old".into(),
            refresh_token: Some("rt-old".into()),
            expires_ms: Some(1_000),
            scope: None,
            headers: vec![],
            body_format: BodyFormat::Form,
            subscription_provider: None,
            resource: None,
            issuer: None,
        },
    )
    .await;
    let gap_resolver = make_resolver(
        store.clone(),
        Arc::new(FailSecondCheck(AtomicUsize::new(0))),
        BTreeMap::new(),
        Redactor::default(),
        &[],
    );
    assert_eq!(
        gap_resolver
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("gap-cred").unwrap(),
                CredentialKind::OAuth,
                CallerSurface::RemoteMcp,
                false,
            )
            .await
            .err(),
        Some(AuthError::Stale)
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
    let row = store
        .get_credential(
            ProfileId("personal".into()),
            SecretName::parse("gap-cred").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, CredentialState::Active);

    // 2. `invalid_client` is non-permanent (restores `active`, never quarantines to `needs_login`).
    let active = MemoryActive::new(Some("rev-1")).check("rev-1");
    let resolver = make_resolver(
        store.clone(),
        active.clone(),
        BTreeMap::new(),
        Redactor::default(),
        &[],
    )
    .with_cache_ttl(Duration::from_millis(20));
    assert_eq!(
        resolver
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("gap-cred").unwrap(),
                CredentialKind::OAuth,
                CallerSurface::RemoteMcp,
                false,
            )
            .await
            .err(),
        Some(AuthError::Unavailable)
    );
    let row = store
        .get_credential(
            ProfileId("personal".into()),
            SecretName::parse("gap-cred").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, CredentialState::Active);

    // 3. Imported OAuth with `expires_ms: None` and refresh token refreshes on first resolve.
    seed_cred(
        &store,
        "personal",
        "no-exp-cred",
        &CredentialPayload::OAuth {
            token_endpoint: mock.token_url(),
            client_id: "cid".into(),
            client_secret: None,
            access_token: "at-initial".into(),
            refresh_token: Some("rt-initial".into()),
            expires_ms: None,
            scope: None,
            headers: vec![],
            body_format: BodyFormat::Form,
            subscription_provider: None,
            resource: None,
            issuer: None,
        },
    )
    .await;
    let fresh = resolver
        .resolve_managed(
            ProfileId("personal".into()),
            SecretName::parse("no-exp-cred").unwrap(),
            CredentialKind::OAuth,
            CallerSurface::RemoteMcp,
            false,
        )
        .await
        .unwrap();
    assert_eq!(fresh.secret.0, b"at-ok-1");

    // 4. Cache revalidates after TTL: deleting the row (`logout`) surfaces `Missing` without restart.
    store
        .delete_credential(
            "test:logout".into(),
            ProfileId("personal".into()),
            SecretName::parse("no-exp-cred").unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(35)).await;
    assert_eq!(
        resolver
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("no-exp-cred").unwrap(),
                CredentialKind::OAuth,
                CallerSurface::RemoteMcp,
                false,
            )
            .await
            .err(),
        Some(AuthError::Missing)
    );
}
