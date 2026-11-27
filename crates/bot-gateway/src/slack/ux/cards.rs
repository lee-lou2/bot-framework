//! Pure progress/stop card copy and authorization helpers (no external I/O).
use bot_core::{id::PrincipalId, intake::RunProgress, profile::Snapshot, state::RunState};
use serde_json::{Value, json};

pub const STOP_CONFIRM: &str = "bot_stop_confirm";
pub const STOP_CONTINUE: &str = "bot_stop_continue";

pub(super) const STOP_ACK: &str = "🛑 Stop requested. I'll post the result below.";
pub(super) const STOP_RESUMED: &str = "▶️ Continuing without stopping.";
pub(super) const STOP_FINISHED: &str = "✔️ The task finished while waiting for confirmation.";
pub(super) const STOP_GONE: &str = "This task has already finished or the prompt has expired.";

const STEPS: [&str; 4] = [
    "Request received",
    "Loading context",
    "Drafting reply",
    "Sending",
];
const STOP_NOTE: &str = "If the buttons are not visible, mention me in this thread with \"stop\".";
const AUTO_SEND_NOTICE: &str = "Sent automatically";

/// Matches only standalone stop commands (ignoring case, whitespace, and punctuation)
/// so normal questions containing stop words are not intercepted.
pub fn is_stop_phrase(text: &str) -> bool {
    const COMMANDS: [&str; 6] = [
        "stop",
        "cancel",
        "abort",
        "halt",
        "stopplease",
        "cancelplease",
    ];
    let text = text.split(AUTO_SEND_NOTICE).next().unwrap_or_default();
    let t: String = text
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    COMMANDS.contains(&t.as_str())
}

fn elapsed_label(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

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
            0 | 1 => "No output was discarded.",
            2 => "The draft reply was discarded and nothing was posted.",
            _ => "The completed reply was discarded and nothing was posted.",
        };
        (
            "🛑 Stopped",
            format!(
                "{bar}\n\nStopped at: {doing} · {} elapsed",
                elapsed_label(elapsed_s)
            ),
            format!("{did} Mention me again if you need this later."),
        )
    } else {
        let will = match step {
            0 | 1 => "Stopping now won't discard any output yet.",
            2 => "Stopping now will discard the draft reply and post nothing.",
            _ => "Stopping now will discard the completed reply — it's almost done.",
        };
        (
            "⏳ Task in progress",
            format!(
                "{bar}\n\n{doing} · {} elapsed\n\n{will}",
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
                    "text": {"type": "plain_text", "text": "Stop here", "emoji": true},
                    "value": id
                },
                {
                    "type": "button",
                    "action_id": STOP_CONTINUE,
                    "text": {"type": "plain_text", "text": "Continue", "emoji": true},
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

/// Maps `RunProgress` to `(step, optional_stream_card_title, stop_card_activity)`.
pub(super) fn describe_progress(
    progress: &RunProgress,
    elapsed_s: u64,
) -> (usize, Option<String>, String) {
    if progress.cancel_requested {
        return (2, Some("Stopping...".into()), "Stopping".into());
    }
    if progress.waiting_approval {
        return (
            2,
            Some("Waiting for approval".into()),
            "Waiting for approval".into(),
        );
    }
    if let Some(tool) = &progress.active_tool {
        return (
            2,
            Some(format!("Running tool: {tool}")),
            format!("Running tool ({tool})"),
        );
    }
    if let Some(agent) = &progress.active_agent {
        let short = agent.rsplit('/').next().unwrap_or(agent);
        return (
            2,
            Some(format!("Working as {short}")),
            format!("Working as {short}"),
        );
    }
    match progress.state {
        RunState::Ready => (0, None, "Waiting in queue".into()),
        RunState::Retry => (
            2,
            Some("Temporarily blocked; retrying".into()),
            "Retrying".into(),
        ),
        RunState::Waiting => (
            2,
            Some("Waiting on sub-task".into()),
            "Waiting on sub-task".into(),
        ),
        RunState::Leased if elapsed_s >= 15 => {
            (2, Some("Thinking deeply".into()), "Thinking deeply".into())
        }
        RunState::Leased => (2, Some("Drafting reply".into()), "Drafting reply".into()),
        RunState::Succeeded | RunState::Dead | RunState::Cancelled => {
            (3, None, "Finishing up".into())
        }
    }
}

pub(super) fn can_stop(
    snapshot: Option<&Snapshot>,
    actor: &PrincipalId,
    requester: &PrincipalId,
) -> bool {
    let Some(roles) = snapshot.and_then(|s| s.members.get(actor)) else {
        return false;
    };
    actor == requester
        || roles
            .iter()
            .any(|r| matches!(r.as_str(), "admin" | "owner"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_phrase_matches_only_exact_commands() {
        assert!(is_stop_phrase("stop"));
        assert!(is_stop_phrase("Cancel please"));
        assert!(is_stop_phrase("STOP!"));
        assert!(is_stop_phrase("  abort. "));
        assert!(is_stop_phrase("halt"));
        assert!(is_stop_phrase("stop please Sent automatically · test"));
        assert!(!is_stop_phrase("stop this"));
        assert!(!is_stop_phrase("why is the server stopped"));
        assert!(!is_stop_phrase("don't stop"));
        assert!(!is_stop_phrase(""));
    }

    #[test]
    fn stepper_marks_done_current_and_stopped() {
        assert_eq!(
            stepper(2, false),
            "🟩 Request received  ›  🟩 Loading context  ›  🟨 Drafting reply  ›  ⬜ Sending"
        );
        assert_eq!(
            stepper(2, true),
            "🟩 Request received  ›  🟩 Loading context  ›  🟥 Drafting reply  ›  ⬜ Sending"
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
            Some("Drafting reply")
        );
        assert_eq!(
            describe_progress(&base, 20).1.as_deref(),
            Some("Thinking deeply")
        );
        let with_agent = RunProgress {
            active_agent: Some("main/proxy".into()),
            ..base.clone()
        };
        assert_eq!(
            describe_progress(&with_agent, 5).1.as_deref(),
            Some("Working as proxy")
        );
        let with_tool = RunProgress {
            active_tool: Some("proxy-stats.usage-chart".into()),
            ..with_agent.clone()
        };
        assert_eq!(
            describe_progress(&with_tool, 5).1.as_deref(),
            Some("Running tool: proxy-stats.usage-chart")
        );
        let waiting = RunProgress {
            waiting_approval: true,
            ..with_tool.clone()
        };
        let (_, waiting_card, waiting_doing) = describe_progress(&waiting, 5);
        assert_eq!(waiting_card.as_deref(), Some("Waiting for approval"));
        assert_eq!(waiting_doing, "Waiting for approval");
        let cancelling = RunProgress {
            cancel_requested: true,
            ..waiting
        };
        assert_eq!(
            describe_progress(&cancelling, 5).1.as_deref(),
            Some("Stopping...")
        );
    }

    #[test]
    fn can_stop_requires_current_membership_even_for_the_requester() {
        let mut snap = crate::testing::profile("personal")
            .member("slack:home/U1", &["member"])
            .member("slack:home/U2", &["admin"])
            .member("slack:home/U3", &["owner"])
            .member("slack:home/U4", &["member"])
            .build();
        let u1 = PrincipalId("slack:home/U1".into());
        let u2 = PrincipalId("slack:home/U2".into());
        let u3 = PrincipalId("slack:home/U3".into());
        let u4 = PrincipalId("slack:home/U4".into());
        assert!(can_stop(Some(&snap), &u1, &u1));
        assert!(can_stop(Some(&snap), &u2, &u1));
        assert!(can_stop(Some(&snap), &u3, &u1));
        assert!(!can_stop(Some(&snap), &u4, &u1));
        // Missing snapshot or revoked membership denies even the original requester.
        assert!(!can_stop(None, &u1, &u1));
        std::sync::Arc::make_mut(&mut snap).members.remove(&u1);
        assert!(!can_stop(Some(&snap), &u1, &u1));
    }
}
