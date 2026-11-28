//! Regression corpus of 45 legacy (`417162e`) V1 (`BOTPROFILE1`) bundles verifying byte-exact restore, policy revision, and prompt assembly.
//!
//! Each `(bundle_sha256, policy_sha256, prompt_sha256, prompt_len)` tuple is pinned from the `417162e` binary
//! output with `context::assemble(now = UNIX_EPOCH)` so validation tightenings never shrink the legacy restore set.

use bot_core::{
    command::{JsonPayload, Target, Work},
    id::*,
    state::{Conversation, ConversationKind, RunContext, RunOrigin},
};
use bot_profile::{Limits, from_bundle, revision_of};
use bot_runtime::context::{ContextInput, assemble};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::UNIX_EPOCH,
};

const BASE_PROFILE: &str = "version: 1\nid: personal\ntimezone: UTC\nmodel: assistant\nroles:\n  owner: [run]\nmembers:\n  - {identity: \"local:owner\", roles: [owner]}\nbindings:\n  - {id: desk, adapter: cli, account: owner, agent: main}\n";
const BASE_AGENT: &str = "Stable agent.\n";
const BODY: &str = "Visible stable paragraph.\n";
const KNOWN: &str =
    "---\nname: memo\ndescription: HEADER_SENTINEL\n---\nVisible stable paragraph.\n";

const POL_BASE: &str = "a4f409e28bc3c3729f8965e2a0548c59b8bd1a981e316bfb5fb2501774900fa2";
const POL_CRON: &str = "f144d7763de40c7ffdee623a435ff13614e33a2a1faaa8f16e7446abed3322cd";
const POL_APPR: &str = "73d1f69f3200d1f8b8f4857f6bdb3fed52792283bf4b306e28041f2bbc062dfc";
const PROMPT_CLEAN: &str = "0b630d79fa2b82770b9ecab4e375d2c117739cf7b798ffea861d34590dc53477";
const PROMPT_APPR: &str = "cc5af620f49374c3bba354b3b4d5175646c70b27a78037b80221ecd55eb3afa2";

struct Case {
    name: &'static str,
    files: BTreeMap<String, Vec<u8>>,
    bundle_sha: &'static str,
    policy_sha: &'static str,
    prompt_sha: &'static str,
    prompt_len: usize,
}

fn encode_v1(files: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = b"BOTPROFILE1\n".to_vec();
    out.extend_from_slice(&(files.len() as u32).to_be_bytes());
    for (path, bytes) in files {
        out.extend_from_slice(&(path.len() as u32).to_be_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

fn base_files() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("profile.yaml".into(), BASE_PROFILE.as_bytes().to_vec()),
        (
            "agents/main/AGENT.md".into(),
            BASE_AGENT.as_bytes().to_vec(),
        ),
        ("skills/memo/SKILL.md".into(), BODY.as_bytes().to_vec()),
    ])
}

fn fm(tail: &str) -> String {
    format!("---\n{tail}\n---\n{BODY}")
}

fn with_file(
    name: &'static str,
    path: &str,
    bytes: impl Into<Vec<u8>>,
    bundle_sha: &'static str,
    prompt_sha: &'static str,
    prompt_len: usize,
) -> Case {
    let mut files = base_files();
    files.insert(path.into(), bytes.into());
    Case {
        name,
        files,
        bundle_sha,
        policy_sha: POL_BASE,
        prompt_sha,
        prompt_len,
    }
}

fn agg_case(
    name: &'static str,
    lens: (usize, usize, bool),
    bundle_sha: &'static str,
    prompt_sha: &'static str,
    prompt_len: usize,
) -> Case {
    let mut files = base_files();
    files.insert("skills/memo/SKILL.md".into(), vec![b'a'; lens.0]);
    files.insert(
        "agents/main/agents/research/AGENT.md".into(),
        b"Child agent.".to_vec(),
    );
    files.insert(
        "agents/main/skills/other/SKILL.md".into(),
        vec![b'b'; lens.1],
    );
    if lens.2 {
        files.insert(
            "agents/main/agents/research/skills/third/SKILL.md".into(),
            b"c".to_vec(),
        );
    }
    Case {
        name,
        files,
        bundle_sha,
        policy_sha: POL_BASE,
        prompt_sha,
        prompt_len,
    }
}

fn cron_case(name: &'static str, expr: &str, bundle_sha: &'static str) -> Case {
    let mut files = base_files();
    let profile = BASE_PROFILE.replace(
        "  - {identity: \"local:owner\", roles: [owner]}\n",
        "  - {identity: \"local:owner\", roles: [owner]}\n  - {identity: \"service:morning\", roles: [owner]}\n",
    );
    let triggers = format!(
        "version: 1\ntriggers:\n  - id: morning\n    \"on\":\n      cron: {}\n    principal: service:morning\n    target:\n      instruction: summary\n",
        expr.trim_end()
    );
    files.insert("profile.yaml".into(), profile.into_bytes());
    files.insert("agents/main/triggers.yaml".into(), triggers.into_bytes());
    Case {
        name,
        files,
        bundle_sha,
        policy_sha: POL_CRON,
        prompt_sha: PROMPT_CLEAN,
        prompt_len: 215,
    }
}

fn approval_case(name: &'static str, spelling: &str, bundle_sha: &'static str) -> Case {
    let alias = "a".repeat(63);
    let approvers: Vec<String> = (0..16).map(|i| format!("slack:{alias}/U{i:031}")).collect();
    let targets: Vec<String> = (0..64)
        .map(|i| {
            let op = if i == 0 {
                ".".to_owned()
            } else {
                format!(".{}.", "x".repeat(126))
            };
            format!("{{\"operation\": \"{op}\", \"target\": \"channel:r{i}\"}}")
        })
        .collect();
    let app_json: Vec<String> = approvers.iter().map(|a| format!("\"{a}\"")).collect();
    let pre = format!(
        "{{\"id\": \"pre\", \"on\": {{\"cron\": \"0 9 * * *\"}}, \"principal\": \"service:morning\", \"target\": {{\"tool\": \"memo.notify\", \"args\": {{\"report\": \"daily\"}}}}, \"preapproved\": {{\"max_per_day\": {spelling}, \"targets\": [{}]}}}}",
        targets.join(", ")
    );
    let human = format!(
        "{{\"id\": \"confirm\", \"on\": {{\"event\": \"message\", \"binding\": \"team\"}}, \"target\": {{\"tool\": \"memo.notify\"}}, \"match\": {{\"body.text\": \"confirm\"}}, \"input\": {{\"report\": \"body.text\"}}, \"human_approval\": {{\"approvers\": [{}]}}}}",
        app_json.join(", ")
    );
    let mut people = "  - {identity: \"service:morning\", roles: [owner]}\n".to_owned();
    for who in &approvers {
        people.push_str(&format!("  - {{identity: \"{who}\", roles: [owner]}}\n"));
    }
    let profile = format!(
        "{}  - {{id: team, adapter: slack, workspace: {alias}, channel: planning, agent: main}}\npolicy:\n  scopes: [run, memo.notify]\n  approve: [memo.notify]\n",
        BASE_PROFILE
            .replace("owner: [run]", "owner: [run, memo.notify]")
            .replace("bindings:\n", &format!("{people}bindings:\n")),
    );
    let tools = "version: 1\nserver: notify\ntools:\n  - id: notify\n    class: irreversible\n    effects: true\n    scopes: [memo.notify]\n    input: {type: object, required: [report], properties: {report: {type: string}}, additionalProperties: false}\n";
    let mut files = base_files();
    files.insert("profile.yaml".into(), profile.into_bytes());
    files.insert("skills/memo/tools.yaml".into(), tools.as_bytes().to_vec());
    files.insert(
        "agents/main/triggers.yaml".into(),
        format!("version: 1\ntriggers:\n  - {pre}\n  - {human}\n").into_bytes(),
    );
    Case {
        name,
        files,
        bundle_sha,
        policy_sha: POL_APPR,
        prompt_sha: PROMPT_APPR,
        prompt_len: 169,
    }
}

fn corpus() -> Vec<Case> {
    let sc =
        |name, skill: Vec<u8>, b, p, len| with_file(name, "skills/memo/SKILL.md", skill, b, p, len);
    let mut cases = vec![
        // Legacy AGENT.md bare `---` / `---\r`
        with_file(
            "agent-bare",
            "agents/main/AGENT.md",
            b"---",
            "7cd419ea4a85e95ce1dc11b11d8c8d18e1818611facdf03a9d78d28249e5a374",
            "ef8e8518242bff06b8da418eedb25de07d67bc2a453c8ee188dd5cc495f81cb6",
            205,
        ),
        with_file(
            "agent-bare-cr",
            "agents/main/AGENT.md",
            b"---\r",
            "31bc8d17afe88df3fb043f7aeee5a35d015311e6b2a133465319c5a4faf1a484",
            "ef8e8518242bff06b8da418eedb25de07d67bc2a453c8ee188dd5cc495f81cb6",
            205,
        ),
        // Legacy SKILL.md boundary cases (28 variants)
        sc(
            "clean",
            BODY.into(),
            "fecdc4bd6d311aba77dcc8b64efefa867be7add15901813fad3c86f2ce0c52d6",
            PROMPT_CLEAN,
            215,
        ),
        sc(
            "valid",
            KNOWN.into(),
            "17c35a92d25b31c3414269346d94e979c94e12f5c2181f864548d7e4052e88a4",
            "54d18d47100b9164a0c7a03ac521c8aee91645065fdf6366a0859b711a076c7b",
            263,
        ),
        sc(
            "validcrlf",
            KNOWN.replace('\n', "\r\n").into(),
            "deea37a05543066e1c8d2dc73f64b5a7c7f29b95f70606eef8f03b96207e7443",
            "a85d8d2815a1d44c14a521d9538d627fadb197f9a2817f761733ce5e19efec1e",
            267,
        ),
        sc(
            "unclosed",
            format!("---\n{BODY}").into(),
            "fd4a1fe7248bf9f256ca44be09f46f0f53f4aeffb88f48740ed28a36a126b1c6",
            "bdc670c0657d551aa61ed4c83c1bf5ce8eb503de70914aa69db4dbcfbf09647e",
            219,
        ),
        sc(
            "bare",
            "---".into(),
            "fbdf77fc19dc355b491a34693c0127c5ab6f1ee3b81994e0ecf518094417802b",
            "caaf9bd8279a37eefa8bf770882159df24bdb1139e38cecd355ebd0a286185bb",
            193,
        ),
        sc(
            "unknown",
            fm("retired-example: text").into(),
            "13bc2a247ee3024ef8f1aeaa6f0a2d1a6c6351ae54120cd29455b33951601415",
            "b83093fd293e071a5f9f257bd1e1c637eaabda517b8ceeea7c86a4f32aa6dfe8",
            245,
        ),
        sc(
            "wrongname",
            fm("name: elsewhere").into(),
            "852ae3c480be4eecc4f6357bee474383d8a49a761adaff610ec65d7c20a6121c",
            "3fe24071af4f5fceb0a145e4123aeb95ecde5a04ae9cae546ad8838c6fbbedaf",
            239,
        ),
        sc(
            "authority",
            fm("allowed-tools: noop").into(),
            "34c969436d1e5bbe490678d59b928a47c00a7f75d98d05760887fad04acf5c4d",
            "eb838e128ecf37818c37c757c406dc3370e1873096108e824a46c07a25404b7f",
            243,
        ),
        sc(
            "case",
            fm("Allowed-Tools: noop").into(),
            "ccff6f6c6271dc4afa251af509648824dfd0487ffe373cd435f903e57beb45ea",
            "1d5cd5df11e2da3be70a81a429259236cdcdcf30d027c1a8d157bfc1ff6ec0d4",
            243,
        ),
        sc(
            "crlf",
            fm("allowed-tools: noop").replace('\n', "\r\n").into(),
            "d9329d2f5b1ac069fa8f39b0a856c83a68bb365ee5230868feee853cd55de342",
            "ee287970fa299a934762d970006a3bb3f32689c96483d94a0bac7ee39e4bef74",
            246,
        ),
        sc(
            "alias",
            fm("description: &desc words\nlicense: *desc").into(),
            "a6749c0abbb5bbbff6316c2f96f08e6e61aeae7c1d02c56d89e5b352019bf75c",
            "423cead64d9e603403c9b5326d2fd95e004db1b107bc4eed0d43283dccee93cb",
            263,
        ),
        sc(
            "nested",
            fm("metadata:\n  allowed-tools: noop").into(),
            "b34e6a45f508b737496a65f916bce97b1b77ad976a6aac4ee74a604b87775433",
            "b002f6c20e21296bad670417e58849ac4bc11d8991f84ac2c216d31526bf944d",
            255,
        ),
        sc(
            "nested_obj",
            fm("metadata:\n  example:\n    allowed-tools: noop").into(),
            "3804fe8aa220332b313672658cc19f892d50f32fca1ca104de88640017ddc332",
            "08ff98ebf84e7af2c6e43db64b11f3a9a65f6acb3aca356cf484933749906b2f",
            268,
        ),
        sc(
            "second",
            format!("{KNOWN}---\nallowed-tools: noop\n---\n").into(),
            "4eec061998c49610c2a1cbc2ffbfe7599ef79f9279aef9d02600e5ca43b0c262",
            "c6b9a11a2fa76a2431c24539ebc441231afc010af95fa20076963769a468941b",
            291,
        ),
        sc(
            "blankdesc",
            fm("description: '   '").into(),
            "e0214504993f9e7dbe3c0e75682e1f0ac06adaac6d1805beab0d528457022436",
            "cafea13b1030447a368f3f02ba81d8bee6a8dc28416e9f3ba62d65feb96e7698",
            242,
        ),
        sc(
            "duped",
            fm("name: elsewhere\nname: memo").into(),
            "1c3e3aafa98aab82a5a04ee23dfe9b1466aa88e37757960bb0ee61335f90bf78",
            "bdcd871b9c8320b83ac3ef49ea421610383408850295d32e2f14f1c0453759e2",
            250,
        ),
        sc(
            "whitespace",
            format!("--- \nallowed-tools: noop\n---\n{BODY}").into(),
            "a9d0de128d14afd07d2f3347fee5e21d6388c05c30bbfd75e11a09e9f2dee0bc",
            "fb80d7b03c9a9a1fd3ea80fcc9c03592b398358600ec845240515cdadeddf616",
            244,
        ),
        sc(
            "header-name-only",
            "---\nname: memo\n---\nshaped text\n".into(),
            "b06bbf068a59d45976d072899659d192f7cd6074d7ac09952071b4de0b031869",
            "d92af639a2da5cb2629d85300cd85be3ae477f0a8b0d5272dc99e83c78c8690f",
            220,
        ),
        sc(
            "leading-space",
            "\n  First line. \r\n  second line.\r\n\t\r\n".into(),
            "3d57c64b41f54f86ec70324b43dce891b162f08dfd49d0612133fb01dbcaf9c2",
            "ac0de0145eca67dc0bb28129485843a9a81dd46604f0f831d25e128dd63265e2",
            218,
        ),
        sc(
            "delimiter-trailing-space",
            "--- \r\nordinary text\r\n".into(),
            "16b10d64c26df9bc0ccaa70e43fb11fde613edec802e6e75fc4fef69d1aaccfc",
            "0099e1debe49f25c4ec22a3b7b7dfb1da8e4a1a92890b27b5677ae8a8d9593ed",
            209,
        ),
        sc(
            "delimiter-in-body",
            "ordinary text\n---\nmore text\n".into(),
            "45580d8e533f52e9bcb5402bcb2d2d3e5a3117f7a1b5f42b533f27b992f054eb",
            "39c1bf82c46a19efc3ca7ae8289c00a5c57951aea9c49328a24b7d9eae9b2f89",
            217,
        ),
        sc(
            "big",
            vec![b'x'; 32_769],
            "b65410d55ae2bc4c21e5ddb8e43415c7a7f3fe6491b2bd54f23cd37850d14962",
            "3c95a749490b3f61caab529b8bfc21bff89430a41482b37d72ebdf28478426a3",
            32_959,
        ),
        sc(
            "exact",
            vec![b'x'; 32_768],
            "e48ff5d49cd6002e769ae1bbd8439da92fb632793302fb5dfd3dfdc8d0134007",
            "e5ac98a6a9f4d781faddc724a0f711c987ea9722158e563cb2c622f0aa8ce358",
            32_958,
        ),
        agg_case(
            "aggeq",
            (16_000, 16_768, false),
            "547ff5e7518a389566660cdf1729479b9bdf1492354b766b09e25a48274f2961",
            "586fd6132c699c73c919ffd115eb890e0456eed48f7618636565455765803cfe",
            33_088,
        ),
        agg_case(
            "agg",
            (16_384, 16_385, false),
            "b505cee8d969d6e6fde43b3f3f4844ac23bc745f1917d80dcd43cf6df1adefa7",
            "ba79e23ae0955d0d90c38230039049917f2a051b0962dc23a235cde5767ac4d1",
            33_089,
        ),
        agg_case(
            "agginc",
            (16_384, 16_385, true),
            "2125b525c13c6be0b4827784c476f0b4a159ee8033eb4764b6aa5372738902a3",
            "ba79e23ae0955d0d90c38230039049917f2a051b0962dc23a235cde5767ac4d1",
            33_089,
        ),
        with_file(
            "mixed-skills",
            "agents/main/skills/extra/SKILL.md",
            b"---\nname: extra\n---\nExtra instructions.\n",
            "a7610865cb4e37a566610982a5c90738541d9274454590a337ce74e8c143f2f9",
            "20fc02551ba031e066c85a0b3a350c2c8c3340e5bd3e6b77177e963dafad60ba",
            266,
        ),
        with_file(
            "mixed-header-removed",
            "agents/main/skills/extra/SKILL.md",
            b"Extra instructions.\n",
            "cac34bacff078a6db491f72ef0c70b97f649a907580eeddac7ce5ac84e657dca",
            "cc9c8d1918ef85be1e5d824338b57cb38964054f8d065d9630c157432efaea9f",
            246,
        ),
    ];
    // Legacy triggers.yaml cron variants (12 cases: ASCII, block, folded, Unicode whitespace)
    let crons = [
        (
            "cron-ordinary",
            "\"0 9 * * *\"",
            "613d58f050ee596f7ff9e5dc86c259917b18d0c255564147a995f7c65ba2ce48",
        ),
        (
            "cron-tabs",
            "\"0\\t9 * * *\"",
            "7109479cd849c99f573dd487f6f81778766059e495df8dab86a6f8df3c3b0cf9",
        ),
        (
            "cron-block",
            "|\n        0 9\n        * * *\n",
            "59f437a7900e690a0ae6e95728d623ed3231ee488ee7006544a7291b5243c23f",
        ),
        (
            "cron-folded",
            ">-\n        0 9\n        * * *\n",
            "c1d38bc62e750754f08dbbd2c998fbabfab30c22bc362722228a15f16a4a3ed4",
        ),
        (
            "cron-quoted-newline",
            "\"0\\n9 * * *\"",
            "9b9e2c1023671394072c1c29034350b7ff38376b2ac3f4d8a460406d3e4ed925",
        ),
        (
            "cron-alias",
            "\"@midnight\"",
            "7b63173650e80048a9022619677213427a2462aef8b682c3fbc38bf65a75faf4",
        ),
        (
            "cron-ascii-padding",
            "\"  \\t@midnight\\r\\n \"",
            "9be80564ff7dacea224b228f492b7e2a502a11b90c20addd08f91e0ee2c49d29",
        ),
        (
            "cron-nbsp-inner",
            "\"0\\u00a09 * * *\"",
            "32156ed11e6496486c760c69aa091ce2bf9d1d900d6cadbaef9650c23485df48",
        ),
        (
            "cron-narrow-nbsp-inner",
            "\"0\\u202f9 * * *\"",
            "c18a8fd562bb7a75565da3c17f7742c6eb0c5c7be8f81320397edc01d7529d2c",
        ),
        (
            "cron-em-space-inner",
            "\"0\\u20039 * * *\"",
            "a0aa588aa9abdcf44e418bb0d23c6d6e90d15d3aae953a4460875a0ae89b404c",
        ),
        (
            "cron-ideographic-inner",
            "\"0\\u30009 * * *\"",
            "8246b31fc63323db2b66cb065ab78b06e0f4ed309a1887e92e6f62dee6c30eb7",
        ),
        (
            "cron-unicode-alias-padding",
            "\"\\u2002@midnight\\u3000\"",
            "77a37881aa4bb32e9834a6efad35ed5f656867a361d1aac71e999d26a2ba51b0",
        ),
    ];
    cases.extend(crons.into_iter().map(|(n, e, h)| cron_case(n, e, h)));
    // Legacy preapproved (64 pairs, 128-byte ops) + human_approval (16 approvers, 102-char) boundaries (3 cases)
    let approvals = [
        (
            "approval-65535",
            "65535",
            "1a144386216b7cbf75866410fb42e1f88eeb0517380d8bd5f15b73172f319698",
        ),
        (
            "approval-0xffff",
            "0xFFFF",
            "f7e50af85bd22b8872568ca3f4ad6c971697bb7a499a318eb1126e983d5ba27b",
        ),
        (
            "approval-plus",
            "+65535",
            "4d6b16e24719b2dbfe0b525c83b2f49f64c1928b36d31e1bd4b78cafc5e9a27b",
        ),
    ];
    cases.extend(
        approvals
            .into_iter()
            .map(|(n, s, h)| approval_case(n, s, h)),
    );
    cases
}

fn sample_run(profile: ProfileId, main: AgentId, revision: Revision) -> RunContext {
    RunContext {
        attempt: Attempt {
            profile,
            run: RunId("r".into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(1),
        },
        root: RunId("r".into()),
        principal: PrincipalId("local:owner".into()),
        agent: main.clone(),
        conversation: Conversation {
            kind: ConversationKind::Cli,
            id: "owner".into(),
            thread: None,
        },
        binding: BindingId("desk".into()),
        api: None,
        intake: bot_core::state::IntakeMeta::default(),
        scopes: BTreeSet::from(["run".to_owned()]),
        private_memory_read: false,
        work: Work {
            revision,
            target: Target::Agent {
                agent: main,
                instruction: "sample".into(),
            },
            input: JsonPayload(r#"{"text":"sample"}"#.into()),
            attachments: Vec::new(),
            steps: Vec::new(),
            pass_pos: None,
        },
        approver: None,
        trigger: None,
        origin: RunOrigin::Message,
    }
}

#[test]
fn all_45_legacy_417162e_bundles_restore_and_assemble_byte_identical_prompts() {
    let cases = corpus();
    assert_eq!(cases.len(), 45);
    let (id, main, limits) = (
        ProfileId("personal".into()),
        AgentId("main".into()),
        Limits::default(),
    );
    for case in &cases {
        let bundle = encode_v1(&case.files);
        let rev = revision_of(&bundle);
        assert_eq!(
            rev.0, case.bundle_sha,
            "{}: V1 bundle SHA mismatch",
            case.name
        );
        let snap = from_bundle(&bundle, &id, &rev, &limits)
            .unwrap_or_else(|e| panic!("{}: legacy V1 bundle restore failed: {e:?}", case.name));
        assert_eq!(
            snap.policy_revision.0, case.policy_sha,
            "{}: policy revision mismatch",
            case.name
        );
        let run = sample_run(id.clone(), main.clone(), snap.revision.clone());
        let assembled = assemble(&ContextInput {
            snapshot: &snap,
            run: &run,
            history: &[],
            memory: &[],
            now: UNIX_EPOCH,
        })
        .unwrap_or_else(|e| panic!("{}: prompt assembly failed: {e:?}", case.name));
        assert_eq!(
            assembled.system.len(),
            case.prompt_len,
            "{}: prompt length mismatch",
            case.name
        );
        assert_eq!(
            revision_of(assembled.system.as_bytes()).0,
            case.prompt_sha,
            "{}: prompt content mismatch",
            case.name
        );
    }
}
