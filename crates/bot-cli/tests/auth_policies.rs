//! End-to-end tests for Google Service Account JWT-bearer minting and caller-surface auth policies.
mod auth_support;
mod process_support;

use auth_support::{MockTokenServer, make_resolver, open_store, seed_cred};
use bot_admin::trace::Redactor;
use bot_cli::auth::payload::{BodyFormat, CredentialPayload};
use bot_core::{
    auth::{AuthError, AuthResolver, AuthSource, CallerSurface, CliVendor, CredentialKind},
    id::{ProfileId, SecretName},
};
use bot_gateway::testing::ports::MemoryActive;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::Ordering,
    time::Duration,
};

#[tokio::test]
async fn google_service_account_jwt_exchange_and_surface_policies_are_enforced() {
    let Some((_schema, store)) = open_store().await else {
        return;
    };
    let pem_out = std::process::Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
        ])
        .output()
        .expect("openssl genpkey");
    assert!(pem_out.status.success());
    let pem = String::from_utf8(pem_out.stdout).unwrap();

    let mock = MockTokenServer::start(|_, body| {
        assert!(
            body.contains("urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"),
            "{body}"
        );
        assert!(body.contains("assertion="), "{body}");
        (
            200,
            json!({"access_token": "ya29.sa-minted-token", "expires_in": 3600}).to_string(),
            Duration::ZERO,
        )
    })
    .await;
    seed_cred(
        &store,
        "personal",
        "gcp-sa",
        &CredentialPayload::ServiceAccount {
            client_email: "bot@proj.iam.gserviceaccount.com".into(),
            private_key_pem: pem,
            token_uri: mock.token_url(),
            scope: "https://www.googleapis.com/auth/cloud-platform".into(),
            access_token: None,
            expires_ms: None,
        },
    )
    .await;
    seed_cred(
        &store,
        "personal",
        "claude-sub",
        &CredentialPayload::SubscriptionToken {
            provider: "claude".into(),
            token: "sk-ant-oat01-live-test".into(),
        },
    )
    .await;
    seed_cred(
        &store,
        "personal",
        "codex-sub",
        &CredentialPayload::OAuth {
            token_endpoint: mock.token_url(),
            client_id: "app_codex_1".into(),
            client_secret: None,
            access_token: "codex-access-1".into(),
            refresh_token: Some("codex-rt-1".into()),
            expires_ms: Some(i64::MAX / 2),
            scope: None,
            headers: vec![("chatgpt-account-id".into(), "acct-codex-1".into())],
            body_format: BodyFormat::Json,
            subscription_provider: Some("codex".into()),
            resource: None,
            issuer: None,
        },
    )
    .await;

    let active = MemoryActive::new(Some("rev-1")).check("rev-1");
    let redactor = Redactor::default();
    let resolver_no_sub = make_resolver(
        store.clone(),
        active.clone(),
        BTreeMap::new(),
        redactor.clone(),
        &[("personal", "sneaky-oat", "sk-ant-oat01-static")],
    );

    let sa = resolver_no_sub
        .resolve(
            ProfileId("personal".into()),
            AuthSource::Credential {
                name: SecretName::parse("gcp-sa").unwrap(),
                expected_kind: CredentialKind::ServiceAccount,
            },
            CallerSurface::CliRelay(CliVendor::Google),
            false,
        )
        .await
        .unwrap();
    assert_eq!(sa.secret.0, b"ya29.sa-minted-token");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    assert!(
        !redactor
            .mask("token ya29.sa-minted-token")
            .contains("ya29.sa-minted-token")
    );
    assert_eq!(
        resolver_no_sub
            .resolve(
                ProfileId("personal".into()),
                AuthSource::Secret(SecretName::parse("sneaky-oat").unwrap()),
                CallerSurface::DirectApi,
                false,
            )
            .await
            .err(),
        Some(AuthError::Denied)
    );
    assert_eq!(
        resolver_no_sub
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("claude-sub").unwrap(),
                CredentialKind::SubscriptionToken,
                CallerSurface::CliRelay(CliVendor::Claude),
                false,
            )
            .await
            .err(),
        Some(AuthError::Denied)
    );

    let allow = BTreeMap::from([(
        ProfileId("personal".into()),
        BTreeSet::from(["claude".to_owned(), "codex".to_owned()]),
    )]);
    let resolver_sub = make_resolver(store, active, allow, Redactor::default(), &[]);
    assert_eq!(
        resolver_sub
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("claude-sub").unwrap(),
                CredentialKind::SubscriptionToken,
                CallerSurface::CliRelay(CliVendor::Claude),
                false,
            )
            .await
            .unwrap()
            .secret
            .0,
        b"sk-ant-oat01-live-test"
    );
    let x = resolver_sub
        .resolve_managed(
            ProfileId("personal".into()),
            SecretName::parse("codex-sub").unwrap(),
            CredentialKind::OAuth,
            CallerSurface::CliRelay(CliVendor::Codex),
            false,
        )
        .await
        .unwrap();
    assert_eq!(x.secret.0, b"codex-access-1");
    assert_eq!(
        x.headers,
        vec![("chatgpt-account-id".into(), "acct-codex-1".into())]
    );
    assert_eq!(
        resolver_sub
            .resolve_managed(
                ProfileId("personal".into()),
                SecretName::parse("codex-sub").unwrap(),
                CredentialKind::OAuth,
                CallerSurface::DirectApi,
                false,
            )
            .await
            .err(),
        Some(AuthError::Denied)
    );
}
