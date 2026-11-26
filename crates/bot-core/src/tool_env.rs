//! 도구 설정과 자격이 쓸 수 없는 환경 변수 이름(로더·셸·샌드박스·프록시)의 공통 판정. 프로필 검증(`bot-profile`)과 도구 실행(`bot-broker`)이 같은 규칙을 쓰도록
//! `bot_core::profile::forbidden_env_name`으로 내보낸다.

/// 도구 설정과 자격에서 공통으로 차단하는 로더·셸·샌드박스·프록시 환경 이름.
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
