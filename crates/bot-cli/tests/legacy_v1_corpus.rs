//! 구판(`417162e`) 바이너리가 실제로 수락·보관한 V1(`BOTPROFILE1`) 번들 45종 전체 회귀 코퍼스 시험.
//!
//! 각 사례의 `(bundle_sha256, policy_sha256, prompt_sha256, prompt_len)`은 `417162e` 구판 바이너리가
//! 실제로 만든 번들과 `context::assemble(now = UNIX_EPOCH)` 출력에서 고정한 암호 해시다.
//! - 번들 바이트의 SHA-256이 `417162e` 실물 번들과 1비트까지 같음을 먼저 검증한 뒤 `from_bundle`로 복원한다.
//! - 복원한 스냅숏의 `policy_revision`과 조립된 시스템 프롬프트의 SHA-256·길이가 `417162e`와 바이트 단위로
//!   일치함을 단언한다. 앞으로 검증 규칙을 바꾸다 구판 수락 집합을 줄이면 이 시험이 즉시 잡는다.

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
const PROMPT_CLEAN: &str = "7f8530777bb914710a263fd6189b43297fb5de5cb66c2764703735f472f65d1f";
const PROMPT_APPR: &str = "e6bd15d22fe7e1c5a6cfab16cb225e9e161568506fd957d92a5f173576a30167";

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
        prompt_len: 105,
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
        prompt_len: 59,
    }
}

fn corpus() -> Vec<Case> {
    let sc =
        |name, skill: Vec<u8>, b, p, len| with_file(name, "skills/memo/SKILL.md", skill, b, p, len);
    let mut cases = vec![
        // 구판 AGENT.md 단독 `---` / `---\r`
        with_file(
            "agent-bare",
            "agents/main/AGENT.md",
            b"---",
            "7cd419ea4a85e95ce1dc11b11d8c8d18e1818611facdf03a9d78d28249e5a374",
            "ae6826257be5c660f50a7425639ebdbbaf9c425b5bcda28f69c0798b37cea89c",
            95,
        ),
        with_file(
            "agent-bare-cr",
            "agents/main/AGENT.md",
            b"---\r",
            "31bc8d17afe88df3fb043f7aeee5a35d015311e6b2a133465319c5a4faf1a484",
            "ae6826257be5c660f50a7425639ebdbbaf9c425b5bcda28f69c0798b37cea89c",
            95,
        ),
        // 구판 SKILL.md 경계 전체(28종)
        sc(
            "clean",
            BODY.into(),
            "fecdc4bd6d311aba77dcc8b64efefa867be7add15901813fad3c86f2ce0c52d6",
            PROMPT_CLEAN,
            105,
        ),
        sc(
            "valid",
            KNOWN.into(),
            "17c35a92d25b31c3414269346d94e979c94e12f5c2181f864548d7e4052e88a4",
            "7add2d312fc63a1b63a856e209f56f1538d56433f39eaadec75e77f70f688ccf",
            153,
        ),
        sc(
            "validcrlf",
            KNOWN.replace('\n', "\r\n").into(),
            "deea37a05543066e1c8d2dc73f64b5a7c7f29b95f70606eef8f03b96207e7443",
            "836a8bd3dd9fa4cd3e13f8d35f5c266d14a7d611e94ca0bbbe4a755b1dd108d7",
            157,
        ),
        sc(
            "unclosed",
            format!("---\n{BODY}").into(),
            "fd4a1fe7248bf9f256ca44be09f46f0f53f4aeffb88f48740ed28a36a126b1c6",
            "efd693c6d9c797b9fd91541edba237136f5116337ad6827d57901dc458aea269",
            109,
        ),
        sc(
            "bare",
            "---".into(),
            "fbdf77fc19dc355b491a34693c0127c5ab6f1ee3b81994e0ecf518094417802b",
            "d154e838a91984320d8dc8d9449bcde6c11f9c071d325e92d6761b4284f576e5",
            83,
        ),
        sc(
            "unknown",
            fm("retired-example: text").into(),
            "13bc2a247ee3024ef8f1aeaa6f0a2d1a6c6351ae54120cd29455b33951601415",
            "956679006ab364efe3ed2c2328d38c199570e5b8efb1a22265739bf783a8b3d2",
            135,
        ),
        sc(
            "wrongname",
            fm("name: elsewhere").into(),
            "852ae3c480be4eecc4f6357bee474383d8a49a761adaff610ec65d7c20a6121c",
            "2667bfe45d84308d78a42b8d2546e07fb535a6e2ea08bc19bbfe182256762f3e",
            129,
        ),
        sc(
            "authority",
            fm("allowed-tools: noop").into(),
            "34c969436d1e5bbe490678d59b928a47c00a7f75d98d05760887fad04acf5c4d",
            "bea16c08ecb664b9864b59b943c35595c956737823992b3df3717e746dc39b1d",
            133,
        ),
        sc(
            "case",
            fm("Allowed-Tools: noop").into(),
            "ccff6f6c6271dc4afa251af509648824dfd0487ffe373cd435f903e57beb45ea",
            "06c4a937780a05ebe84156c7c45f915b55b4fae3bb72b798c4f7b379fecc81fa",
            133,
        ),
        sc(
            "crlf",
            fm("allowed-tools: noop").replace('\n', "\r\n").into(),
            "d9329d2f5b1ac069fa8f39b0a856c83a68bb365ee5230868feee853cd55de342",
            "fd9975d9117e5e4c24cb008d3eec384b15fa6259ab35e8b236e05b13123277f6",
            136,
        ),
        sc(
            "alias",
            fm("description: &desc words\nlicense: *desc").into(),
            "a6749c0abbb5bbbff6316c2f96f08e6e61aeae7c1d02c56d89e5b352019bf75c",
            "97591643a6e9b8a52f132c3aebbce3ae242ee6300ddfa0ab3a7933ffa47653b3",
            153,
        ),
        sc(
            "nested",
            fm("metadata:\n  allowed-tools: noop").into(),
            "b34e6a45f508b737496a65f916bce97b1b77ad976a6aac4ee74a604b87775433",
            "822c33b2c501197623a122363009b4dc0508a142ea695a0e8c7d7eca99e5e19c",
            145,
        ),
        sc(
            "nested_obj",
            fm("metadata:\n  example:\n    allowed-tools: noop").into(),
            "3804fe8aa220332b313672658cc19f892d50f32fca1ca104de88640017ddc332",
            "64a77025bfb402c685b5a55f24070367a86126bcc7080adca060674833588739",
            158,
        ),
        sc(
            "second",
            format!("{KNOWN}---\nallowed-tools: noop\n---\n").into(),
            "4eec061998c49610c2a1cbc2ffbfe7599ef79f9279aef9d02600e5ca43b0c262",
            "5b30699cef7193a724c31dfbd1ff3aff8aa364955def59c7722e2b89d550b40b",
            181,
        ),
        sc(
            "blankdesc",
            fm("description: '   '").into(),
            "e0214504993f9e7dbe3c0e75682e1f0ac06adaac6d1805beab0d528457022436",
            "4bcfa54a8925d85f5b7e13ba10b1f73cffd1ebdcff67067e831c4616147ab267",
            132,
        ),
        sc(
            "duped",
            fm("name: elsewhere\nname: memo").into(),
            "1c3e3aafa98aab82a5a04ee23dfe9b1466aa88e37757960bb0ee61335f90bf78",
            "2add24d9daed285ba498ef2f3e0292d069bdaaffe29a7085579c3678315c5156",
            140,
        ),
        sc(
            "whitespace",
            format!("--- \nallowed-tools: noop\n---\n{BODY}").into(),
            "a9d0de128d14afd07d2f3347fee5e21d6388c05c30bbfd75e11a09e9f2dee0bc",
            "8fb35e277494b6e68471a8dcc91582cf6e59f1ab99885377b36eb3e44bd1935d",
            134,
        ),
        sc(
            "header-name-only",
            "---\nname: memo\n---\nshaped text\n".into(),
            "b06bbf068a59d45976d072899659d192f7cd6074d7ac09952071b4de0b031869",
            "d5b34b966c8314fc924c8e0df3abdf949bc9f4505ca2eafe819dd9616ce1b346",
            110,
        ),
        sc(
            "leading-space",
            "\n  First line. \r\n  second line.\r\n\t\r\n".into(),
            "3d57c64b41f54f86ec70324b43dce891b162f08dfd49d0612133fb01dbcaf9c2",
            "8a69f44400ba1cd57dcb68d28f01f383a4d0571097c15fabbb57e33435e08daa",
            108,
        ),
        sc(
            "delimiter-trailing-space",
            "--- \r\nordinary text\r\n".into(),
            "16b10d64c26df9bc0ccaa70e43fb11fde613edec802e6e75fc4fef69d1aaccfc",
            "2ab8293e0f3cd718946714365cd48b7559999ac7a443333e4b38a3c567372727",
            99,
        ),
        sc(
            "delimiter-in-body",
            "ordinary text\n---\nmore text\n".into(),
            "45580d8e533f52e9bcb5402bcb2d2d3e5a3117f7a1b5f42b533f27b992f054eb",
            "77ca83cf6259d830ab31aa58005a26e6a4b0f615de3904f75eb6e6005856ee72",
            107,
        ),
        sc(
            "big",
            vec![b'x'; 32_769],
            "b65410d55ae2bc4c21e5ddb8e43415c7a7f3fe6491b2bd54f23cd37850d14962",
            "82e5b175edd7438350fe64f3fc5d73b6b00c733a020b55b2397f02da5d6ef59e",
            32_849,
        ),
        sc(
            "exact",
            vec![b'x'; 32_768],
            "e48ff5d49cd6002e769ae1bbd8439da92fb632793302fb5dfd3dfdc8d0134007",
            "120c911a479eb94855cab04eb2e571c4647f25af921095ec2c523232e8cb923b",
            32_848,
        ),
        agg_case(
            "aggeq",
            (16_000, 16_768, false),
            "547ff5e7518a389566660cdf1729479b9bdf1492354b766b09e25a48274f2961",
            "fee423fba7b16f5d6bef1bf501294657deae720d9ff84813a63c219270843030",
            32_983,
        ),
        agg_case(
            "agg",
            (16_384, 16_385, false),
            "b505cee8d969d6e6fde43b3f3f4844ac23bc745f1917d80dcd43cf6df1adefa7",
            "a35e5da259ecdf2b018df3fb261dbbd119832fee10e0ccb1fc94e2639a5a70c6",
            32_984,
        ),
        agg_case(
            "agginc",
            (16_384, 16_385, true),
            "2125b525c13c6be0b4827784c476f0b4a159ee8033eb4764b6aa5372738902a3",
            "a35e5da259ecdf2b018df3fb261dbbd119832fee10e0ccb1fc94e2639a5a70c6",
            32_984,
        ),
        with_file(
            "mixed-skills",
            "agents/main/skills/extra/SKILL.md",
            b"---\nname: extra\n---\nExtra instructions.\n",
            "a7610865cb4e37a566610982a5c90738541d9274454590a337ce74e8c143f2f9",
            "0e482ec96c962d5d711ec02c85adeacb0ba38565a39781147dbe28517510faa7",
            156,
        ),
        with_file(
            "mixed-header-removed",
            "agents/main/skills/extra/SKILL.md",
            b"Extra instructions.\n",
            "cac34bacff078a6db491f72ef0c70b97f649a907580eeddac7ce5ac84e657dca",
            "5c17fc95c96d744f9cd143bf04dce5520737713d20d05f6b88c179b124413938",
            136,
        ),
    ];
    // 구판 triggers.yaml cron 변형 전체(12종: ASCII·블록·접힌·유니코드 공백)
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
    // 구판 preapproved 64쌍·128바이트 조각 + human_approval 16명·102자 경계(3종)
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
        assert_eq!(rev.0, case.bundle_sha, "{}: V1 번들 SHA 불일치", case.name);
        let snap = from_bundle(&bundle, &id, &rev, &limits)
            .unwrap_or_else(|e| panic!("{}: 구판 V1 번들 복원 실패: {e:?}", case.name));
        assert_eq!(
            snap.policy_revision.0, case.policy_sha,
            "{}: 정책 판본 불일치",
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
        .unwrap_or_else(|e| panic!("{}: 프롬프트 조립 실패: {e:?}", case.name));
        assert_eq!(
            assembled.system.len(),
            case.prompt_len,
            "{}: 프롬프트 길이 불일치",
            case.name
        );
        assert_eq!(
            revision_of(assembled.system.as_bytes()).0,
            case.prompt_sha,
            "{}: 프롬프트 내용 불일치",
            case.name
        );
    }
}
