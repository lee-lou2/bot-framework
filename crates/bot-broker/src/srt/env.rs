//! 도구 서버로 넘기는 환경: 이름 검사, 운반 변수 스크립트, 경로 이름 다듬기.

use super::CARRY;

pub use bot_core::profile::forbidden_env_name;

/// 자식 안에서 자격을 선언한 이름으로 옮겨 다는 셸 문장. 이름은 `forbidden_env_name`을 통과한 것만 온다.
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

/// 경로 이름에 쓸 수 있게 다듬는다.
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
