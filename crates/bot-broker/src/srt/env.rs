//! Tool server environment helpers: name validation, secret carry wrapper, and path sanitization.

use super::CARRY;

pub use bot_core::profile::forbidden_env_name;

/// Builds a shell preamble moving carried secrets into declared env names; callers must pre-validate via `forbidden_env_name`.
pub(super) fn carry_script(names: &[&str]) -> String {
    let mut script = String::new();
    for (i, name) in names.iter().enumerate() {
        script.push_str(&format!(
            "{name}=\"${{{CARRY}{i}}}\"; export {name}; unset {CARRY}{i}; "
        ));
    }
    script.push_str("exec \"$@\"");
    script
}

#[cfg(test)]
pub(super) fn safe(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
