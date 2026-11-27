//! Verifies byte-exact legacy (`BOTPROFILE1`) skill prompt restoration vs. front-matter stripping and distinct revision hashes in `BOTPROFILE2`.
use bot_core::{
    command::{JsonPayload, Target, Work},
    id::{AgentId, Attempt, BindingId, ClaimEpoch, Generation, PrincipalId, ProfileId, RunId},
    profile::Snapshot,
    state::{Conversation, ConversationKind, RunContext, RunOrigin},
};
use bot_profile::{HostCatalog, Limits, from_bundle, load, revision_of};
use bot_runtime::context::{ContextInput, assemble};
use std::{collections::BTreeSet, fs, time::UNIX_EPOCH};

const BODY: &str = "Visible stable paragraph.\n";
const VALID_FM: &str =
    "---\nname: memo\ndescription: HEADER_SENTINEL\n---\nVisible stable paragraph.\n";
const PROFILE_YAML: &str = "version: 1\nid: personal\ntimezone: UTC\nmodel: assistant\nroles:\n  owner: [run]\nmembers:\n  - {identity: \"local:owner\", roles: [owner]}\nbindings:\n  - {id: desk, adapter: cli, account: owner, agent: main}\n";
const AGENT_MD: &str = "Stable agent.\n";

fn build_v1_bundle(skill_text: &str) -> Vec<u8> {
    let mut entries = vec![
        ("agents/main/AGENT.md", AGENT_MD.as_bytes()),
        ("profile.yaml", PROFILE_YAML.as_bytes()),
        ("skills/memo/SKILL.md", skill_text.as_bytes()),
    ];
    entries.sort_by_key(|(path, _)| *path);
    let mut out = b"BOTPROFILE1\n".to_vec();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (path, bytes) in entries {
        out.extend_from_slice(&(path.len() as u32).to_be_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

fn assemble_system(snapshot: &Snapshot) -> String {
    let id = ProfileId("personal".into());
    let agent = AgentId("main".into());
    let run = RunContext {
        attempt: Attempt {
            profile: id,
            run: RunId("r".into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(1),
        },
        root: RunId("r".into()),
        principal: PrincipalId("local:owner".into()),
        agent: agent.clone(),
        conversation: Conversation {
            kind: ConversationKind::Cli,
            id: "owner".into(),
            thread: None,
        },
        binding: BindingId("desk".into()),
        api: None,
        scopes: BTreeSet::from(["run".to_owned()]),
        private_memory_read: false,
        work: Work {
            revision: snapshot.revision.clone(),
            target: Target::Agent {
                agent,
                instruction: "sample".into(),
            },
            input: JsonPayload(r#"{"text":"sample"}"#.into()),
            attachments: vec![],
            steps: vec![],
            pass_pos: None,
        },
        approver: None,
        trigger: None,
        origin: RunOrigin::Message,
    };
    assemble(&ContextInput {
        snapshot,
        run: &run,
        history: &[],
        memory: &[],
        now: UNIX_EPOCH,
    })
    .expect("context assembly")
    .system
}

#[test]
fn v1_restored_prompt_matches_legacy_bytes_and_differs_in_revision_from_v2() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("personal");
    fs::create_dir_all(root.join("agents/main")).unwrap();
    fs::create_dir_all(root.join("skills/memo")).unwrap();
    fs::write(root.join("profile.yaml"), PROFILE_YAML).unwrap();
    fs::write(root.join("agents/main/AGENT.md"), AGENT_MD).unwrap();
    fs::write(root.join("skills/memo/SKILL.md"), VALID_FM).unwrap();

    let id = ProfileId("personal".into());
    let limits = Limits::default();

    // 1) Legacy V1 restore preserves front matter verbatim (152 bytes).
    let v1_bytes = build_v1_bundle(VALID_FM);
    let v1_rev = revision_of(&v1_bytes);
    let v1_snap = from_bundle(&v1_bytes, &id, &v1_rev, &limits).expect("V1 restore");
    let v1_system = assemble_system(&v1_snap);
    assert!(v1_system.contains("HEADER_SENTINEL"), "{v1_system}");
    assert!(v1_system.contains("---\nname: memo\n"), "{v1_system}");
    assert_eq!(v1_system.len(), 152, "{v1_system:?}");

    // 2) Current V2 load strips front matter (104 bytes) and yields a different revision.
    let set = load(dir.path(), &HostCatalog::demo(), &limits).expect("V2 load");
    let v2_snap = set.profile(&id).unwrap();
    assert!(set.bundle(&id).unwrap().starts_with(b"BOTPROFILE2\n"));
    assert_ne!(
        v2_snap.revision, v1_rev,
        "different prompt interpretation must produce a distinct revision"
    );
    let v2_system = assemble_system(v2_snap);
    assert!(!v2_system.contains("HEADER_SENTINEL"), "{v2_system}");
    assert!(!v2_system.contains("---"), "{v2_system}");
    assert_eq!(v2_system.len(), 104, "{v2_system:?}");

    // 3) Plain SKILL.md without front matter keeps identical bytes and revision across V1 and V2.
    fs::write(root.join("skills/memo/SKILL.md"), BODY).unwrap();
    let clean_v1_bytes = build_v1_bundle(BODY);
    let clean_v1_rev = revision_of(&clean_v1_bytes);
    let clean_v1_snap = from_bundle(&clean_v1_bytes, &id, &clean_v1_rev, &limits).unwrap();
    let clean_set = load(dir.path(), &HostCatalog::demo(), &limits).unwrap();
    let clean_v2_snap = clean_set.profile(&id).unwrap();
    assert_eq!(clean_v2_snap.revision, clean_v1_rev);
    assert_eq!(
        assemble_system(&clean_v1_snap),
        assemble_system(clean_v2_snap)
    );
}
