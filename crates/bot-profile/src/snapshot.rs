//! 판본: 검증한 바이트 묶음(bundle)과 SHA-256. 같은 바이트면 언제 어디서 만들어도 같은 판본이다.
use crate::{
    build,
    hash::{canonical_json, sha256_hex},
    issue::{Issue, Issues},
    limits::Limits,
    model::Built,
    scan::Tree,
};
use bot_core::{
    id::{ProfileId, Revision},
    profile::{Adapter, ApprovalRoute, Snapshot},
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

const MAGIC_V1: &[u8] = b"BOTPROFILE1\n";
const MAGIC_V2: &[u8] = b"BOTPROFILE2\n";

/// 묶음 판본(해석 규칙 세대).
/// - `V1`(`BOTPROFILE1\n`): 구판이 보관한 묶음 또는 `SKILL.md` front matter를 쓰지 않는 프로필. 복원 시 구판의 `SKILL.md` 규칙을 쓴다.
/// - `V2`(`BOTPROFILE2\n`): `SKILL.md` front matter를 해석·제거하고 에이전트별 32 KiB 상한을 적용한 묶음.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundleFormat {
    V1,
    V2,
}

impl BundleFormat {
    fn magic(self) -> &'static [u8] {
        match self {
            Self::V1 => MAGIC_V1,
            Self::V2 => MAGIC_V2,
        }
    }
}

/// 파일을 경로순으로 `u32 경로길이 | 경로 | u64 길이 | 바이트`로 이은 정규 바이트.
pub fn encode_with(tree: &Tree, format: BundleFormat) -> Vec<u8> {
    let mut out = format.magic().to_vec();
    out.extend_from_slice(
        &u32::try_from(tree.files.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    for (path, bytes) in &tree.files {
        out.extend_from_slice(&u32::try_from(path.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

#[cfg(test)]
fn encode(tree: &Tree) -> Vec<u8> {
    encode_with(tree, BundleFormat::V1)
}

pub fn revision_of(bundle: &[u8]) -> Revision {
    Revision(sha256_hex(bundle))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<usize> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
}

fn bad(message: &str) -> Issue {
    Issue::error("bundle-invalid", "", message)
}

fn path_ok(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|s| s != "." && s != ".." && crate::text::name_is_clean(s))
}

/// 정규 형태만 받는다(경로 오름차순·중복 없음·꼬리 바이트 없음). 한도는 스캔과 같다.
pub fn decode(bundle: &[u8], limits: &Limits) -> Result<(BundleFormat, Tree), Issue> {
    let (format, rest) = if let Some(rest) = bundle.strip_prefix(MAGIC_V2) {
        (BundleFormat::V2, rest)
    } else if let Some(rest) = bundle.strip_prefix(MAGIC_V1) {
        (BundleFormat::V1, rest)
    } else {
        return Err(bad("프로필 묶음이 아니에요"));
    };
    let mut cur = Cursor {
        bytes: rest,
        pos: 0,
    };
    let count = cur.u32().ok_or_else(|| bad("잘렸어요"))?;
    if count > limits.max_files {
        return Err(bad("파일이 너무 많아요"));
    }
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let (mut total, mut last): (u64, Option<String>) = (0, None);
    for _ in 0..count {
        let path_len = cur.u32().ok_or_else(|| bad("잘렸어요"))?;
        let path = std::str::from_utf8(cur.take(path_len).ok_or_else(|| bad("잘렸어요"))?)
            .map_err(|_| bad("경로가 UTF-8이 아니에요"))?
            .to_owned();
        let len = cur.u64().ok_or_else(|| bad("잘렸어요"))?;
        let limit = if path.ends_with(".yaml") {
            limits.max_yaml_bytes
        } else {
            limits.max_file_bytes
        };
        total = total.saturating_add(len);
        if !path_ok(&path) || len > limit || total > limits.max_profile_bytes {
            return Err(bad(&format!("허용하지 않는 항목이에요: {path}")));
        }
        // 스캔과 같은 깊이·폴더 수 한도를 트리를 만들기 전에 적용한다
        if path.split('/').count() > limits.max_depth {
            return Err(bad(&format!("경로가 너무 깊어요: {path}")));
        }
        for (at, _) in path.match_indices('/') {
            dirs.insert(path[..at].to_owned());
            if dirs.len() > limits.max_dirs {
                return Err(bad("폴더가 너무 많아요"));
            }
        }
        if last.as_ref().is_some_and(|prev| *prev >= path) {
            return Err(bad("경로가 오름차순이 아니에요"));
        }
        let data = cur
            .take(usize::try_from(len).map_err(|_| bad("너무 커요"))?)
            .ok_or_else(|| bad("잘렸어요"))?;
        last = Some(path.clone());
        files.insert(path, data.to_vec());
    }
    if cur.pos != rest.len() {
        return Err(bad("꼬리에 남는 바이트가 있어요"));
    }
    Ok((format, Tree { files, dirs }))
}

/// 승인에 묶는 정책 판본: roles·members·policy와 트리거 주체·승인자 매핑, OpenAI 모델 바인딩의 모델→에이전트·승인 경로 매핑.
/// OpenAI 모델 바인딩이 없는 프로필은 문서에 `openai` 키를 넣지 않는다 — 웹훅 전용 프로필의 판본 바이트가 그대로여야 한다.
pub fn policy_revision(built: &Built) -> Revision {
    let parts = &built.parts;
    let mut triggers = Map::new();
    for agent in built.agents.values() {
        for t in &agent.triggers {
            if let crate::model::Source::Cron {
                principal,
                approver,
                ..
            } = &t.on
            {
                let who =
                    json!({"principal": principal.0, "approver": approver.as_ref().map(|a| &a.0)});
                triggers.insert(format!("{}/{}", agent.id.0, t.id), who);
            }
        }
    }
    let members: Map<String, Value> = parts
        .members
        .values()
        .map(|m| (m.identity.0.clone(), json!(m.roles)))
        .collect();
    let policy_agents: Map<String, Value> = parts
        .policy
        .agents
        .iter()
        .map(|(k, v)| (k.0.clone(), json!(v)))
        .collect();
    let mut doc = json!({
        "roles": parts.roles,
        "members": members,
        "policy": {"scopes": parts.policy.scopes, "agents": policy_agents, "approve": parts.policy.approve},
        "triggers": triggers,
    });
    let openai = openai_mapping(built);
    if !openai.is_empty() {
        doc["openai"] = Value::Object(openai);
    }
    Revision(sha256_hex(canonical_json(&doc).as_bytes()))
}

/// `<별칭>/<모델>` → 바인딩 id·에이전트·승인 경로(HTTP 주체 → 사람과 전달 바인딩). 매핑이 바뀌면 옛 승인으로 새 매핑을 열 수 없다.
fn openai_mapping(built: &Built) -> Map<String, Value> {
    built
        .parts
        .bindings
        .values()
        .filter_map(|b| {
            let Adapter::OpenAi {
                endpoint,
                model,
                approvers,
            } = &b.kind
            else {
                return None;
            };
            let approvers: Map<String, Value> = approvers
                .iter()
                .map(|(requester, route)| {
                    let route = match route {
                        ApprovalRoute::Local { person } => json!({"person": person.0}),
                        ApprovalRoute::Slack { person, binding } => {
                            json!({"person": person.0, "binding": binding.0})
                        }
                    };
                    (requester.0.clone(), route)
                })
                .collect();
            Some((
                format!("{endpoint}/{model}"),
                json!({"binding": b.id.0, "agent": b.agent.0, "approvers": approvers}),
            ))
        })
        .collect()
}

/// 저장해 둔 묶음에서 판본을 다시 만든다. 구조·참조 검증을 다시 하고, 해시가 `revision`과 같은지,
/// profile.yaml의 id가 `profile`과 같은지 확인한다. 호스트 등록부(서버·모델·입구 별칭)는 보지 않는다 —
/// 진행 중이던 옛 판본의 재개용이며, 호출·승인·게시 직전의 현재 권한 검사를 대신하지 않는다.
pub fn from_bundle(
    bundle: &[u8],
    profile: &ProfileId,
    revision: &Revision,
    limits: &Limits,
) -> Result<Snapshot, Vec<Issue>> {
    // 해시부터 본다: 기록한 판본과 다른 바이트는 경로를 해석하기 전에 거절한다
    let room = limits.max_profile_bytes + limits.max_files as u64 * 4096 + 4096;
    if bundle.len() as u64 > room {
        return Err(vec![bad("묶음이 너무 커요")]);
    }
    if revision_of(bundle) != *revision {
        return Err(vec![Issue::error(
            "revision-mismatch",
            "",
            "묶음의 해시가 기록한 판본과 달라요",
        )]);
    }
    let (format, tree) = decode(bundle, limits).map_err(|i| vec![i])?;
    let mut issues = Issues::new(limits.max_issues);
    match build::build_for_bundle(format, &tree, Some(&profile.0), limits, &mut issues) {
        Some(out) => Ok(out.snapshot),
        None => {
            let mut found = issues.into_vec();
            if found.is_empty() {
                found.push(Issue::error("invalid", "", "검증에 실패했어요"));
            }
            Err(found)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Tree {
        Tree::from_files([("b.md", b"two".to_vec()), ("a/x.yaml", b"one".to_vec())])
    }

    #[test]
    fn bundle_round_trips_and_is_canonical() {
        for format in [BundleFormat::V1, BundleFormat::V2] {
            let bundle = encode_with(&tree(), format);
            let (got, back) = decode(&bundle, &Limits::default()).unwrap();
            assert_eq!(got, format);
            assert_eq!(back.files, tree().files);
            assert_eq!(encode_with(&back, format), bundle);
            assert_eq!(revision_of(&bundle).0.len(), 64);
        }
        assert_ne!(
            revision_of(&encode_with(&tree(), BundleFormat::V1)),
            revision_of(&encode_with(&tree(), BundleFormat::V2)),
            "같은 파일 트리라도 묶음 판본(매직)이 다르면 판본 해시가 다르다"
        );
    }

    #[test]
    fn decode_rejects_malformed() {
        let limits = Limits::default();
        let good = encode(&tree());
        assert!(decode(b"nope", &limits).is_err());
        assert!(decode(&good[..good.len() - 1], &limits).is_err());
        let mut tail = good.clone();
        tail.push(0);
        assert!(decode(&tail, &limits).is_err());
        let evil = encode(&Tree::from_files([("../x", b"1".to_vec())]));
        assert!(decode(&evil, &limits).is_err());
        let mut order = MAGIC_V1.to_vec();
        order.extend_from_slice(&2u32.to_be_bytes());
        for p in ["b", "a"] {
            order.extend_from_slice(&1u32.to_be_bytes());
            order.extend_from_slice(p.as_bytes());
            order.extend_from_slice(&0u64.to_be_bytes());
        }
        assert!(decode(&order, &limits).is_err());
    }

    #[test]
    fn mutated_bundles_never_panic() {
        use crate::testrand::Rng;
        let limits = Limits::default();
        let good = encode(&Tree::from_files([
            ("profile.yaml", b"version: 1\n".to_vec()),
            ("agents/main/AGENT.md", "지침\n".as_bytes().to_vec()),
        ]));
        let mut rng = Rng::new(0xabad_1dea);
        for _ in 0..20_000 {
            let mut bytes = good.clone();
            for _ in 0..1 + rng.below(4) {
                match rng.below(3) {
                    0 => {
                        let at = rng.below(bytes.len());
                        bytes[at] = rng.next() as u8;
                    }
                    1 => {
                        let keep = rng.below(bytes.len());
                        bytes.truncate(keep);
                    }
                    _ => bytes.extend((0..rng.below(8)).map(|_| rng.next() as u8)),
                }
                if bytes.is_empty() {
                    bytes.push(0);
                }
            }
            let outcome =
                std::panic::catch_unwind(|| decode(&bytes, &limits).map(|(_, t)| t.files.len()));
            assert!(outcome.is_ok(), "패닉: {bytes:?}");
        }
    }

    /// `(경로, 내용)`을 정규 순서 그대로 이어 붙인다(검증 없이).
    fn raw(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = MAGIC_V1.to_vec();
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (path, data) in entries {
            out.extend_from_slice(&(path.len() as u32).to_be_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&(data.len() as u64).to_be_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn decode_applies_depth_and_dir_limits_before_building_the_tree() {
        let shallow = Limits {
            max_depth: 3,
            ..Limits::default()
        };
        assert!(decode(&raw(&[("a/b/c.md", b"x")]), &shallow).is_ok());
        assert!(decode(&raw(&[("a/b/c/d.md", b"x")]), &shallow).is_err());
        let few = Limits {
            max_dirs: 2,
            ..Limits::default()
        };
        assert!(decode(&raw(&[("a/b/x.md", b"x")]), &few).is_ok());
        let many = raw(&[("a/b/c/x.md", b"x")]);
        assert!(decode(&many, &few).is_err(), "부모 폴더가 셋이면 거절");
        let wide = raw(&[("a/x.md", b"x"), ("b/x.md", b"x"), ("c/x.md", b"x")]);
        assert!(decode(&wide, &few).is_err(), "서로 다른 폴더 수도 센다");
    }

    #[test]
    fn from_bundle_checks_size_and_hash_before_reading_paths() {
        let limits = Limits::default();
        let id = ProfileId("personal".into());
        let evil = raw(&[("../escape", b"x")]);
        let wrong = Revision("0".repeat(64));
        let codes = |r: Result<Snapshot, Vec<Issue>>| {
            r.err().unwrap().iter().map(|i| i.code).collect::<Vec<_>>()
        };
        assert_eq!(
            codes(from_bundle(&evil, &id, &wrong, &limits)),
            ["revision-mismatch"]
        );
        assert_eq!(
            codes(from_bundle(&evil, &id, &revision_of(&evil), &limits)),
            ["bundle-invalid"]
        );
        let tiny = Limits {
            max_profile_bytes: 0,
            max_files: 0,
            ..Limits::default()
        };
        let big = vec![0u8; 10_000];
        assert_eq!(
            codes(from_bundle(&big, &id, &revision_of(&big), &tiny)),
            ["bundle-invalid"]
        );
        // 너무 큰 묶음은 해시를 계산하기 전에 거절한다(해시가 달라도 크기 오류가 먼저 나온다)
        assert_eq!(
            codes(from_bundle(&big, &id, &wrong, &tiny)),
            ["bundle-invalid"]
        );
    }
}
