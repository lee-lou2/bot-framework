//! Forbidden tool and secret environment variable names shared by profile validation and broker execution.

pub fn forbidden_env_name(name: &str) -> bool {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    const EXACT: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "IFS",
        "ENV",
        "BASH_ENV",
        "SHELLOPTS",
        "BASHOPTS",
        "PS4",
        "PWD",
        "OLDPWD",
        "NODE_OPTIONS",
        "NODE_PATH",
        "NODE_EXTRA_CA_CERTS",
        "SANDBOX_RUNTIME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
        "NODE_TLS_REJECT_UNAUTHORIZED",
        "NODE_USE_ENV_PROXY",
        "BUN_OPTIONS",
        "ZDOTDIR",
        "_",
    ];
    const PREFIX: &[&str] = &[
        "LD_",
        "DYLD_",
        "BASH_FUNC_",
        "SRT_",
        "CLAUDE_",
        "BOT_",
        "NPM_CONFIG_",
        "PYTHON",
        "RUBY",
        "PERL",
        "GIT_",
        "BUNDLE_",
        "JAVA_",
        "JDK_",
        "_JAVA_",
    ];
    !valid
        || EXACT.contains(&name)
        || PREFIX.iter().any(|p| name.starts_with(p))
        || name.ends_with("_PROXY")
}

#[cfg(test)]
mod tests {
    use super::forbidden_env_name;

    #[test]
    fn profile_settings_cannot_replace_host_transport_or_interpreter_settings() {
        for name in [
            "PATH",
            "LD_PRELOAD",
            "BOT_STATE_DIR",
            "ALL_PROXY",
            "PYTHONPATH",
            "GIT_CONFIG_GLOBAL",
            "NODE_OPTIONS",
            "SSL_CERT_FILE",
            "lowercase",
        ] {
            assert!(forbidden_env_name(name), "{name}");
        }
        assert!(!forbidden_env_name("CALENDAR_NAME"));
        assert!(!forbidden_env_name("SERVICE_PORT"));
    }
}
