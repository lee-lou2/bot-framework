//! End-to-end Remote MCP OAuth 401 forced refresh + single retry verification over real loopback HTTP and PostgreSQL.
mod process_support;

use bot_admin::trace::Redactor;
use bot_broker::{
    mcp::{http::LEGACY, remote::Remote},
    ports::{Ran, ToolJob, ToolRunner},
    proxy::{SystemResolver, TcpConnector},
    resolve::{HostPort, Resolved},
    runner::{Deps as RunnerDeps, HostRegistry, McpRunner, RunnerConfig},
    sandbox::{Process, Sandbox, SandboxSpec},
    testing::{CountingEntropy, FakeEffects, FakeResolver, FakeSecrets},
};
use bot_cli::{
    auth::{
        CREDENTIAL_KEY_NAME, CREDENTIAL_KEY_PROFILE, HostAuthResolver,
        crypto::{MasterKey, aad_bytes},
        http::AuthHttp,
        payload::{BodyFormat, CredentialPayload, canonical_host_port},
    },
    serve::remote::ReqwestWire,
};
use bot_core::{
    auth::{CredentialStore, CredentialUpsert},
    command::JsonPayload,
    id::{
        Attempt, CallId, ClaimEpoch, Generation, PrincipalId, ProfileId, Revision, RunId,
        SecretName, ToolId,
    },
    port::{Error, Reply},
    profile::Tool,
    state::{Conversation, ConversationKind, Risk, RunOrigin},
};
use bot_gateway::testing::ports::MemoryActive;
use bot_store::{Config, Store};
use process_support::Schema;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const MASTER_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct UnusedSandbox;
impl Sandbox for UnusedSandbox {
    fn spawn(&self, _: SandboxSpec) -> Reply<'_, Process> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn reap(&self, _: &str) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn remote_mcp_401_triggers_single_forced_refresh_and_succeeds() {
    let Some(schema) = Schema::create() else {
        return;
    };
    let store = Arc::new(
        Store::connect(&schema.url(), Config::default())
            .await
            .expect("connect test DB"),
    );
    store.migrate().await.expect("migrate test DB");

    // Mock OAuth token server counting refresh requests.
    let oauth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oauth_port = oauth_listener.local_addr().unwrap().port();
    let refresh_hits = Arc::new(AtomicUsize::new(0));
    let hits_bg = refresh_hits.clone();
    let oauth_task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = oauth_listener.accept().await else {
                break;
            };
            let hits = hits_bg.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                hits.fetch_add(1, Ordering::SeqCst);
                let body = json!({
                    "access_token": "at-fresh-2",
                    "refresh_token": "rt-fresh-2",
                    "expires_in": 3600
                })
                .to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });

    // Mock remote MCP server rejecting stale token with 401 + WWW-Authenticate and accepting rotated token.
    let mcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp_port = mcp_listener.local_addr().unwrap().port();
    let mcp_task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = mcp_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut raw_bytes = Vec::new();
                let mut buf = [0u8; 4096];
                let head_end;
                loop {
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    raw_bytes.extend_from_slice(&buf[..n]);
                    if let Some(pos) = raw_bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        head_end = pos + 4;
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&raw_bytes[..head_end]).into_owned();
                let content_len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while raw_bytes.len() < head_end + content_len {
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    raw_bytes.extend_from_slice(&buf[..n]);
                }
                let body = String::from_utf8_lossy(&raw_bytes[head_end..head_end + content_len]);
                let authed = head
                    .lines()
                    .any(|l| l.eq_ignore_ascii_case("authorization: bearer at-fresh-2"));
                if !authed {
                    let reply = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"mcp\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(reply.as_bytes()).await;
                    return;
                }
                let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let resp = match method {
                    "initialize" => {
                        json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":LEGACY,"capabilities":{"tools":{}},"serverInfo":{"name":"m","version":"1"}}})
                    }
                    "notifications/initialized" => {
                        let r = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(r.as_bytes()).await;
                        return;
                    }
                    "tools/list" => {
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{"name":"echo","description":"Echo tool","inputSchema":{"type":"object"}}]}})
                    }
                    "tools/call" => {
                        json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"{\"ok\":true}"}]}})
                    }
                    _ => {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}})
                    }
                };
                let text = resp.to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });

    let token_url = format!("http://127.0.0.1:{oauth_port}/oauth/token");
    let payload = CredentialPayload::OAuth {
        token_endpoint: token_url.clone(),
        client_id: "mcp-client".into(),
        client_secret: None,
        access_token: "at-stale-1".into(),
        refresh_token: Some("rt-stale-1".into()),
        expires_ms: Some(i64::MAX / 2),
        scope: None,
        headers: Vec::new(),
        body_format: BodyFormat::Form,
        subscription_provider: None,
        resource: None,
        issuer: None,
    };
    let master = MasterKey::parse_hex(MASTER_HEX).unwrap();
    let profile = ProfileId("personal".into());
    let cred = SecretName::parse("remote-mcp-oauth").unwrap();
    let host_port = canonical_host_port(&token_url, true).unwrap();
    let aad = aad_bytes(&profile, &cred, payload.kind(), &host_port);
    let (nonce, ciphertext) = master
        .seal(&aad, &serde_json::to_vec(&payload).unwrap())
        .unwrap();
    store
        .upsert_credential(
            "test:seed".into(),
            CredentialUpsert {
                profile: profile.clone(),
                name: cred.clone(),
                kind: payload.kind(),
                token_endpoint_host: host_port,
                nonce,
                ciphertext,
                expires_ms: payload.expires_ms(),
            },
        )
        .await
        .unwrap();

    let redactor = Redactor::default();
    let secrets = Arc::new(FakeSecrets::new(&[(
        CREDENTIAL_KEY_PROFILE,
        CREDENTIAL_KEY_NAME,
        MASTER_HEX,
    )]));
    let auth = HostAuthResolver::new(
        secrets,
        Some(store as Arc<dyn CredentialStore>),
        MemoryActive::new(Some("rev-1")).check("rev-1"),
        AuthHttp::for_testing(Arc::new(FakeResolver::new(&[]))).unwrap(),
        BTreeMap::new(),
        BTreeMap::new(),
        Arc::new(move |s| redactor.add(s)),
    );
    let remote = Remote::parse_oauth(&format!("http://127.0.0.1:{mcp_port}/mcp"), cred).unwrap();
    let rev = remote.revision();
    let registry = HostRegistry {
        servers: BTreeMap::from([("srv".into(), rev.clone())]),
        egress_caps: BTreeMap::from([(
            profile.clone(),
            vec![HostPort {
                host: "127.0.0.1".into(),
                port: mcp_port,
            }],
        )]),
        private_egress: BTreeMap::new(),
        remote: BTreeMap::from([("srv".into(), remote)]),
    };
    let wire = Arc::new(ReqwestWire::new(Arc::new(SystemResolver)).unwrap());
    let runner = McpRunner::new(
        RunnerDeps {
            sandbox: Arc::new(UnusedSandbox),
            secrets: Arc::new(FakeSecrets::new(&[])),
            effects: Arc::new(FakeEffects::new()),
            resolver: Arc::new(FakeResolver::new(&[])),
            connector: Arc::new(TcpConnector),
            entropy: Arc::new(CountingEntropy::default()),
        },
        registry,
        RunnerConfig::default(),
    )
    .with_auth(Arc::new(auth.clone()))
    .with_wire(wire);

    let tool = Tool {
        server: "srv".into(),
        class: Risk::Read,
        external: false,
        effects: false,
        timeout_secs: Some(10),
        destination: None,
        scopes: BTreeSet::from(["srv.read".into()]),
        env: BTreeSet::new(),
        secrets: BTreeMap::new(),
        egress: BTreeSet::from([format!("127.0.0.1:{mcp_port}")]),
        input: JsonPayload(r#"{"type":"object"}"#.into()),
        description: None,
        definition: None,
        mcp_name: Some("echo".into()),
        revision: Revision(format!("r1@{rev}")),
    };
    let resolved = Resolved::new(ToolId("srv.echo".into()), tool).unwrap();
    let job = ToolJob {
        attempt: Attempt {
            profile,
            run: RunId("r1".into()),
            generation: Generation(1),
            claim: Some(ClaimEpoch(1)),
        },
        principal: PrincipalId("local:owner".into()),
        origin: RunOrigin::Message,
        call: CallId("c1".into()),
        tool: resolved,
        args: "{}".into(),
        env: Vec::new(),
        conversation: Conversation {
            kind: ConversationKind::Cli,
            id: "owner".into(),
            thread: None,
        },
        reply_to: None,
        owner: "r1.1.c1".into(),
    };
    let out = runner.run(job).await;
    mcp_task.abort();
    oauth_task.abort();
    auth.shutdown().await;
    match out {
        Ok(Ran::Done(body)) => assert!(body.0.contains("\\\"ok\\\":true"), "{}", body.0),
        other => panic!("expected Ok(Ran::Done) after 401 refresh retry, got {other:?}"),
    }
    assert_eq!(refresh_hits.load(Ordering::SeqCst), 1);
}
