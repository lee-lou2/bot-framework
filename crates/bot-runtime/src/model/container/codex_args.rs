//! Strict validation of Codex CLI `-c`/`--config` relay overrides before launching a model VM.

fn field_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Requires explicit `model_provider="relay"`, `model_providers.relay.base_url`, and `mcp_servers.relay.url`
/// matching `canonical`, while rejecting table/array syntax, whitespace around `=`, and foreign providers/MCPs.
pub(super) fn codex_relay_args_ok(args: &[String], canonical: &str) -> bool {
    let want_api = format!("model_providers.relay.base_url=\"{canonical}/v1\"");
    let want_mcp = format!("mcp_servers.relay.url=\"{canonical}/mcp\"");
    let (mut seen_provider, mut seen_api, mut seen_mcp) = (false, false, false);
    let mut check = |entry: &str| -> bool {
        let Some((raw_key, raw_val)) = entry.split_once('=') else {
            return false;
        };
        let touches_provider = raw_key.contains("model_provider");
        let touches_mcp = raw_key.contains("mcp_servers");
        if !touches_provider && !touches_mcp {
            return true;
        }
        // Reject whitespace around `=`, quoted/nested keys, and inline table/array syntax.
        if raw_key != raw_key.trim()
            || raw_val != raw_val.trim()
            || raw_key.chars().any(char::is_whitespace)
            || raw_val.starts_with('{')
            || raw_val.starts_with('[')
        {
            return false;
        }
        if raw_key == "model_provider" {
            if entry != "model_provider=\"relay\"" {
                return false;
            }
            seen_provider = true;
            return true;
        }
        let mut parts = raw_key.split('.');
        let (Some(root), Some(name), Some(field), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        if name != "relay" || !field_ok(field) {
            return false;
        }
        match (root, field) {
            ("model_providers", "base_url") => {
                if entry != want_api {
                    return false;
                }
                seen_api = true;
                true
            }
            ("model_providers", _) => true,
            ("mcp_servers", "url") => {
                if entry != want_mcp {
                    return false;
                }
                seen_mcp = true;
                true
            }
            ("mcp_servers", "command" | "args") => false,
            ("mcp_servers", _) => true,
            _ => false,
        }
    };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            break;
        }
        if arg == "-c" || arg == "--config" {
            let Some(val) = args.get(i + 1) else {
                return false;
            };
            if !check(val) {
                return false;
            }
            i += 2;
            continue;
        }
        if let Some(val) = arg
            .strip_prefix("--config=")
            .or_else(|| arg.strip_prefix("-c="))
            .or_else(|| arg.strip_prefix("-c"))
            && !check(val)
        {
            return false;
        }
        i += 1;
    }
    seen_provider && seen_api && seen_mcp
}
