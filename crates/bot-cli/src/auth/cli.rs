//! CLI handlers for `bot auth` (set-token, set-oauth, set-service-account, import-codex, login, status, refresh, logout).

use super::{
    HostAuthResolver,
    crypto::{MasterKey, aad_bytes},
    http::AuthHttp,
    import::{CODEX_TOKEN_ENDPOINT, load_dedicated_codex_auth},
    login::run_login,
    payload::{
        BodyFormat, CredentialPayload, canonical_host_port, extra_header_value_ok,
        is_allowed_extra_header, is_claude_subscription_token, token_value_ok,
    },
    service_account::parse_service_account_json,
};
use crate::{
    cli::{AuthAction, AuthArgs},
    load::{load_host, secret_store},
    settings::Settings,
};
use bot_broker::resolve::HostPort;
use bot_core::{
    active::ActiveCheck,
    auth::{CallerSurface, CredentialKind, CredentialStore, CredentialUpsert},
    id::{ProfileId, SecretName},
    port::Reply,
};
use bot_gateway::cli::socket::current_uid;
use bot_store::{Config, Store};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{IsTerminal, Read},
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

struct AlwaysActive;
impl ActiveCheck for AlwaysActive {
    fn verify(&self) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

pub async fn run(settings: &Settings, args: AuthArgs) -> Result<(), String> {
    let url = settings
        .database_url
        .as_deref()
        .ok_or("BOT_DATABASE_URL is required for `bot auth`")?;
    let store = Store::connect(url, Config::default())
        .await
        .map_err(|_| "failed to connect to PostgreSQL store".to_owned())?;
    store
        .migrate()
        .await
        .map_err(|_| "failed to run store migrations".to_owned())?;
    let store = Arc::new(store);
    let actor = format!(
        "cli:uid:{}",
        current_uid().map_err(|_| "cannot determine current uid".to_owned())?
    );

    match args.action {
        AuthAction::Status { profile, json } => status(store.as_ref(), profile, json).await,
        AuthAction::Logout { profile, name } => {
            let (profile, name) = parse_ids(&profile, &name)?;
            let deleted = store
                .delete_credential(actor, profile.clone(), name.clone())
                .await
                .map_err(|e| format!("failed to remove credential: {e:?}"))?;
            if deleted {
                println!("Removed credential {}/{}", profile.0, name.as_str());
            } else {
                println!("No credential found for {}/{}", profile.0, name.as_str());
            }
            Ok(())
        }
        AuthAction::SetToken {
            profile,
            name,
            provider,
        } => {
            let (profile, name) = parse_ids(&profile, &name)?;
            if provider != "claude" {
                return Err(format!(
                    "unsupported subscription token provider `{provider}` (expected `claude`)"
                ));
            }
            let raw = read_non_tty_stdin("setup-token")?;
            let token = raw.trim().to_owned();
            if !is_claude_subscription_token(&token) {
                return Err(
                    "invalid Claude setup-token: expected `sk-ant-oat...` from `claude setup-token`"
                        .to_owned(),
                );
            }
            let payload = CredentialPayload::SubscriptionToken { provider, token };
            persist(
                settings,
                store.as_ref(),
                actor,
                profile,
                name,
                &payload,
                false,
            )
            .await?;
            println!("Stored subscription token credential.");
            Ok(())
        }
        AuthAction::SetOauth {
            profile,
            name,
            token_endpoint,
            client_id,
            scope,
            header,
            json_body,
        } => {
            let (profile, name) = parse_ids(&profile, &name)?;
            let http = AuthHttp::new()?;
            let egress = load_profile_egress(settings, &profile)?;
            http.vet_url(&token_endpoint, egress.as_deref())?;
            let headers = parse_extra_headers(&header)?;
            let raw = read_non_tty_stdin("OAuth JSON bundle")?;
            let doc: Value = serde_json::from_str(&raw)
                .map_err(|_| "stdin must be a JSON object with `access_token`".to_owned())?;
            let access_token = doc
                .get("access_token")
                .and_then(Value::as_str)
                .filter(|t| token_value_ok(t))
                .ok_or("JSON bundle missing valid `access_token`")?
                .to_owned();
            let refresh_token = doc
                .get("refresh_token")
                .and_then(Value::as_str)
                .filter(|t| token_value_ok(t))
                .map(str::to_owned);
            let client_secret = doc
                .get("client_secret")
                .and_then(Value::as_str)
                .filter(|t| token_value_ok(t))
                .map(str::to_owned);
            let expires_ms = doc
                .get("expires_in")
                .and_then(Value::as_i64)
                .map(|secs| now_ms().saturating_add(secs.clamp(60, 86_400 * 30) * 1000));
            let payload = CredentialPayload::OAuth {
                token_endpoint,
                client_id,
                client_secret,
                access_token,
                refresh_token,
                expires_ms,
                scope,
                headers,
                body_format: if json_body {
                    BodyFormat::Json
                } else {
                    BodyFormat::Form
                },
                subscription_provider: None,
                resource: None,
                issuer: None,
            };
            persist(
                settings,
                store.as_ref(),
                actor,
                profile,
                name,
                &payload,
                false,
            )
            .await?;
            println!("Stored OAuth credential.");
            Ok(())
        }
        AuthAction::SetServiceAccount {
            profile,
            name,
            file,
            scope,
        } => {
            let (profile, name) = parse_ids(&profile, &name)?;
            let raw = std::fs::read_to_string(&file)
                .map_err(|_| format!("cannot read service account file {}", file.display()))?;
            let sa = parse_service_account_json(&raw, false)?;
            let http = AuthHttp::new()?;
            let egress = load_profile_egress(settings, &profile)?;
            http.vet_url(&sa.token_uri, egress.as_deref())?;
            let payload = CredentialPayload::ServiceAccount {
                client_email: sa.client_email,
                private_key_pem: sa.private_key_pem,
                token_uri: sa.token_uri,
                scope,
                access_token: None,
                expires_ms: None,
            };
            persist(
                settings,
                store.as_ref(),
                actor,
                profile,
                name,
                &payload,
                false,
            )
            .await?;
            println!("Stored Google service-account credential.");
            Ok(())
        }
        AuthAction::ImportCodex {
            profile,
            name,
            from,
            keep_source,
        } => {
            let (profile, name) = parse_ids(&profile, &name)?;
            let http = AuthHttp::new()?;
            let egress = load_profile_egress(settings, &profile)?;
            http.vet_url(CODEX_TOKEN_ENDPOINT, egress.as_deref())?;
            let uid = current_uid().map_err(|_| "cannot determine current uid".to_owned())?;
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
            let payload = load_dedicated_codex_auth(
                &from,
                CODEX_TOKEN_ENDPOINT,
                home.as_deref(),
                codex_home.as_deref(),
                uid,
            )?;
            persist(
                settings,
                store.as_ref(),
                actor,
                profile,
                name,
                &payload,
                false,
            )
            .await?;
            if !keep_source {
                std::fs::remove_file(&from).map_err(|_| {
                    format!(
                        "imported credential into store, but failed to delete source file {} — delete it manually to prevent rotating refresh-token races",
                        from.display()
                    )
                })?;
                println!("Imported dedicated Codex OAuth credential and deleted source file.");
            } else {
                println!("Imported dedicated Codex OAuth credential (source file kept).");
            }
            Ok(())
        }
        AuthAction::Login {
            profile,
            server,
            name,
            authorize_url,
            token_url,
            client_id,
            scope,
            paste,
        } => {
            run_login(
                settings,
                store.as_ref(),
                actor,
                profile,
                server,
                name,
                authorize_url,
                token_url,
                client_id,
                scope,
                paste,
            )
            .await
        }
        AuthAction::Refresh { profile, name } => {
            let (profile, name) = parse_ids(&profile, &name)?;
            let row = store
                .get_credential(profile.clone(), name.clone())
                .await
                .map_err(|e| format!("failed to load credential: {e:?}"))?
                .ok_or_else(|| format!("credential {}/{} not found", profile.0, name.as_str()))?;
            let uid = current_uid().map_err(|_| "cannot determine current uid".to_owned())?;
            let host = load_host(settings, uid)?;
            let secrets = secret_store(settings)?;
            let egress = host_egress_map(&host)?;
            let allow_sub = host
                .allow_subscription_auth
                .iter()
                .map(|(p, v)| (ProfileId(p.clone()), v.clone()))
                .collect();
            let resolver = HostAuthResolver::new(
                secrets,
                Some(store.clone() as Arc<dyn CredentialStore>),
                Arc::new(AlwaysActive),
                AuthHttp::new()?,
                egress,
                allow_sub,
                Arc::new(|_| {}),
            );
            let surface = match row.kind {
                CredentialKind::SubscriptionToken => {
                    CallerSurface::CliRelay(bot_core::auth::CliVendor::Claude)
                }
                CredentialKind::ServiceAccount => CallerSurface::DirectApi,
                CredentialKind::OAuth => {
                    if host
                        .allow_subscription_auth
                        .get(&profile.0)
                        .is_some_and(|s| s.contains("codex"))
                    {
                        CallerSurface::CliRelay(bot_core::auth::CliVendor::Codex)
                    } else {
                        CallerSurface::RemoteMcp
                    }
                }
            };
            resolver
                .resolve_managed(profile.clone(), name.clone(), row.kind, surface, true)
                .await
                .map_err(|e| format!("refresh failed: {}", e.code()))?;
            resolver.shutdown().await;
            println!("Refreshed credential {}/{}.", profile.0, name.as_str());
            Ok(())
        }
    }
}

async fn status(
    store: &dyn CredentialStore,
    profile: Option<String>,
    as_json: bool,
) -> Result<(), String> {
    let filter = match profile {
        Some(p) => {
            if !bot_core::id::is_name(&p) {
                return Err(format!("invalid profile name `{p}`"));
            }
            Some(ProfileId(p))
        }
        None => None,
    };
    let rows = store
        .list_credentials(filter)
        .await
        .map_err(|e| format!("failed to list credentials: {e:?}"))?;
    if as_json {
        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "profile": r.profile.0,
                    "name": r.name.as_str(),
                    "kind": r.kind.config_str(),
                    "token_endpoint_host": r.token_endpoint_host,
                    "version": r.version,
                    "state": r.state.as_str(),
                    "expires_ms": r.expires_ms,
                    "updated_ms": r.updated_ms,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_owned())
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("No managed credentials stored.");
        return Ok(());
    }
    let now = now_ms();
    for r in rows {
        let expiry = match r.expires_ms {
            None => "no-expiry".to_owned(),
            Some(exp) if exp <= now => "expired".to_owned(),
            Some(exp) => format!("expires in {}m", (exp - now) / 60_000),
        };
        let host = if r.token_endpoint_host.is_empty() {
            "-"
        } else {
            &r.token_endpoint_host
        };
        println!(
            "{}/{} · {} · state={} · v{} · {} · {}",
            r.profile.0,
            r.name.as_str(),
            r.kind.config_str(),
            r.state.as_str(),
            r.version,
            expiry,
            host
        );
    }
    Ok(())
}

pub async fn persist(
    settings: &Settings,
    store: &dyn CredentialStore,
    actor: String,
    profile: ProfileId,
    name: SecretName,
    payload: &CredentialPayload,
    allow_loopback_http: bool,
) -> Result<i64, String> {
    payload
        .validate()
        .map_err(|why| format!("invalid credential payload: {why}"))?;
    let secrets = secret_store(settings)?;
    let master = MasterKey::load(secrets.as_ref()).await.map_err(|_| {
        "missing or invalid host credential encryption key (`host` / `credential-key`, 64 hex chars)"
            .to_owned()
    })?;
    let host_port = match payload.token_endpoint() {
        Some(url) => canonical_host_port(url, allow_loopback_http)?,
        None => String::new(),
    };
    // Refuse silent rebinding of an existing credential's endpoint host or OAuth issuer; require explicit `bot auth logout`.
    if let Ok(Some(existing)) = store.get_credential(profile.clone(), name.clone()).await {
        if !existing.token_endpoint_host.is_empty() && existing.token_endpoint_host != host_port {
            return Err(format!(
                "credential {}/{} is already bound to `{}`; run `bot auth logout` first before rebinding to `{host_port}`",
                profile.0,
                name.as_str(),
                existing.token_endpoint_host
            ));
        }
        let old_aad = aad_bytes(
            &existing.profile,
            &existing.name,
            existing.kind,
            &existing.token_endpoint_host,
        );
        if let Ok(old_plain) = master.open(&old_aad, &existing.nonce, &existing.ciphertext)
            && let Ok(CredentialPayload::OAuth {
                issuer: Some(old_issuer),
                ..
            }) = serde_json::from_slice::<CredentialPayload>(&old_plain)
            && let CredentialPayload::OAuth {
                issuer: new_issuer, ..
            } = payload
            && new_issuer.as_deref() != Some(old_issuer.as_str())
        {
            return Err(format!(
                "credential {}/{} is already bound to issuer `{old_issuer}`; run `bot auth logout` first before rebinding",
                profile.0,
                name.as_str()
            ));
        }
    }
    let aad = aad_bytes(&profile, &name, payload.kind(), &host_port);
    let plain = serde_json::to_vec(payload).map_err(|_| "failed to encode payload".to_owned())?;
    let (nonce, ciphertext) = master
        .seal(&aad, &plain)
        .map_err(|_| "failed to encrypt credential payload".to_owned())?;
    store
        .upsert_credential(
            actor,
            CredentialUpsert {
                profile,
                name,
                kind: payload.kind(),
                token_endpoint_host: host_port,
                nonce,
                ciphertext,
                expires_ms: payload.expires_ms(),
            },
        )
        .await
        .map_err(|e| format!("failed to persist credential: {e:?}"))
}

pub(super) fn parse_ids(profile: &str, name: &str) -> Result<(ProfileId, SecretName), String> {
    if !bot_core::id::is_name(profile) {
        return Err(format!("invalid profile name `{profile}`"));
    }
    let secret = SecretName::parse(name)
        .filter(|n| !bot_broker::private::reserved_secret(n))
        .ok_or_else(|| format!("invalid or reserved credential name `{name}`"))?;
    Ok((ProfileId(profile.to_owned()), secret))
}

fn parse_extra_headers(raw: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for item in raw {
        let Some((k, v)) = item.split_once('=') else {
            return Err(format!(
                "invalid `--header` `{item}` (expected `Name=Value`)"
            ));
        };
        let lower = k.trim().to_ascii_lowercase();
        let val = v.trim();
        if !is_allowed_extra_header(&lower) {
            return Err(format!(
                "disallowed header `{lower}` (allowed: {})",
                super::payload::ALLOWED_EXTRA_HEADERS.join(", ")
            ));
        }
        if !extra_header_value_ok(val) {
            return Err(format!("invalid value for header `{lower}`"));
        }
        out.push((lower, val.to_owned()));
    }
    Ok(out)
}

fn read_non_tty_stdin(label: &str) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        return Err(format!(
            "refusing to read {label} from an interactive TTY — pipe it via stdin"
        ));
    }
    let mut buf = String::new();
    std::io::stdin()
        .take(128 * 1024)
        .read_to_string(&mut buf)
        .map_err(|_| format!("failed to read {label} from stdin"))?;
    if buf.trim().is_empty() {
        return Err(format!("empty {label} on stdin"));
    }
    Ok(buf)
}

pub(super) fn load_profile_egress(
    settings: &Settings,
    profile: &ProfileId,
) -> Result<Option<Vec<HostPort>>, String> {
    let Some(_) = settings.host_config else {
        return Ok(None);
    };
    let uid = current_uid().map_err(|_| "cannot determine current uid".to_owned())?;
    let host = load_host(settings, uid)?;
    let map = host_egress_map(&host)?;
    Ok(map.get(profile).cloned())
}

pub fn host_egress_map(
    host: &crate::host::HostFile,
) -> Result<BTreeMap<ProfileId, Vec<HostPort>>, String> {
    host.catalog
        .egress
        .iter()
        .map(|(profile, targets)| {
            let ports = targets
                .iter()
                .map(|target| {
                    HostPort::parse(target)
                        .ok_or_else(|| format!("egress.{profile}: cannot parse {target}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((ProfileId(profile.clone()), ports))
        })
        .collect()
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
