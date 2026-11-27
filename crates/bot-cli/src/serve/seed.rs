//! 기억 시드 조립. 프로필 스냅샷의 시드(`profile::MemorySeed`)를 저장소 입력(`command::MemorySeed`)으로 바꾼다.
//! 등록 지문은 owner·visibility·본문을 모두 반영한다 — 소유자나 공개 범위만 바뀌어도 새 항목으로 등록된다.
//! 지문: owner 문자열, visibility(소문자 `private`/`profile`), 본문 순서로 각각 `u64 빅엔디언 길이 + UTF-8 바이트`를
//! 이어 SHA-256. 경로는 지문이 아니라 별도 키다((프로필, 에이전트, 경로, 지문)이 같으면 다시 만들지 않는다).
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

/// 저장소가 받을 수 없는 시드의 이유(본문은 싣지 않는다). 받을 수 있으면 None. 저장소 게시·등록과 같은 검증 함수
/// (`bot_store::check_seed`)를 쓴다 — 저장소가 시드를 거절하는 일이 활성 판본 교체 뒤에 드러나지 않게 시작 전·`bot validate`가 미리 거른다.
pub fn rejection(seed: &MemorySeed) -> Option<String> {
    bot_store::check_seed(seed).err().map(|e| e.to_string())
}

/// 이 프로필의 시드 중 저장소가 받지 못할 것의 안내 문구
pub fn problems(snapshot: &Snapshot) -> Vec<String> {
    seeds(snapshot)
        .iter()
        .filter_map(|seed| {
            rejection(seed)
                .map(|why| format!("{}: {}/{}: {why}", seed.profile.0, seed.agent.0, seed.path))
        })
        .collect()
}

/// 프로필 하나의 시드 전체(에이전트별)
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
        // 독립 계산(Python hashlib)으로 구한 기준값
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
            "결정적"
        );
        for changed in [
            fingerprint("local:other", Visibility::Private, "hello"),
            fingerprint("local:owner", Visibility::Profile, "hello"),
            fingerprint("local:owner", Visibility::Private, "hello!"),
        ] {
            assert_ne!(base, changed);
        }
        // 조각 경계를 옮겨도 같은 글이 되지 않는다(길이가 앞에 붙는다)
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
        assert_eq!(rejection(&seed_of(&"가".repeat(2730))), None); // 8,190바이트
        assert_eq!(
            rejection(&seed_of(&"a".repeat(8192))),
            None,
            "정확히 8 KiB는 받는다"
        );
        let over = rejection(&seed_of(&"a".repeat(8193))).unwrap();
        assert!(over.contains("8193") && over.contains("8192"), "{over}");
        assert!(rejection(&seed_of("  \n")).unwrap().contains("비어"));
        assert!(rejection(&seed_of("a\0b")).unwrap().contains("NUL"));
        let mut long = seed_of("본문");
        long.path = "p".repeat(257);
        assert!(rejection(&long).unwrap().contains("path"));
        // 안내에는 본문이 실리지 않는다
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
        // 경로가 달라도 지문은 같다(경로는 별도 키)
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
