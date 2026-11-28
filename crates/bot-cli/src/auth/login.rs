//! Interactive PKCE OAuth login flow (`bot auth login`) supporting RFC 9728/8414/7591 and loopback or paste callbacks.

use super::{
    cli::{load_profile_egress, now_ms, parse_ids, persist},
    http::AuthHttp,
    oauth,
    payload::{BodyFormat, CredentialPayload},
};
use crate::{load::load_host, settings::Settings};
use bot_core::{
    auth::CredentialStore,
    id::{ProfileId, SecretName},
};
use bot_gateway::cli::socket::current_uid;
use std::time::Duration;
use tokio::net::TcpListener;

#[allow(clippy::too_many_arguments)]
pub async fn run_login(
    settings: &Settings,
    store: &dyn CredentialStore,
    actor: String,
    profile_raw: String,
    server: Option<String>,
    name_raw: Option<String>,
    authorize_url: Option<String>,
    token_url: Option<String>,
    client_id_arg: Option<String>,
    scope: Option<String>,
    paste: bool,
) -> Result<(), String> {
    let uid = current_uid().map_err(|_| "cannot determine current uid".to_owned())?;
    let host = load_host(settings, uid)?;
    let http = AuthHttp::new()?;
    let egress = load_profile_egress(settings, &ProfileId(profile_raw.clone()))?;

    let (cred_name, auth_ep, token_ep, reg_ep, resource, issuer) = if let Some(alias) = server {
        let remote = host.remote_servers.get(&alias).ok_or_else(|| {
            format!("remote HTTP server `{alias}` is not configured in host.json")
        })?;
        let cred = match name_raw {
            Some(n) => SecretName::parse(&n).ok_or("invalid credential name")?,
            None => remote.oauth.clone().ok_or_else(|| {
                format!("servers.{alias} does not configure `auth.kind = \"oauth\"`; pass `--name`")
            })?,
        };
        let discovered = oauth::discover_mcp_oauth(&http, remote, egress.as_deref()).await?;
        if let Some(pinned) = &remote.oauth_issuer
            && discovered.issuer.trim_end_matches('/') != pinned.as_str()
        {
            return Err(format!(
                "discovered OAuth issuer `{}` does not match pinned servers.{alias}.auth.issuer `{pinned}`",
                discovered.issuer
            ));
        }
        (
            cred,
            discovered.authorization_endpoint,
            discovered.token_endpoint,
            discovered.registration_endpoint,
            discovered.resource,
            Some(discovered.issuer),
        )
    } else {
        let cred = name_raw
            .as_deref()
            .and_then(SecretName::parse)
            .ok_or("pass `--server <alias>` or `--name <credential>` with `--authorize-url` and `--token-url`")?;
        let auth_ep = authorize_url.ok_or("`--authorize-url` is required without `--server`")?;
        let token_ep = token_url.ok_or("`--token-url` is required without `--server`")?;
        http.vet_url(&auth_ep, egress.as_deref())?;
        http.vet_url(&token_ep, egress.as_deref())?;
        (cred, auth_ep, token_ep, None, None, None)
    };
    let (profile, name) = parse_ids(&profile_raw, cred_name.as_str())?;
    let (listener, redirect_uri) = if paste {
        (None, "http://127.0.0.1:1455/callback".to_owned())
    } else {
        let l = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "failed to bind loopback OAuth callback listener".to_owned())?;
        let port = l
            .local_addr()
            .map_err(|_| "failed to read loopback listener port".to_owned())?
            .port();
        (Some(l), format!("http://127.0.0.1:{port}/callback"))
    };
    let (client_id, client_secret) = match client_id_arg {
        Some(id) => (id, None),
        None => {
            let reg = reg_ep.as_deref().ok_or(
                "authorization server does not support dynamic client registration; pass `--client-id`",
            )?;
            oauth::register_dynamic_client(&http, reg, &redirect_uri, egress.as_deref()).await?
        }
    };
    let pkce = oauth::pkce_pair()?;
    let state = oauth::random_state()?;
    let url = oauth::build_authorize_url(
        &auth_ep,
        &client_id,
        &redirect_uri,
        scope.as_deref(),
        resource.as_deref(),
        &state,
        &pkce.challenge,
    );
    println!("Open this URL in your browser to authorize:\n\n  {url}\n");
    let code = match listener {
        Some(l) => {
            println!("Waiting for callback on {redirect_uri} (timeout 180s)...");
            oauth::await_loopback_callback(l, &state, Duration::from_secs(180)).await?
        }
        None => {
            eprint!("Paste the full redirect URL and press Enter: ");
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|_| "failed to read pasted callback URL".to_owned())?;
            oauth::parse_pasted_callback(&line, &state)?
        }
    };
    let tokens = oauth::exchange_code(
        &http,
        &token_ep,
        &client_id,
        client_secret.as_deref(),
        &code,
        &redirect_uri,
        &pkce.verifier,
        resource.as_deref(),
        egress.as_deref(),
    )
    .await?;
    let expires_ms = tokens
        .expires_in_secs
        .map(|s| now_ms().saturating_add(s * 1000));
    let payload = CredentialPayload::OAuth {
        token_endpoint: token_ep,
        client_id,
        client_secret,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_ms,
        scope: tokens.scope.or(scope),
        headers: Vec::new(),
        body_format: BodyFormat::Form,
        subscription_provider: None,
        resource,
        issuer,
    };
    persist(
        settings,
        store,
        actor,
        profile.clone(),
        name.clone(),
        &payload,
        false,
    )
    .await?;
    println!("Stored OAuth credential {}/{}.", profile.0, name.as_str());
    Ok(())
}
