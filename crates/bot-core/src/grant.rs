//! v2.2 사전 승인(정적 트리거의 쓰기 허가)과 지정한 사람의 승인 증거의 공유 계약.
//!
//! - 사전 승인(`Preapproval`)은 모델 없이 고정 도구·인자로 시작하는 cron 트리거에만 붙는다. 운영자가 명시적으로 활성화할 때 저장소가
//!   `GrantSpec`의 지문에 묶은 승인 ID를 발급하고, 선언이 바뀌거나 사라지면 그 ID는 철회된다(철회된 ID는 되살아나지 않는다).
//! - 사람 승인(`HumanApproval`)은 입구가 인증한 **이번** 이벤트(`Evidence`)의 행위자가 지정한 승인자일 때만 승인 한 단계를 대신한다.
//! - 두 경로 모두 호출을 시작하는 저장소 커밋 안에서 확인한다(`Authority`). 어느 쪽도 모델·위임·다른 후보에는 상속되지 않는다.
//! - 자격 값은 어떤 구조에도 담지 않는다. 대상은 운영자가 정한 정확한 ID 또는 자격의 **이름**이다.

use crate::id::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// 감사·운영 조회에 쓰는 고정 사유. 공개 MCP 오류 모양이나 CLI 거절 낱말은 늘리지 않는다.
pub mod reason {
    /// 활성화 실패: 선언이 계약에 맞지 않는다.
    pub const INVALID: &str = "invalid_preapproval";
    /// 활성화 실패: 판본 고정·대상 결박·쓰기 경유를 보증할 수 없는 도구다.
    pub const UNENFORCEABLE: &str = "preapproval_unenforceable";
    /// 승인 ID가 철회·교체됐거나 정지 세대가 달라졌거나 지문이 지금 선언과 다르다. 새 허가 0.
    pub const STALE: &str = "preapproval_stale";
    /// 트리거가 일시정지 중이다. 새 접수·승인 시작·새 효과 허가 0.
    pub const PAUSED: &str = "trigger_paused";
    /// 효과 요청이 허용 쌍 밖이거나 `{operation,target,params}` 모양이 아니다.
    pub const TARGET_DENIED: &str = "preapproval_target_denied";
    /// 오늘(UTC) 한도를 모두 썼다.
    pub const LIMIT: &str = "preapproval_limit";
    /// 지정한 사람의 증거가 이 호출에 맞지 않아 쓰지 않았다.
    pub const HUMAN_MISMATCH: &str = "human_approval_mismatch";
}

/// 사전 승인 ID. 발급마다 새로 만든다.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct GrantId(pub String);

/// `max_per_day`의 상한. 양의 정수 1..=65535.
pub const MAX_PER_DAY: u32 = 65_535;
/// 허용 쌍 목록의 최대 길이와 한 조각의 최대 바이트(감사·화면에 그대로 싣는다).
pub const MAX_PAIRS: usize = 64;
pub const MAX_PIECE_BYTES: usize = 128;

/// triggers.yaml `preapproved`: 하루 새 외부 쓰기 허가 횟수와 허용하는 (operation, target) 쌍. 쌍은 정렬되고 중복이 없다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Preapproval {
    pub max_per_day: u32,
    pub targets: Vec<(String, String)>,
}

/// triggers.yaml `human_approval`: 승인할 수 있는 사람(중복 없는 사람 멤버 신원, 정렬).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanApproval {
    pub approvers: Vec<PrincipalId>,
}

/// 쌍의 조각 하나가 허용 모양인가: 비어 있지 않고, 제어 문자·공백·와일드카드·URL·경로 구분이 없다.
/// 자격 값이나 URL은 허용하지 않는다(이름이나 정확한 ID만).
pub fn piece_ok(piece: &str) -> bool {
    !piece.is_empty()
        && piece.len() <= MAX_PIECE_BYTES
        && piece.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '#' | '@' | '+')
        })
        && !piece.contains("://")
        && !piece.contains("..")
}

/// 지문에 묶는 재료. 값은 모두 비밀이 아니다.
pub struct GrantInputs<'a> {
    pub profile: &'a ProfileId,
    pub agent: &'a AgentId,
    pub trigger: &'a str,
    /// 정규화한 원천(cron 식·시간대·beat).
    pub source: &'a str,
    pub principal: &'a PrincipalId,
    /// 트리거 주체의 실행 스코프(활성화 때 정한 값).
    pub scopes: &'a BTreeSet<String>,
    pub tool: &'a ToolId,
    /// 설치한 실행물 판본까지 묶은 도구 판본.
    pub tool_revision: &'a Revision,
    /// 고정 인자의 JCS SHA-256.
    pub args_sha256: [u8; 32],
    pub preapproval: &'a Preapproval,
    /// 도구가 해석하는 대상 매핑(자격 이름·선택 환경·egress·목적지)의 지문.
    pub mapping: [u8; 32],
}

/// 운영자가 활성화할 때 승인 ID에 묶는 내용. `fingerprint`가 같으면 같은 승인이다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantSpec {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub trigger: String,
    pub tool: ToolId,
    pub max_per_day: u32,
    pub pairs: Vec<(String, String)>,
    /// 지문 재료를 정규 JSON으로 적은 글. 운영자에게 범위로 보여 주고 기록한다(자격 값 없음).
    pub summary: String,
    pub fingerprint: [u8; 32],
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl GrantSpec {
    pub fn new(input: &GrantInputs<'_>) -> Self {
        let mut pairs = input.preapproval.targets.clone();
        pairs.sort();
        pairs.dedup();
        let material: Value = json!({
            "v": 1,
            "profile": input.profile.0,
            "agent": input.agent.0,
            "trigger": input.trigger,
            "source": input.source,
            "principal": input.principal.0,
            "scopes": input.scopes.iter().collect::<Vec<_>>(),
            "tool": input.tool.0,
            "tool_revision": input.tool_revision.0,
            "args_sha256": hex(&input.args_sha256),
            "pairs": pairs.iter().map(|(o, t)| json!([o, t])).collect::<Vec<_>>(),
            "max_per_day": input.preapproval.max_per_day,
            "mapping": hex(&input.mapping),
        });
        // serde_json의 객체는 키 순서가 정렬이라 같은 재료는 같은 글이 된다.
        let summary = material.to_string();
        let fingerprint: [u8; 32] = Sha256::digest(summary.as_bytes()).into();
        Self {
            profile: input.profile.clone(),
            agent: input.agent.clone(),
            trigger: input.trigger.to_owned(),
            tool: input.tool.clone(),
            max_per_day: input.preapproval.max_per_day,
            pairs,
            summary,
            fingerprint,
        }
    }

    /// 운영자 화면과 `--activate` 출력에 쓰는 한 줄 범위.
    pub fn scope_line(&self) -> String {
        let pairs: Vec<String> = self
            .pairs
            .iter()
            .map(|(operation, target)| format!("{operation}→{target}"))
            .collect();
        format!(
            "{}/{}/{}: 도구 {} · 하루 최대 {}회 · 허용 쌍 {}",
            self.profile.0,
            self.agent.0,
            self.trigger,
            self.tool.0,
            self.max_per_day,
            pairs.join(", ")
        )
    }
}

/// 호출을 시작하는 커밋에서 확인받는 권한. 브로커가 판정하고 저장소가 같은 잠금 안에서 다시 대조한다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Authority {
    /// 사전 승인: 브로커가 지금 선언과 설치 판본에서 다시 계산한 지문. 승인 ID·정지 세대는 접수 때 run에 묶은 값을 저장소가 읽는다.
    Grant { fingerprint: [u8; 32] },
    /// 지정한 사람의 증거: 지금 판본이 승인자로 지정한 사람 중 증거의 행위자. 증거 자체는 접수 때 run에 묶인 값을 저장소가 읽는다.
    Human { actor: PrincipalId },
}

/// `CallLedger::start_with`의 결과.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Granted {
    Started,
    /// 시작하지 않았다(상태 변경 0). 호출은 이 고정 사유로 거절로 닫는다.
    Refused(&'static str),
}

/// 입구가 인증한 이벤트에서 얻은 증거 종류.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceKind {
    /// 멘션·DM 메시지
    Message,
    /// 봇이 참여한 스레드의 사람 답글
    ThreadReply,
    Reaction,
}

impl EvidenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::ThreadReply => "thread_reply",
            Self::Reaction => "reaction",
        }
    }

    pub fn from_recorded(value: &str) -> Option<Self> {
        match value {
            "message" => Some(Self::Message),
            "thread_reply" => Some(Self::ThreadReply),
            "reaction" => Some(Self::Reaction),
            _ => None,
        }
    }
}

/// **이번** 인증된 Slack 이벤트(`AcceptExtras.source`)와 접수 명령(행위자·바인딩·첫 후보 트리거)에서 저장소가 접수 커밋 안에서 만든 승인 증거.
/// 본문·반응 대상 메시지 전문은 담지 않는다. 멤버 신원은 인증된 행위자에서만 오고 본문에서 복원하지 않는다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Evidence {
    pub kind: EvidenceKind,
    /// 설치 팀(인증된 값).
    pub team: String,
    /// 팀 안에서 유일한 이벤트 ID. (팀, 이벤트)당 증거는 한 번만 소비된다.
    pub event_id: String,
    /// 이벤트를 받은 바인딩.
    pub binding: BindingId,
    pub channel: String,
    /// 메시지면 그 메시지의 ts, 반응이면 반응 대상(item) 메시지의 ts.
    pub item_ts: String,
    /// 메시지가 속한 스레드의 root ts. 스레드 밖이면 None.
    pub thread_ts: Option<String>,
    /// 실제 행위자(메시지 작성자·반응한 사람). 접수의 요청자다.
    pub actor: PrincipalId,
    /// 접수가 고른 첫 후보의 트리거 ID(`PassChain.candidates[0]`). 승인자는 지금·고정 판본의 이 트리거 선언에서 읽고, 넘김(PASS)은 증거를 다른 후보로 옮기지 않는다.
    pub trigger: String,
}

/// 운영 화면·API의 사전 승인 한 행. 필드 모양은 관리 API 계약과 같다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreapprovalRow {
    pub profile: String,
    pub agent: String,
    pub trigger: String,
    pub grant_id: String,
    pub epoch: i64,
    pub paused: bool,
    pub used_today: i64,
    pub max_per_day: i64,
}

/// 정지·재개가 가리키는 트리거.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerKey {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub trigger: String,
}

/// 정지·재개의 비교 값: 화면이 본 승인 ID와 정지 세대.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantCas {
    pub grant_id: String,
    pub epoch: i64,
}

/// 정지·재개의 결과. `repeated`면 상태 변경과 감사가 없었다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseOutcome {
    pub row: PreapprovalRow,
    pub repeated: bool,
}

/// 활성 집합을 게시할 때 함께 올리는 사전 승인 입력: 호출한 쪽이 준비한 (프로필, 판본) 목록과 선언에서 계산한 승인 범위.
#[derive(Clone, Debug, Default)]
pub struct GrantPlan {
    /// 호출한 쪽이 번들을 검증·보관하고 복원해 둔 집합(`ActiveRevisions::replace_prepared`의 `prepared`와 같다).
    pub prepared: std::collections::BTreeSet<(ProfileId, Revision)>,
    /// 새 집합에서 사전 승인을 선언한 트리거마다 하나. 비어 있으면 모든 승인을 철회한다.
    pub grants: Vec<GrantSpec>,
}

/// 한 트리거의 정지 상태와 마지막 전환의 기록(응답이 유실된 같은 명령의 재전달을 알아보는 데 쓴다).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PauseState {
    pub epoch: i64,
    pub paused: bool,
    /// 마지막 전환의 직전 epoch·종류(`true` = 정지)·승인 ID. 전환이 없었으면 None.
    pub last: Option<(i64, bool, String)>,
}

/// 정지·재개 명령의 판정.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PauseDecision {
    /// 상태를 바꾼다(epoch + 1, 감사 한 줄).
    Apply,
    /// 이미 그 상태이거나 직전 성공 명령의 재전달이다: 변경·감사 없이 현재 행을 돌려준다.
    Repeated,
    /// 승인이 교체됐거나 그 사이 다른 전환이 있었다(상태가 우연히 같아도 같다).
    Stale,
}

/// `cas`(화면이 본 승인 ID·정지 세대)를 현재와 대조해 정지(`pause`)·재개 명령을 판정한다. 저장소와 가짜 저장소가 같은 규칙을 쓴다.
/// - 승인 ID가 다르면 `Stale`.
/// - 세대가 같으면: 이미 요청한 상태면 `Repeated`, 아니면 `Apply`.
/// - 세대가 하나 앞서 있고 마지막 전환이 정확히 (이 세대에서, 이 종류로, 이 승인으로) 일어났으면 응답 유실 뒤 재전달이라 `Repeated`.
/// - 그 밖의 세대(ABA 포함)는 `Stale`이다. 비교는 가산 없이 한다(외부 입력이 상한이어도 넘치지 않는다).
pub fn decide_pause(
    current_grant: &str,
    state: &PauseState,
    cas: &GrantCas,
    pause: bool,
) -> PauseDecision {
    if current_grant != cas.grant_id || cas.epoch < 0 {
        return PauseDecision::Stale;
    }
    if cas.epoch == state.epoch {
        return if state.paused == pause {
            PauseDecision::Repeated
        } else {
            PauseDecision::Apply
        };
    }
    let replay = cas.epoch.checked_add(1) == Some(state.epoch)
        && state.last.as_ref().is_some_and(|(from, kind, grant)| {
            *from == cas.epoch && *kind == pause && *grant == cas.grant_id
        });
    if replay {
        PauseDecision::Repeated
    } else {
        PauseDecision::Stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        pre: &'a Preapproval,
        scopes: &'a BTreeSet<String>,
        profile: &'a ProfileId,
        agent: &'a AgentId,
        principal: &'a PrincipalId,
        tool: &'a ToolId,
        revision: &'a Revision,
    ) -> GrantInputs<'a> {
        GrantInputs {
            profile,
            agent,
            trigger: "notify",
            source: "cron:0 9 * * *@Asia/Seoul",
            principal,
            scopes,
            tool,
            tool_revision: revision,
            args_sha256: [7; 32],
            preapproval: pre,
            mapping: [9; 32],
        }
    }

    #[test]
    fn the_fingerprint_binds_every_piece_and_ignores_pair_order() {
        let (profile, agent) = (ProfileId("p".into()), AgentId("main".into()));
        let (principal, tool, revision) = (
            PrincipalId("service:reporter".into()),
            ToolId("reports.notify".into()),
            Revision("r1@img".into()),
        );
        let scopes: BTreeSet<String> = ["a".to_owned()].into();
        let pre = Preapproval {
            max_per_day: 24,
            targets: vec![
                ("post".into(), "channel:reports".into()),
                ("post".into(), "channel:alerts".into()),
            ],
        };
        let base = GrantSpec::new(&inputs(
            &pre, &scopes, &profile, &agent, &principal, &tool, &revision,
        ));
        let reordered = Preapproval {
            max_per_day: 24,
            targets: pre.targets.iter().rev().cloned().collect(),
        };
        assert_eq!(
            base.fingerprint,
            GrantSpec::new(&inputs(
                &reordered, &scopes, &profile, &agent, &principal, &tool, &revision
            ))
            .fingerprint,
            "쌍 순서는 지문을 바꾸지 않는다"
        );
        let mut changed = pre.clone();
        changed.max_per_day = 25;
        assert_ne!(
            base.fingerprint,
            GrantSpec::new(&inputs(
                &changed, &scopes, &profile, &agent, &principal, &tool, &revision
            ))
            .fingerprint
        );
        let other = Revision("r1@img2".into());
        assert_ne!(
            base.fingerprint,
            GrantSpec::new(&inputs(
                &pre, &scopes, &profile, &agent, &principal, &tool, &other
            ))
            .fingerprint,
            "설치 판본이 바뀌면 지문이 바뀐다"
        );
        assert!(base.scope_line().contains("post→channel:alerts"));
    }

    fn cas(epoch: i64) -> GrantCas {
        GrantCas {
            grant_id: "g1".into(),
            epoch,
        }
    }

    #[test]
    fn pause_commands_apply_once_and_replays_are_exact() {
        let fresh = PauseState::default();
        assert_eq!(
            decide_pause("g1", &fresh, &cas(0), true),
            PauseDecision::Apply
        );
        assert_eq!(
            decide_pause("g1", &fresh, &cas(0), false),
            PauseDecision::Repeated,
            "이미 풀려 있다"
        );
        let paused = PauseState {
            epoch: 1,
            paused: true,
            last: Some((0, true, "g1".into())),
        };
        assert_eq!(
            decide_pause("g1", &paused, &cas(1), true),
            PauseDecision::Repeated,
            "같은 세대·이미 정지"
        );
        assert_eq!(
            decide_pause("g1", &paused, &cas(0), true),
            PauseDecision::Repeated,
            "응답 유실 뒤 같은 명령"
        );
        assert_eq!(
            decide_pause("g1", &paused, &cas(0), false),
            PauseDecision::Stale,
            "다른 종류"
        );
        assert_eq!(
            decide_pause("g1", &paused, &cas(1), false),
            PauseDecision::Apply
        );
        // pause → resume → pause 뒤 첫 pause의 재전달은 현재 상태가 같아도 낡았다
        let again = PauseState {
            epoch: 3,
            paused: true,
            last: Some((2, true, "g1".into())),
        };
        assert_eq!(
            decide_pause("g1", &again, &cas(0), true),
            PauseDecision::Stale
        );
        assert_eq!(
            decide_pause("g1", &again, &cas(2), true),
            PauseDecision::Repeated
        );
        assert_eq!(
            decide_pause("g2", &again, &cas(3), true),
            PauseDecision::Stale,
            "승인이 교체됐다"
        );
        let max = PauseState {
            epoch: i64::MAX,
            paused: true,
            last: None,
        };
        assert_eq!(
            decide_pause("g1", &max, &cas(i64::MAX), true),
            PauseDecision::Repeated
        );
        assert_eq!(
            decide_pause("g1", &max, &cas(-1), true),
            PauseDecision::Stale
        );
    }

    #[test]
    fn pieces_reject_wildcards_urls_and_blanks() {
        for good in ["post", "channel:reports", "mail-bot", "user@team", "a.b_c"] {
            assert!(piece_ok(good), "{good}");
        }
        for bad in [
            "",
            "*",
            "chan*",
            "https://hooks.example/x",
            "a b",
            "a/b",
            "a..b",
            "line\nbreak",
            "?x",
        ] {
            assert!(!piece_ok(bad), "{bad:?}");
        }
    }
}
