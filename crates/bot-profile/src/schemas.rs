//! 번들 스키마(`schemas/v1`: profile·tools·triggers·env·agent). 바이너리에 넣어 두고 처음 쓸 때 한 번만 컴파일한다.
use crate::schema::Schema;
use serde_json::Value;
use std::sync::OnceLock;

const PROFILE: &str = include_str!("../../../schemas/v1/profile.schema.json");
const TOOLS: &str = include_str!("../../../schemas/v1/tools.schema.json");
const TRIGGERS: &str = include_str!("../../../schemas/v1/triggers.schema.json");
const ENV: &str = include_str!("../../../schemas/v1/env.schema.json");
const AGENT: &str = include_str!("../../../schemas/v1/agent.schema.json");

pub struct Bundled {
    pub profile: Schema,
    pub tools: Schema,
    pub triggers: Schema,
    pub env: Schema,
    /// AGENT.md 맨 위 front matter
    pub agent: Schema,
}

fn load(name: &str, text: &str) -> Result<Schema, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("{name}: {e}"))?;
    Schema::compile_bundled(doc).map_err(|e| format!("{name}: {}", e.join("; ")))
}

fn build() -> Result<Bundled, String> {
    Ok(Bundled {
        profile: load("profile", PROFILE)?,
        tools: load("tools", TOOLS)?,
        triggers: load("triggers", TRIGGERS)?,
        env: load("env", ENV)?,
        agent: load("agent", AGENT)?,
    })
}

/// 번들 스키마가 컴파일되는지는 시험이 보증한다.
#[allow(clippy::missing_panics_doc)]
pub fn bundled() -> &'static Bundled {
    static CELL: OnceLock<Result<Bundled, String>> = OnceLock::new();
    match CELL.get_or_init(build) {
        Ok(b) => b,
        Err(e) => panic!("번들 스키마가 올바르지 않아요: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_schemas_compile() {
        let b = bundled();
        assert!(b.profile.doc().is_object() && b.tools.doc().is_object());
        assert!(b.triggers.doc().is_object() && b.env.doc().is_object());
        assert!(b.agent.doc().is_object());
    }

    #[test]
    fn random_values_never_panic_in_bundled_schemas() {
        use crate::{
            limits::Limits,
            testrand::{Rng, yaml_soup},
            yaml,
        };
        let (b, limits) = (bundled(), Limits::default());
        let mut rng = Rng::new(0xfeed_beef);
        let mut checked = 0;
        for _ in 0..30_000 {
            let text = yaml_soup(&mut rng);
            if let Ok(value) = yaml::parse("fuzz.yaml", &text, &limits) {
                for schema in [&b.profile, &b.tools, &b.triggers, &b.env, &b.agent] {
                    let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        schema.validate(&value)
                    }));
                    assert!(found.is_ok(), "패닉: {text:?}");
                }
                checked += 1;
            }
        }
        assert!(checked > 500, "{checked}");
    }
}
