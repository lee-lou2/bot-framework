//! 진행 카드·중단 카드 문구와 순수 판정 함수(외부 I/O 없음).
use bot_core::{id::PrincipalId, intake::RunProgress, profile::Snapshot, state::RunState};
use serde_json::{Value, json};

pub const STOP_CONFIRM: &str = "bot_stop_confirm";
pub const STOP_CONTINUE: &str = "bot_stop_continue";

pub(super) const STOP_ACK: &str = "🛑 중단을 요청하셨어요. 결과는 아래에 남길게요.";
pub(super) const STOP_RESUMED: &str = "▶️ 중단하지 않고 계속 진행할게요.";
pub(super) const STOP_FINISHED: &str = "✔️ 묻는 사이 작업이 끝났어요.";
pub(super) const STOP_GONE: &str = "이미 끝났거나 지난 확인이라 중단할 작업이 없어요.";

const STEPS: [&str; 4] = ["요청 확인", "맥락 파악", "답 작성", "전송"];
const STOP_NOTE: &str = "버튼이 안 보이면 이 스레드에서 저를 멘션하고 \"중단해줘\"라고 보내도 돼요";
const AUTO_SEND_NOTICE: &str = "자동 발송";

/// 글이 **중단 명령 그것뿐**인가 ("중단해줘" · "멈춰!"). 공백 · 문장부호는 무시한다.
/// 다른 말이 섞이면("멈춰있는 서버 원인 알려줘", "중단하지 마") 중단이 아니라 새 질문이다.
pub fn is_stop_phrase(text: &str) -> bool {
    const COMMANDS: [&str; 7] = [
        "중단",
        "중단해",
        "중단해줘",
        "중단해주세요",
        "멈춰",
        "멈춰줘",
        "멈춰주세요",
    ];
    let text = text.split(AUTO_SEND_NOTICE).next().unwrap_or_default();
    let t: String = text.chars().filter(|c| c.is_alphanumeric()).collect();
    COMMANDS.contains(&t.as_str())
}

fn elapsed_label(secs: u64) -> String {
    if secs >= 60 {
        format!("{}분 {}초", secs / 60, secs % 60)
    } else {
        format!("{secs}초")
    }
}

/// 단계 막대: 끝낸 단계 🟩, 지금(멈췄으면 🟥) 단계 🟨, 남은 단계 ⬜.
fn stepper(step: usize, stopped: bool) -> String {
    let now = if stopped { "🟥" } else { "🟨" };
    STEPS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mark = if i < step {
                "🟩"
            } else if i == step {
                now
            } else {
                "⬜"
            };
            format!("{mark} {name}")
        })
        .collect::<Vec<_>>()
        .join("  ›  ")
}

/// 중단 확인(stopped=false) 또는 중단 결과(stopped=true) Block Kit 블록과 요약 글.
pub(super) fn stop_card_blocks(
    step: usize,
    doing: &str,
    elapsed_s: u64,
    stopped: bool,
    run_id: Option<&str>,
) -> (Vec<Value>, String) {
    let bar = stepper(step, stopped);
    let (title, body, note) = if stopped {
        let did = match step {
            0 | 1 => "버린 결과물은 없어요.",
            2 => "쓰던 답은 버렸고 올리지 않았어요.",
            _ => "다 쓴 답은 버렸고 올리지 않았어요.",
        };
        (
            "🛑 중단했어요",
            format!(
                "{bar}\n\n{doing}에 멈췄어요 · {} 진행",
                elapsed_label(elapsed_s)
            ),
            format!("{did} 다시 필요하면 새로 멘션해 주세요."),
        )
    } else {
        let will = match step {
            0 | 1 => "지금 중단해도 버려질 결과물은 아직 없어요.",
            2 => "지금 중단하면 쓰던 답이 버려지고, 아무것도 올라가지 않아요.",
            _ => "지금 중단하면 다 쓴 답이 버려져요. 거의 다 됐어요.",
        };
        (
            "⏳ 작업이 진행 중이에요",
            format!(
                "{bar}\n\n{doing}이에요 · {}째\n\n{will}",
                elapsed_label(elapsed_s)
            ),
            STOP_NOTE.to_owned(),
        )
    };
    let mut blocks = vec![
        json!({"type": "header", "text": {"type": "plain_text", "text": title, "emoji": true}}),
        json!({"type": "section", "text": {"type": "mrkdwn", "text": body}}),
    ];
    if let Some(id) = run_id
        && !stopped
    {
        blocks.push(json!({
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "action_id": STOP_CONFIRM,
                    "style": "danger",
                    "text": {"type": "plain_text", "text": "여기서 중단", "emoji": true},
                    "value": id
                },
                {
                    "type": "button",
                    "action_id": STOP_CONTINUE,
                    "text": {"type": "plain_text", "text": "계속 진행", "emoji": true},
                    "value": id
                }
            ]
        }));
    }
    blocks.push(json!({
        "type": "context",
        "elements": [{"type": "mrkdwn", "text": note}]
    }));
    (blocks, format!("{title} — {doing}"))
}

/// `RunProgress`를 진행 카드 문구(`None`이면 카드 미표시)·중단 카드 단계로 바꾼다(순수).
pub(super) fn describe_progress(
    progress: &RunProgress,
    elapsed_s: u64,
) -> (usize, Option<String>, String) {
    if progress.cancel_requested {
        return (2, Some("중단하고 있어요".into()), "중단하는 중".into());
    }
    if progress.waiting_approval {
        return match &progress.active_tool {
            Some(tool) => (
                2,
                Some(format!("승인을 기다리고 있어요 ({tool})")),
                format!("도구({tool}) 승인 대기 중"),
            ),
            None => (
                2,
                Some("승인을 기다리고 있어요".into()),
                "승인 대기 중".into(),
            ),
        };
    }
    if let Some(tool) = &progress.active_tool {
        return (
            2,
            Some(format!("도구 실행 중: {tool}")),
            format!("도구({tool}) 실행 중"),
        );
    }
    if let Some(agent) = &progress.active_agent {
        let short = agent.rsplit('/').next().unwrap_or(agent);
        return (
            2,
            Some(format!("{short} 역할에서 작업하고 있어요")),
            format!("{short} 역할에서 작업 중"),
        );
    }
    match progress.state {
        RunState::Ready => (0, None, "순서를 기다리는 중".into()),
        RunState::Retry => (
            2,
            Some("잠시 막혀서 다시 시도하고 있어요".into()),
            "막혀서 다시 시도하는 중".into(),
        ),
        RunState::Waiting => (
            2,
            Some("하위 작업을 기다리고 있어요".into()),
            "하위 작업 대기 중".into(),
        ),
        RunState::Leased if elapsed_s >= 15 => (
            2,
            Some("깊이 생각하고 있어요".into()),
            "깊이 생각하는 중".into(),
        ),
        RunState::Leased => (
            2,
            Some("답을 만들고 있어요".into()),
            "답을 만드는 중".into(),
        ),
        RunState::Succeeded | RunState::Dead | RunState::Cancelled => (3, None, "마무리 중".into()),
    }
}

pub(super) fn can_stop(
    snapshot: Option<&Snapshot>,
    actor: &PrincipalId,
    requester: &PrincipalId,
) -> bool {
    if actor == requester {
        return true;
    }
    snapshot
        .and_then(|s| s.members.get(actor))
        .is_some_and(|roles| {
            roles
                .iter()
                .any(|r| matches!(r.as_str(), "admin" | "owner"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_phrase_matches_only_exact_commands() {
        assert!(is_stop_phrase("중단해줘"));
        assert!(is_stop_phrase("중단 해 주세요"));
        assert!(is_stop_phrase("멈춰!"));
        assert!(is_stop_phrase("  멈춰줘. "));
        assert!(is_stop_phrase("중단해줘 자동 발송 · 테스트"));
        assert!(!is_stop_phrase("이거 중단해줘"));
        assert!(!is_stop_phrase("멈춰있는 서버 원인 알려줘"));
        assert!(!is_stop_phrase("중단하지 마"));
        assert!(!is_stop_phrase(""));
    }

    #[test]
    fn stepper_marks_done_current_and_stopped() {
        assert_eq!(
            stepper(2, false),
            "🟩 요청 확인  ›  🟩 맥락 파악  ›  🟨 답 작성  ›  ⬜ 전송"
        );
        assert_eq!(
            stepper(2, true),
            "🟩 요청 확인  ›  🟩 맥락 파악  ›  🟥 답 작성  ›  ⬜ 전송"
        );
    }

    #[test]
    fn describe_progress_prioritizes_cancel_approval_tool_and_agent() {
        let ready = RunProgress {
            state: RunState::Ready,
            cancel_requested: false,
            active_agent: None,
            active_tool: None,
            waiting_approval: false,
        };
        assert_eq!(describe_progress(&ready, 0).1, None);
        let base = RunProgress {
            state: RunState::Leased,
            ..ready
        };
        assert_eq!(
            describe_progress(&base, 3).1.as_deref(),
            Some("답을 만들고 있어요")
        );
        assert_eq!(
            describe_progress(&base, 20).1.as_deref(),
            Some("깊이 생각하고 있어요")
        );
        let with_agent = RunProgress {
            active_agent: Some("main/proxy".into()),
            ..base.clone()
        };
        assert_eq!(
            describe_progress(&with_agent, 5).1.as_deref(),
            Some("proxy 역할에서 작업하고 있어요")
        );
        let with_tool = RunProgress {
            active_tool: Some("proxy-stats.usage-chart".into()),
            ..with_agent.clone()
        };
        assert_eq!(
            describe_progress(&with_tool, 5).1.as_deref(),
            Some("도구 실행 중: proxy-stats.usage-chart")
        );
        let waiting = RunProgress {
            waiting_approval: true,
            ..with_tool.clone()
        };
        assert!(
            describe_progress(&waiting, 5)
                .1
                .as_deref()
                .is_some_and(|s| s.contains("승인을 기다리고"))
        );
        let cancelling = RunProgress {
            cancel_requested: true,
            ..waiting
        };
        assert_eq!(
            describe_progress(&cancelling, 5).1.as_deref(),
            Some("중단하고 있어요")
        );
    }
}
