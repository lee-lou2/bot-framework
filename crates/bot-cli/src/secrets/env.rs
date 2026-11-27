//! Development `SecretStore` reading profile-scoped `BOT_SECRET__<profile>__<name>` environment variables.

use super::{Namespace, checked_profile};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{ProfileSecrets, Reply, SecretStore},
};
use std::collections::BTreeMap;

const PREFIX: &str = "BOT_SECRET__";

pub fn variable_name(profile: &str, name: &str) -> String {
    format!(
        "{PREFIX}{}__{}",
        profile.replace('-', "_"),
        name.replace('-', "_")
    )
}

#[derive(Default)]
pub struct EnvStore {
    vars: BTreeMap<String, Vec<u8>>,
}

impl std::fmt::Debug for EnvStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvStore")
            .field("entries", &self.vars.len())
            .finish()
    }
}

impl EnvStore {
    pub fn from_process() -> Self {
        use std::os::unix::ffi::OsStrExt;
        Self {
            vars: std::env::vars_os()
                .filter_map(|(key, value)| {
                    let key = key.into_string().ok()?;
                    key.starts_with(PREFIX)
                        .then(|| (key, value.as_bytes().to_vec()))
                })
                .collect(),
        }
    }

    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            vars: pairs
                .into_iter()
                .filter(|(key, _)| key.starts_with(PREFIX))
                .map(|(key, value)| (key, value.into_bytes()))
                .collect(),
        }
    }
}

impl SecretStore for EnvStore {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let handle = checked_profile(&profile).map(|profile| {
            let own = format!("{PREFIX}{}__", profile.replace('-', "_"));
            // Copy only this profile's namespace so foreign secrets never enter the handle.
            let values: BTreeMap<String, Vec<u8>> = self
                .vars
                .iter()
                .filter_map(|(key, value)| {
                    let encoded = key.strip_prefix(&own)?;
                    if encoded.contains('-') {
                        return None;
                    }
                    let name = encoded.replace('_', "-");
                    SecretName::parse(&name)?;
                    Some((name, value.clone()))
                })
                .collect();
            Box::new(Namespace::new(values)) as Box<dyn ProfileSecrets>
        });
        Box::pin(std::future::ready(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::super::block_on;
    use super::*;
    use bot_core::port::Error;

    fn pair(profile: &str, name: &str, value: &str) -> (String, String) {
        (variable_name(profile, name), value.to_string())
    }

    fn store() -> EnvStore {
        EnvStore::from_pairs([
            pair("personal", "calendar-reader", "value-personal"),
            pair("work", "calendar-reader", "value-work"),
            pair("personal", "empty", ""),
            ("UNRELATED".into(), "nope".into()),
        ])
    }

    fn resolve(store: &EnvStore, profile: &str, name: &str) -> Result<String, Error> {
        let handle = block_on(store.for_profile(ProfileId(profile.into())))?;
        let value = block_on(handle.resolve(SecretName::parse(name).unwrap()))?;
        Ok(String::from_utf8(value.0).unwrap())
    }

    #[test]
    fn variable_names_are_readable() {
        assert_eq!(
            variable_name("personal", "calendar-reader"),
            "BOT_SECRET__personal__calendar_reader"
        );
        assert_eq!(
            variable_name("home-2", "api-key"),
            "BOT_SECRET__home_2__api_key"
        );
    }

    #[test]
    fn each_profile_sees_only_its_own_value_for_the_same_name() {
        let store = store();
        assert_eq!(
            resolve(&store, "personal", "calendar-reader").unwrap(),
            "value-personal"
        );
        assert_eq!(
            resolve(&store, "work", "calendar-reader").unwrap(),
            "value-work"
        );
    }

    #[test]
    fn missing_empty_and_foreign_names_are_missing() {
        let store = store();
        for (profile, name) in [
            ("personal", "absent"),
            ("personal", "empty"),
            ("nobody", "calendar-reader"),
        ] {
            assert!(matches!(
                resolve(&store, profile, name),
                Err(Error::Missing)
            ));
        }
    }

    #[test]
    fn double_or_trailing_hyphens_are_invalid_in_either_component() {
        let store = store();
        for bad in ["a--b", "a-", "-a", "a_b"] {
            assert!(SecretName::parse(bad).is_none());
            assert!(matches!(
                block_on(store.for_profile(ProfileId(bad.into()))),
                Err(Error::Invalid)
            ));
        }
    }

    #[test]
    fn a_profile_prefix_does_not_leak_into_a_longer_profile_name() {
        let store = EnvStore::from_pairs([
            pair("personal-extra", "token", "other"),
            pair("personal", "token", "mine"),
        ]);
        assert_eq!(resolve(&store, "personal", "token").unwrap(), "mine");
        assert_eq!(resolve(&store, "personal-extra", "token").unwrap(), "other");
    }

    #[test]
    fn variables_that_are_not_canonical_are_ignored() {
        let store = EnvStore::from_pairs([
            ("BOT_SECRET__personal__api-key".into(), "raw".into()),
            ("BOT_SECRET__personal__API_KEY".into(), "uppercase".into()),
            ("BOT_SECRET__personal__api__key".into(), "extra".into()),
            (
                "BOT_SECRET__706572736F6E616C__6170692D6B6579".into(),
                "old".into(),
            ),
        ]);
        assert!(matches!(
            resolve(&store, "personal", "api-key"),
            Err(Error::Missing)
        ));
        assert!(matches!(
            resolve(&store, "personal", "personal"),
            Err(Error::Missing)
        ));
    }

    #[test]
    fn unchecked_profile_strings_are_invalid() {
        let store = store();
        for bad in ["", "Personal", "../x", "a/b", "personal\n", "a_b"] {
            let result = block_on(store.for_profile(ProfileId(bad.into())));
            assert!(matches!(result, Err(Error::Invalid)), "{bad:?}");
        }
    }

    #[test]
    fn debug_only_reports_the_entry_count() {
        let secret = "synthetic-calendar-token";
        let store = EnvStore::from_pairs([pair("personal", "calendar-reader", secret)]);
        assert_eq!(format!("{store:?}"), "EnvStore { entries: 1 }");
        for output in [format!("{store:?}"), format!("{store:#?}")] {
            assert!(!output.contains(secret));
            assert!(!output.contains(&format!("{:?}", secret.as_bytes())));
            assert!(!output.contains("BOT_SECRET"));
        }
    }

    #[test]
    fn only_secret_prefixed_variables_are_copied() {
        let store = store();
        assert_eq!(store.vars.len(), 3);
        assert!(!format!("{store:?}").contains("nope"));
    }
}
