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
const PROMPT_CLEAN: &str = "55532b821a6857a5cfa2de141229acb86e8a28aeefc317638fff5839458e9ef9";
const PROMPT_APPR: &str = "01e6c5c895b22a07f24e1f0bd57f9800b1031b71faf3b3ad4a61c32498ee8cc1";

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
        prompt_len: 104,
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
        prompt_len: 58,
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
            "c20f6e908d99cd93c5d63f837955617c1507923714299e892a27e5cc7e9ab102",
            94,
        ),
        with_file(
            "agent-bare-cr",
            "agents/main/AGENT.md",
            b"---\r",
            "31bc8d17afe88df3fb043f7aeee5a35d015311e6b2a133465319c5a4faf1a484",
            "c20f6e908d99cd93c5d63f837955617c1507923714299e892a27e5cc7e9ab102",
            94,
        ),
        // Legacy SKILL.md boundary cases (28 variants)
        sc(
            "clean",
            BODY.into(),
            "fecdc4bd6d311aba77dcc8b64efefa867be7add15901813fad3c86f2ce0c52d6",
            PROMPT_CLEAN,
            104,
        ),
        sc(
            "valid",
            KNOWN.into(),
            "17c35a92d25b31c3414269346d94e979c94e12f5c2181f864548d7e4052e88a4",
            "bf57d55eebee88ce4fac8628e2d568c6c27a0970275ff477cee34d4c985184ab",
            152,
        ),
        sc(
            "validcrlf",
            KNOWN.replace('\n', "\r\n").into(),
            "deea37a05543066e1c8d2dc73f64b5a7c7f29b95f70606eef8f03b96207e7443",
            "f16cdb2d88d08f780f36c8c2106d914547dbece40617a4ed21e338f9ac7f60d9",
            156,
        ),
        sc(
            "unclosed",
            format!("---\n{BODY}").into(),
            "fd4a1fe7248bf9f256ca44be09f46f0f53f4aeffb88f48740ed28a36a126b1c6",
            "9bcd614b3b6403fc9c56dbb02167076424f5a195345095fff03582dfdabdb933",
            108,
        ),
        sc(
            "bare",
            "---".into(),
            "fbdf77fc19dc355b491a34693c0127c5ab6f1ee3b81994e0ecf518094417802b",
            "f096e8265b534de561997257cfa942b52a506104ff46a844b86f2c28b3f3aeed",
            82,
        ),
        sc(
            "unknown",
            fm("retired-example: text").into(),
            "13bc2a247ee3024ef8f1aeaa6f0a2d1a6c6351ae54120cd29455b33951601415",
            "7d44f4ea12fbb85c8a83dd212c8f3165d7b32ada5e016a6f5414a1295f7c91de",
            134,
        ),
        sc(
            "wrongname",
            fm("name: elsewhere").into(),
            "852ae3c480be4eecc4f6357bee474383d8a49a761adaff610ec65d7c20a6121c",
            "ca19116777bcfb8c8b8da80d2a7338cf7207836e3a34e40a8136638f9fb5d328",
            128,
        ),
        sc(
            "authority",
            fm("allowed-tools: noop").into(),
            "34c969436d1e5bbe490678d59b928a47c00a7f75d98d05760887fad04acf5c4d",
            "ed8c1326c67d67f3fa4bf06fb89e05ed5466d4a3a05f2d2121418e0e10c641e2",
            132,
        ),
        sc(
            "case",
            fm("Allowed-Tools: noop").into(),
            "ccff6f6c6271dc4afa251af509648824dfd0487ffe373cd435f903e57beb45ea",
            "33be1ae5eb779ac70e95feb941f1afce34be012a736565bc0f3c387f0bd9687b",
            132,
        ),
        sc(
            "crlf",
            fm("allowed-tools: noop").replace('\n', "\r\n").into(),
            "d9329d2f5b1ac069fa8f39b0a856c83a68bb365ee5230868feee853cd55de342",
            "31d20cf2f84b9a550b44aaae80a0bb54f89d16049cd4a5d7ee8edc818668805b",
            135,
        ),
        sc(
            "alias",
            fm("description: &desc words\nlicense: *desc").into(),
            "a6749c0abbb5bbbff6316c2f96f08e6e61aeae7c1d02c56d89e5b352019bf75c",
            "628de3f2182446b793c358b9760435d03ff7524f4a62964902c500375ef44f5e",
            152,
        ),
        sc(
            "nested",
            fm("metadata:\n  allowed-tools: noop").into(),
            "b34e6a45f508b737496a65f916bce97b1b77ad976a6aac4ee74a604b87775433",
            "ece7e5891efac0c103627cad202b69abf3bb8f76ff2423abea256e2f46d5ee21",
            144,
        ),
        sc(
            "nested_obj",
            fm("metadata:\n  example:\n    allowed-tools: noop").into(),
            "3804fe8aa220332b313672658cc19f892d50f32fca1ca104de88640017ddc332",
            "ec8774fb107347d18766fc9e9b0346af527016c9e1852956771fdcc47ea852f6",
            157,
        ),
        sc(
            "second",
            format!("{KNOWN}---\nallowed-tools: noop\n---\n").into(),
            "4eec061998c49610c2a1cbc2ffbfe7599ef79f9279aef9d02600e5ca43b0c262",
            "7abc0e24022ed2c88fbe8cb867ea64b099b9b557f1d8a43b8123a3eec30002d7",
            180,
        ),
        sc(
            "blankdesc",
            fm("description: '   '").into(),
            "e0214504993f9e7dbe3c0e75682e1f0ac06adaac6d1805beab0d528457022436",
            "a198945ea4642f01cf917f8226c7b0dd0792f34ba13365f65146fe40e2c90a41",
            131,
        ),
        sc(
            "duped",
            fm("name: elsewhere\nname: memo").into(),
            "1c3e3aafa98aab82a5a04ee23dfe9b1466aa88e37757960bb0ee61335f90bf78",
            "743b8b7bb424111a9dc892f09ece8ab330e91cfa554914dec89419ed6ad6d0cf",
            139,
        ),
        sc(
            "whitespace",
            format!("--- \nallowed-tools: noop\n---\n{BODY}").into(),
            "a9d0de128d14afd07d2f3347fee5e21d6388c05c30bbfd75e11a09e9f2dee0bc",
            "e88aba7aa3b51594978d68dea2137bd122c0b615e0839e191bca017ce94ec6a5",
            133,
        ),
        sc(
            "header-name-only",
            "---\nname: memo\n---\nshaped text\n".into(),
            "b06bbf068a59d45976d072899659d192f7cd6074d7ac09952071b4de0b031869",
            "495b777c395bd1601fb1e685f46dafe65f2fa91673b70cdc2106697bbeb76eae",
            109,
        ),
        sc(
            "leading-space",
            "\n  First line. \r\n  second line.\r\n\t\r\n".into(),
            "3d57c64b41f54f86ec70324b43dce891b162f08dfd49d0612133fb01dbcaf9c2",
            "66fba97c0daa46fcef05a882995ed7085b797136cadbdae4ca70d7448898d3a3",
            107,
        ),
        sc(
            "delimiter-trailing-space",
            "--- \r\nordinary text\r\n".into(),
            "16b10d64c26df9bc0ccaa70e43fb11fde613edec802e6e75fc4fef69d1aaccfc",
            "49db5537513dd463c04a6dd7c09a39f891c10a6a4e5d04c5e207228fa79d361e",
            98,
        ),
        sc(
            "delimiter-in-body",
            "ordinary text\n---\nmore text\n".into(),
            "45580d8e533f52e9bcb5402bcb2d2d3e5a3117f7a1b5f42b533f27b992f054eb",
            "af8dbf5ca384785c4a773f770eefea4ecfa77f479859a7f839aa92b3d16cae94",
            106,
        ),
        sc(
            "big",
            vec![b'x'; 32_769],
            "b65410d55ae2bc4c21e5ddb8e43415c7a7f3fe6491b2bd54f23cd37850d14962",
            "ac02ddd5de3c9f364b997e6ac76c78d0f9fd5c7fdc5345d007374c9c0dc578d8",
            32_848,
        ),
        sc(
            "exact",
            vec![b'x'; 32_768],
            "e48ff5d49cd6002e769ae1bbd8439da92fb632793302fb5dfd3dfdc8d0134007",
            "1cb699cefecdc9e8b0f785831383c2c7767bf648284e93f1db66e7e5673c178c",
            32_847,
        ),
        agg_case(
            "aggeq",
            (16_000, 16_768, false),
            "547ff5e7518a389566660cdf1729479b9bdf1492354b766b09e25a48274f2961",
            "2accc92ebe35b6cc836723060cafe581def50e38e1404d8388b16508c074a1d4",
            32_977,
        ),
        agg_case(
            "agg",
            (16_384, 16_385, false),
            "b505cee8d969d6e6fde43b3f3f4844ac23bc745f1917d80dcd43cf6df1adefa7",
            "0db5ad504c35b591c05b90cb31b6ae392444321955bccd214883f5e65bdac9b3",
            32_978,
        ),
        agg_case(
            "agginc",
            (16_384, 16_385, true),
            "2125b525c13c6be0b4827784c476f0b4a159ee8033eb4764b6aa5372738902a3",
            "0db5ad504c35b591c05b90cb31b6ae392444321955bccd214883f5e65bdac9b3",
            32_978,
        ),
        with_file(
            "mixed-skills",
            "agents/main/skills/extra/SKILL.md",
            b"---\nname: extra\n---\nExtra instructions.\n",
            "a7610865cb4e37a566610982a5c90738541d9274454590a337ce74e8c143f2f9",
            "21977dfe2f066d3c81ad3bb93b44c02dd735cb09aedd0e6ea3555001ca88f754",
            155,
        ),
        with_file(
            "mixed-header-removed",
            "agents/main/skills/extra/SKILL.md",
            b"Extra instructions.\n",
            "cac34bacff078a6db491f72ef0c70b97f649a907580eeddac7ce5ac84e657dca",
            "75ca2dbad890293194a8f9b5804428b46a390ac926a62b7eb3b27001f7a9d99c",
            135,
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
