//! Direct Streamable HTTP execution path for remote MCP servers with local-equivalent boundaries.
//!
//! Enforces single-target `EgressPolicy` (with per-connection vetting delegated to `RemoteWire`),
//! profile-scoped bearer headers vetted against responses, pre-call `tools.lock.yaml` definition pins,
//! rejection of `effects`/`secrets`/`env` on remote servers, and no silent retries (`Unknown` on
//! post-send disconnect or timeout because closing the socket does not prove remote termination).

use super::*;
use crate::mcp::{
    header,
    http::{Http, MODERN},
};

fn before_call(error: McpError) -> Ran {
    match error {
        McpError::Exposed => Ran::Refused("response_credential"),
        _ => Ran::NotSent,
    }
}

/// JSON-RPC / MCP error codes proving pre-execution rejection (valid only on 2xx responses matching the request ID).
fn rejected(code: i64) -> bool {
    (-32602..=-32600).contains(&code) || (-32022..=-32020).contains(&code)
}

/// Remote `_meta` carries only idempotency keys (`profile-id`, `run-id`, `call-id`) and never principal, origin, conversation, or destination.
fn meta_members(job: &ToolJob) -> String {
    format!(
        "\"bot/call-id\":{},\"bot/profile-id\":{},\"bot/run-id\":{}",
        json_quote(&job.call.0),
        json_quote(&job.attempt.profile.0),
        json_quote(&job.attempt.run.0)
    )
}

impl McpRunner {
    pub(super) async fn admit_remote_auth(
        &self,
        profile: ProfileId,
        server: String,
    ) -> Result<(), Error> {
        let oauth = self
            .host()
            .remote
            .get(&server)
            .and_then(|remote| remote.oauth.clone());
        let Some(name) = oauth else {
            return Ok(());
        };
        if crate::private::reserved_secret(&name) {
            return Err(Error::Missing);
        }
        let source = bot_core::auth::AuthSource::Credential {
            name,
            expected_kind: bot_core::auth::CredentialKind::OAuth,
        };
        match self
            .auth
            .resolve(
                profile,
                source,
                bot_core::auth::CallerSurface::RemoteMcp,
                false,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(
                bot_core::auth::AuthError::Missing
                | bot_core::auth::AuthError::NeedsLogin
                | bot_core::auth::AuthError::Denied
                | bot_core::auth::AuthError::Invalid,
            ) => Err(Error::Missing),
            Err(_) => Ok(()),
        }
    }

    async fn resolve_remote_auth(
        &self,
        profile: &ProfileId,
        remote: &Remote,
        force_refresh: bool,
    ) -> Result<(Option<String>, Vec<Vec<u8>>), Ran> {
        let Some(source) = remote.auth_source() else {
            return Ok((None, Vec::new()));
        };
        if crate::private::reserved_secret(source.credential_name()) {
            return Err(Ran::NotSent);
        }
        let Ok(resolved) = self
            .auth
            .resolve(
                profile.clone(),
                source,
                bot_core::auth::CallerSurface::RemoteMcp,
                force_refresh,
            )
            .await
        else {
            return Err(Ran::NotSent);
        };
        let Ok(text) = String::from_utf8(resolved.secret.0.clone()) else {
            return Err(Ran::NotSent);
        };
        // Reject control characters before placing the bearer directly into an HTTP header.
        if text.is_empty() || text.contains(['\0', '\r', '\n']) {
            return Err(Ran::NotSent);
        }
        let credentials = resolved.scan_values();
        Ok((Some(text), credentials))
    }

    pub(super) async fn execute_remote(
        &self,
        job: &ToolJob,
        remote: &Remote,
        budget: Budget,
    ) -> Ran {
        let tool = &job.tool.tool;
        if tool.effects || !tool.secrets.is_empty() || !tool.env.is_empty() {
            return Ran::NotSent;
        }
        let Some(wire) = self.wire.clone() else {
            return Ran::NotSent;
        };
        let permitted = {
            let host = self.host();
            let cap = host
                .egress_caps
                .get(&job.attempt.profile)
                .map(Vec::as_slice);
            EgressPolicy::new(&job.tool.egress, cap).permits(&remote.host, remote.port)
        };
        if !permitted {
            return Ran::NotSent;
        }
        let sent = AtomicBool::new(false);
        match timeout(
            budget.wall,
            self.drive_remote_with_auth(job, remote, wire, &sent, budget.call),
        )
        .await
        {
            Ok(ran) => ran,
            Err(_) if sent.load(Ordering::SeqCst) => Ran::Unknown("wall_timeout"),
            Err(_) => Ran::NotSent,
        }
    }

    async fn drive_remote_with_auth(
        &self,
        job: &ToolJob,
        remote: &Remote,
        wire: Arc<dyn RemoteWire>,
        sent: &AtomicBool,
        call_limit: Duration,
    ) -> Ran {
        let (bearer, credentials) = match self
            .resolve_remote_auth(&job.attempt.profile, remote, false)
            .await
        {
            Ok(pair) => pair,
            Err(ran) => return ran,
        };
        let mut http = Http::new(
            wire.clone(),
            remote.clone(),
            bearer,
            self.cfg.allow_private_egress,
        );
        let first = self
            .drive_remote(job, &mut http, &credentials, sent, call_limit)
            .await;
        match first {
            RemoteStep::Done(ran) => {
                http.close().await;
                ran
            }
            RemoteStep::Unauthorized => {
                let retry_oauth =
                    remote.oauth.is_some() && !sent.load(Ordering::SeqCst) && http.auth_challenge();
                http.close().await;
                if !retry_oauth {
                    return Ran::NotSent;
                }
                let (bearer, credentials) = match self
                    .resolve_remote_auth(&job.attempt.profile, remote, true)
                    .await
                {
                    Ok(pair) => pair,
                    Err(ran) => return ran,
                };
                let mut refreshed =
                    Http::new(wire, remote.clone(), bearer, self.cfg.allow_private_egress);
                let out = match self
                    .drive_remote(job, &mut refreshed, &credentials, sent, call_limit)
                    .await
                {
                    RemoteStep::Done(ran) => ran,
                    RemoteStep::Unauthorized => Ran::NotSent,
                };
                refreshed.close().await;
                out
            }
        }
    }

    async fn drive_remote(
        &self,
        job: &ToolJob,
        http: &mut Http,
        credentials: &[Vec<u8>],
        sent: &AtomicBool,
        call_limit: Duration,
    ) -> RemoteStep {
        let limit = self.cfg.handshake_timeout;
        if let Err(error) = http.connect(limit).await {
            return match error {
                McpError::Status { status: 401, .. } => RemoteStep::Unauthorized,
                other => RemoteStep::Done(before_call(other)),
            };
        }
        let Some(schema) = parse(&job.tool.tool.input.0) else {
            return RemoteStep::Done(Ran::NotSent);
        };
        let Some(declared) = canonicalize(&schema) else {
            return RemoteStep::Done(Ran::NotSent);
        };
        let entry = match http.find_tool(&job.tool.name, limit).await {
            Ok(entry) => entry,
            Err(McpError::Status { status: 401, .. }) => return RemoteStep::Unauthorized,
            Err(error) => return RemoteStep::Done(before_call(error)),
        };
        if let Err(ran) = check_definition(&job.tool.tool, entry.as_ref(), &declared) {
            return RemoteStep::Done(ran);
        }
        // Derive `Mcp-Param-*` headers from the pinned declaration schema, never from server metadata.
        let params = if http.version() == Some(MODERN) {
            let Some(args) = parse(&job.args) else {
                return RemoteStep::Done(Ran::NotSent);
            };
            match header::param_headers(&schema, &args) {
                Ok(params) => params,
                Err(_) => return RemoteStep::Done(Ran::Refused("header_annotation")),
            }
        } else {
            Vec::new()
        };
        sent.store(true, Ordering::SeqCst);
        let result = http
            .call(
                &job.tool.name,
                &job.args,
                &meta_members(job),
                params,
                call_limit,
            )
            .await;
        RemoteStep::Done(match result {
            Ok(value) => self.vet(&value, credentials),
            Err(McpError::Unreached | McpError::Stale) => Ran::NotSent,
            // Call already reached the server, so block the response (writes become held).
            Err(McpError::Exposed) => Ran::Blocked("result_credential"),
            Err(McpError::Server(code)) if rejected(code) => Ran::Rejected,
            Err(McpError::Server(_)) => Ran::Unknown("server_error"),
            // Non-2xx status (401/403/5xx) does not prove whether the server executed the call.
            Err(McpError::Status { .. }) => Ran::Unknown("http_status"),
            Err(McpError::Timeout) => Ran::Unknown("timeout"),
            Err(McpError::Closed) => Ran::Unknown("closed"),
            Err(McpError::Io) => Ran::Unknown("io"),
            Err(_) => Ran::Unknown("protocol"),
        })
    }
}

enum RemoteStep {
    Done(Ran),
    Unauthorized,
}
