//! Memory seed conversion and deterministic SHA-256 fingerprinting for profile activation.
//!
//! Fingerprint hashes length-prefixed `(owner, visibility, text)` with u64 big-endian lengths;
//! path is kept as a separate deduplication key `(profile, agent, path, sha256)`.

use bot_core::{command::MemorySeed, profile::Snapshot, state::Visibility};
use sha2::{Digest, Sha256};

pub fn visibility_name(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Private => "private",
        Visibility::Profile => "profile",
    }
}

pub fn fingerprint(owner: &str, visibility: Visibility, text: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in [owner, visibility_name(visibility), text] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().into()
}

pub fn rejection(seed: &MemorySeed) -> Option<String> {
    bot_store::check_seed(seed).err().map(|e| e.to_string())
}

pub fn problems(snapshot: &Snapshot) -> Vec<String> {
    seeds(snapshot)
        .iter()
        .filter_map(|seed| {
            rejection(seed)
                .map(|why| format!("{}: {}/{}: {why}", seed.profile.0, seed.agent.0, seed.path))
        })
        .collect()
}

pub fn seeds(snapshot: &Snapshot) -> Vec<MemorySeed> {
    snapshot
        .agents
        .iter()
        .flat_map(|(agent, entry)| {
            entry.memory_seeds.iter().map(move |seed| MemorySeed {
                profile: snapshot.id.clone(),
                agent: agent.clone(),
                path: seed.document.relative_path.clone(),
                sha256: fingerprint(&seed.owner.0, seed.visibility, &seed.document.text),
                owner: seed.owner.clone(),
                visibility: seed.visibility,
                text: seed.document.text.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: [u8; 32]) -> String {
        bot_core::random::hex(&bytes)
    }

    #[test]
    fn matches_the_documented_construction() {
        assert_eq!(
            hex(fingerprint("local:owner", Visibility::Private, "hello")),
            "8f819fbf630cbf61b6716ce12fb33078bd3216bd111eb10d3233c887f1199920"
        );
        assert_eq!(
            hex(fingerprint("local:owner", Visibility::Profile, "hello")),
            "7ac5b49d8565fafad1f273d32c82d55beb54907b59a8a01b9e3f18091d25f0d0"
        );
    }

    #[test]
    fn each_field_alone_changes_the_fingerprint_and_boundaries_cannot_shift() {
        let base = fingerprint("local:owner", Visibility::Private, "hello");
        assert_eq!(
            base,
            fingerprint("local:owner", Visibility::Private, "hello"),
            "deterministic"
        );
        for changed in [
            fingerprint("local:other", Visibility::Private, "hello"),
            fingerprint("local:owner", Visibility::Profile, "hello"),
            fingerprint("local:owner", Visibility::Private, "hello!"),
        ] {
            assert_ne!(base, changed);
        }
        assert_ne!(
            fingerprint("a", Visibility::Private, "bprivate"),
            fingerprint("ab", Visibility::Private, "private")
        );
    }

    fn seed_of(text: &str) -> MemorySeed {
        use bot_core::id::{AgentId, PrincipalId, ProfileId};
        MemorySeed {
            profile: ProfileId("personal".into()),
            agent: AgentId("main".into()),
            path: "memory/note.md".into(),
            sha256: [0; 32],
            owner: PrincipalId("local:owner".into()),
            visibility: Visibility::Private,
            text: text.into(),
        }
    }

    #[test]
    fn seeds_beyond_the_store_limits_are_rejected_before_anything_is_written() {
        assert_eq!(rejection(&seed_of(&"€".repeat(2730))), None); // 8,190 bytes
        assert_eq!(
            rejection(&seed_of(&"a".repeat(8192))),
            None,
            "exact 8 KiB is allowed"
        );
        let over = rejection(&seed_of(&"a".repeat(8193))).unwrap();
        assert!(over.contains("8193") && over.contains("8192"), "{over}");
        assert!(rejection(&seed_of("  \n")).unwrap().contains("empty"));
        assert!(rejection(&seed_of("a\0b")).unwrap().contains("NUL"));
        let mut long = seed_of("body");
        long.path = "p".repeat(257);
        assert!(rejection(&long).unwrap().contains("path"));
        assert!(!over.contains("aaaa"));
    }

    #[test]
    fn assembles_one_seed_per_agent_entry_with_path_as_a_separate_key() {
        use bot_core::{
            id::{AgentId, PrincipalId},
            profile::{Document, MemorySeed as Seed},
        };
        use bot_gateway::testing::profile;
        let mut snapshot = (*profile("personal").build()).clone();
        snapshot
            .agents
            .get_mut(&AgentId("main".into()))
            .unwrap()
            .memory_seeds
            .push(Seed {
                document: Document {
                    relative_path: "memory/note.md".into(),
                    text: "hello".into(),
                },
                owner: PrincipalId("local:owner".into()),
                visibility: Visibility::Private,
            });
        let seeds = seeds(&snapshot);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].path, "memory/note.md");
        assert_eq!(seeds[0].agent.0, "main");
        assert_eq!(
            seeds[0].sha256,
            fingerprint("local:owner", Visibility::Private, "hello")
        );
        snapshot
            .agents
            .get_mut(&AgentId("main".into()))
            .unwrap()
            .memory_seeds[0]
            .document
            .relative_path = "memory/other.md".into();
        assert_eq!(super::seeds(&snapshot)[0].sha256, seeds[0].sha256);
    }
}
