//! Shared test harness for host-managed auth engine integration tests.
use crate::process_support::Schema;
use bot_admin::trace::Redactor;
use bot_broker::testing::{FakeResolver, FakeSecrets};
use bot_cli::auth::{
    CREDENTIAL_KEY_NAME, CREDENTIAL_KEY_PROFILE, HostAuthResolver,
    crypto::{MasterKey, aad_bytes},
    http::AuthHttp,
    payload::{CredentialPayload, canonical_host_port},
};
use bot_core::{
    active::ActiveCheck,
    auth::{CredentialStore, CredentialUpsert},
    id::{ProfileId, SecretName},
};
use bot_store::{Config, Store};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub const MASTER_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

pub async fn open_store() -> Option<(Schema, Arc<Store>)> {
    let schema = Schema::create()?;
    let store = Store::connect(&schema.url(), Config::default())
        .await
        .expect("connect test DB");
    store.migrate().await.expect("migrate test DB");
    Some((schema, Arc::new(store)))
}

pub struct MockTokenServer {
    addr: std::net::SocketAddr,
    pub hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockTokenServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockTokenServer {
    pub async fn start<F>(handler: F) -> Self
    where
        F: Fn(usize, &str) -> (u16, String, Duration) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = hits.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits_bg.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 32 * 1024];
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let body = req.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
                    let idx = hits.fetch_add(1, Ordering::SeqCst);
                    let (status, resp_body, delay) = handler(idx, &body);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let reply = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        Self { addr, hits, task }
    }

    pub fn token_url(&self) -> String {
        format!("http://127.0.0.1:{}/oauth/token", self.addr.port())
    }
}

pub async fn seed_cred(
    store: &Store,
    profile: &str,
    name: &str,
    payload: &CredentialPayload,
) -> i64 {
    let master = MasterKey::parse_hex(MASTER_HEX).unwrap();
    let profile_id = ProfileId(profile.to_owned());
    let secret_name = SecretName::parse(name).unwrap();
    let host_port = payload
        .token_endpoint()
        .map(|u| canonical_host_port(u, true).unwrap())
        .unwrap_or_default();
    let aad = aad_bytes(&profile_id, &secret_name, payload.kind(), &host_port);
    let plain = serde_json::to_vec(payload).unwrap();
    let (nonce, ciphertext) = master.seal(&aad, &plain).unwrap();
    store
        .upsert_credential(
            "test:seed".into(),
            CredentialUpsert {
                profile: profile_id,
                name: secret_name,
                kind: payload.kind(),
                token_endpoint_host: host_port,
                nonce,
                ciphertext,
                expires_ms: payload.expires_ms(),
            },
        )
        .await
        .unwrap()
}

pub fn make_resolver(
    store: Arc<Store>,
    active: Arc<dyn ActiveCheck>,
    allow_sub: BTreeMap<ProfileId, BTreeSet<String>>,
    redactor: Redactor,
    extra_secrets: &[(&str, &str, &str)],
) -> HostAuthResolver {
    let mut entries = vec![(CREDENTIAL_KEY_PROFILE, CREDENTIAL_KEY_NAME, MASTER_HEX)];
    entries.extend_from_slice(extra_secrets);
    let secrets = Arc::new(FakeSecrets::new(&entries));
    let http = AuthHttp::for_testing(Arc::new(FakeResolver::new(&[]))).unwrap();
    HostAuthResolver::new(
        secrets,
        Some(store as Arc<dyn CredentialStore>),
        active,
        http,
        BTreeMap::new(),
        allow_sub,
        Arc::new(move |s| redactor.add(s)),
    )
}
