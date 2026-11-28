//! Hardened import of a dedicated Codex CLI login (`auth.json`) into host-managed OAuth storage.

use super::payload::{BodyFormat, CredentialPayload, token_value_ok};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const CODEX_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const MAX_IMPORT_BYTES: u64 = 128 * 1024;

/// Extracts the client_id (`aud`) from an unverified JWT payload (`string` or 1-element string array).
pub fn extract_codex_client_id(id_token: &str) -> Result<String, String> {
    let mut parts = id_token.split('.');
    let _header = parts.next().ok_or("id_token is not a JWT")?;
    let payload_b64 = parts.next().ok_or("id_token is missing JWT payload")?;
    let _sig = parts.next().ok_or("id_token is missing JWT signature")?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload_b64.as_bytes())
        .map_err(|_| "id_token JWT payload is not valid base64url".to_owned())?;
    let claims: Value = serde_json::from_slice(&decoded)
        .map_err(|_| "id_token JWT payload is not valid JSON".to_owned())?;
    let aud = match claims.get("aud") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Array(items)) if items.len() == 1 => items[0]
            .as_str()
            .ok_or("id_token `aud` entry must be a string")?,
        _ => {
            return Err(
                "id_token must contain a single `aud` client_id string (or 1-element array)"
                    .to_owned(),
            );
        }
    };
    if !aud.starts_with("app_") || !token_value_ok(aud) {
        return Err("id_token `aud` is not a valid Codex OAuth client_id (`app_...`)".to_owned());
    }
    Ok(aud.to_owned())
}

fn live_codex_auth_paths(home: Option<&Path>, codex_home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = home {
        out.push(h.join(".codex/auth.json"));
    }
    if let Some(ch) = codex_home {
        out.push(ch.join("auth.json"));
    }
    out
}

/// Validates and reads a dedicated Codex `auth.json` file without touching the user's live CLI login.
pub fn load_dedicated_codex_auth(
    source: &Path,
    token_endpoint: &str,
    home: Option<&Path>,
    codex_home: Option<&Path>,
    current_uid: u32,
) -> Result<CredentialPayload, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(source)
        .map_err(|_| {
            format!(
                "cannot open {} (must be a non-symlink regular file)",
                source.display()
            )
        })?;
    let meta = file
        .metadata()
        .map_err(|_| "failed to inspect source file metadata".to_owned())?;
    if !meta.is_file() {
        return Err("source path must be a regular file".to_owned());
    }
    if meta.len() > MAX_IMPORT_BYTES {
        return Err("source auth.json exceeds maximum size (128 KiB)".to_owned());
    }
    if meta.nlink() != 1 {
        return Err("refusing to import a hard-linked auth.json (nlink must be 1)".to_owned());
    }
    if meta.uid() != current_uid {
        return Err("refusing to import auth.json not owned by the current user".to_owned());
    }
    if meta.mode() & 0o077 != 0 {
        return Err(
            "refusing to import auth.json with group/other permissions (run `chmod 600`)"
                .to_owned(),
        );
    }
    let canonical = std::fs::canonicalize(source)
        .map_err(|_| "cannot resolve canonical path of source auth.json".to_owned())?;
    for live in live_codex_auth_paths(home, codex_home) {
        if let Ok(live_canon) = std::fs::canonicalize(&live)
            && live_canon == canonical
        {
            return Err(
                "refusing to import live Codex CLI auth.json — rotating refresh tokens would break interactive Codex sessions; run `CODEX_HOME=<dedicated-dir> codex login --device-auth` first"
                    .to_owned(),
            );
        }
        if let Ok(live_meta) = std::fs::metadata(&live)
            && live_meta.dev() == meta.dev()
            && live_meta.ino() == meta.ino()
        {
            return Err(
                "refusing to import file pointing to the same inode as live Codex CLI auth.json"
                    .to_owned(),
            );
        }
    }
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|_| "failed to read source auth.json as UTF-8".to_owned())?;
    parse_codex_auth_json(&raw, token_endpoint)
}

/// Extracts `exp` (epoch ms) from an unverified JWT payload when present.
pub fn extract_jwt_exp_ms(token: &str) -> Option<i64> {
    let payload_b64 = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload_b64.as_bytes()).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    let exp_secs = claims.get("exp").and_then(Value::as_i64)?;
    (exp_secs > 0).then(|| exp_secs.saturating_mul(1000))
}

/// Parses Codex `auth.json` tokens (discarding `id_token` and `OPENAI_API_KEY` after extracting `client_id`).
pub fn parse_codex_auth_json(raw: &str, token_endpoint: &str) -> Result<CredentialPayload, String> {
    let doc: Value =
        serde_json::from_str(raw).map_err(|_| "source file is not valid JSON".to_owned())?;
    let tokens = doc
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or("source auth.json is missing `tokens` object (API-key-only auth.json is not an OAuth login)")?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| token_value_ok(t))
        .ok_or("source auth.json is missing valid `tokens.access_token`")?
        .to_owned();
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|t| token_value_ok(t))
        .ok_or("source auth.json is missing valid `tokens.refresh_token`")?
        .to_owned();
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && super::payload::extra_header_value_ok(s))
        .ok_or("source auth.json is missing `tokens.account_id`")?
        .to_owned();
    let id_token = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .ok_or("source auth.json is missing `tokens.id_token` needed to read `client_id`")?;
    let client_id = extract_codex_client_id(id_token)?;
    // Use JWT `exp` when present; fall back to `0` so the first resolve refreshes and captures a real TTL.
    let expires_ms = Some(extract_jwt_exp_ms(&access_token).unwrap_or(0));
    Ok(CredentialPayload::OAuth {
        token_endpoint: token_endpoint.to_owned(),
        client_id,
        client_secret: None,
        access_token,
        refresh_token: Some(refresh_token),
        expires_ms,
        scope: None,
        headers: vec![("chatgpt-account-id".to_owned(), account_id)],
        body_format: BodyFormat::Json,
        subscription_provider: Some("codex".to_owned()),
        resource: None,
        issuer: Some("https://auth.openai.com".to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn fake_jwt(aud: Value) -> String {
        let h = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#);
        let p = URL_SAFE_NO_PAD.encode(json!({"aud": aud, "sub": "user-1"}).to_string());
        format!("{h}.{p}.sig")
    }

    fn write_0600(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn extracts_client_id_from_string_and_single_element_array_aud() {
        assert_eq!(
            extract_codex_client_id(&fake_jwt(json!("app_test123"))).unwrap(),
            "app_test123"
        );
        assert_eq!(
            extract_codex_client_id(&fake_jwt(json!(["app_test456"]))).unwrap(),
            "app_test456"
        );
        assert!(extract_codex_client_id(&fake_jwt(json!(["app_1", "app_2"]))).is_err());
        assert!(extract_codex_client_id(&fake_jwt(json!("not-app"))).is_err());
    }

    #[test]
    fn refuses_live_codex_path_symlinks_loose_perms_and_imports_dedicated_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let live_dir = home.join(".codex");
        let dedicated_dir = dir.path().join("dedicated");
        std::fs::create_dir_all(&live_dir).unwrap();
        std::fs::create_dir_all(&dedicated_dir).unwrap();

        let content = json!({
            "OPENAI_API_KEY": "sk-should-be-dropped",
            "tokens": {
                "access_token": "ey-access-1",
                "refresh_token": "rt-refresh-1",
                "account_id": "acct-99",
                "id_token": fake_jwt(json!("app_codex_client"))
            }
        })
        .to_string();
        let live_file = live_dir.join("auth.json");
        write_0600(&live_file, &content);
        let uid = bot_gateway::cli::socket::current_uid().unwrap();

        // Refuses ~/.codex/auth.json
        assert!(
            load_dedicated_codex_auth(&live_file, CODEX_TOKEN_ENDPOINT, Some(&home), None, uid)
                .unwrap_err()
                .contains("live Codex CLI")
        );

        // Refuses loose permissions
        let dedicated = dedicated_dir.join("auth.json");
        std::fs::write(&dedicated, &content).unwrap();
        std::fs::set_permissions(&dedicated, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            load_dedicated_codex_auth(&dedicated, CODEX_TOKEN_ENDPOINT, Some(&home), None, uid)
                .unwrap_err()
                .contains("chmod 600")
        );

        // Refuses symlink
        let link = dedicated_dir.join("link.json");
        std::os::unix::fs::symlink(&dedicated, &link).unwrap();
        assert!(
            load_dedicated_codex_auth(&link, CODEX_TOKEN_ENDPOINT, Some(&home), None, uid).is_err()
        );

        // Succeeds on dedicated 0600 regular file and drops id_token & OPENAI_API_KEY.
        std::fs::set_permissions(&dedicated, std::fs::Permissions::from_mode(0o600)).unwrap();
        let payload =
            load_dedicated_codex_auth(&dedicated, CODEX_TOKEN_ENDPOINT, Some(&home), None, uid)
                .unwrap();
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("sk-should-be-dropped"));
        assert!(!serialized.contains("eyJ"));
        match payload {
            CredentialPayload::OAuth {
                client_id,
                headers,
                body_format,
                subscription_provider,
                ..
            } => {
                assert_eq!(client_id, "app_codex_client");
                assert_eq!(
                    headers,
                    vec![("chatgpt-account-id".into(), "acct-99".into())]
                );
                assert_eq!(body_format, BodyFormat::Json);
                assert_eq!(subscription_provider.as_deref(), Some("codex"));
            }
            _ => panic!("expected OAuth payload"),
        }
    }
}
