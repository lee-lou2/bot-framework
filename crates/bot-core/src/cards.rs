//! Owner-DM approval card delivery contract and strict card line parsers.
//!
//! Cards stay closed until a `Posted` receipt is confirmed against the active set, generation, and expiry.
//! `Unavailable` and `Held` close the request as `APPROVAL_DM_UNAVAILABLE` without auto-reposting and post
//! only a fixed public notice line in the originating channel.

use crate::{
    api::ApprovalBlock,
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId, Revision},
    intake::{NotSentReason, PushTransportId},
    port::Reply,
};
use std::time::Duration;

pub const LABEL_TITLE: &str = "Title";
pub const LABEL_TOOL: &str = "Tool";
pub const LABEL_RISK: &str = "Risk";
pub const LABEL_REASON: &str = "Reason";
pub const LABEL_REQUESTER: &str = "Requester";
pub const LABEL_APPROVER: &str = "Approver";
pub const LABEL_RECIPIENT: &str = "Recipient";
pub const LABEL_CONTENT: &str = "Content";
pub const LABEL_ARGS: &str = "Args";
pub const LABEL_MEMORY_ID: &str = "Memory ID";
pub const LABEL_SHARE_SCOPE: &str = "Scope";
pub const LABEL_BODY: &str = "Body";

/// Fixed channel notices carrying no draft, recipient, hash, buttons, or approval ID.
pub const DM_SENT_NOTICE: &str = "Approval request sent via DM";
pub const DM_UNAVAILABLE_NOTICE: &str =
    "Approval request could not be sent via DM; request closed.";
pub const DM_UNCONFIRMED_NOTICE: &str = "Could not confirm approval DM delivery; request closed.";

/// Audit outcome and unsent-receipt status when an approval DM cannot be delivered.
pub const APPROVAL_DM_UNAVAILABLE: &str = "approval_dm_unavailable";

/// Monotonically increasing attempt number per claim on a `nonce` so stale outcomes never apply after reopen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardKey {
    pub nonce: ApprovalId,
    pub attempt: u32,
}

/// Originating Slack channel and thread metadata for a channel-started owner-DM approval card.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardSource {
    pub channel: String,
    /// Thread root timestamp when the originating conversation is a thread.
    pub thread_ts: Option<String>,
    /// Triggering message timestamp when known (differs from `thread_ts` for replies inside a thread).
    pub message_ts: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardJob {
    pub key: CardKey,
    pub profile: ProfileId,
    /// Slack binding selecting the installation that posts the DM.
    pub binding: BindingId,
    /// Designated human approver (`slack:<alias>/<U...>`).
    pub owner: PrincipalId,
    /// Exact `{nonce,title,lines}` card payload bytes.
    pub card: JsonPayload,
    pub generation: Generation,
    /// Originating Slack channel/thread for channel-started cards (`None` for HTTP-origin cards).
    pub source: Option<CardSource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CardOutcome {
    Posted {
        team: String,
        dm: String,
        ts: String,
    },
    /// Confirmed non-delivery; retried after backoff.
    NotSent {
        reason: NotSentReason,
        retry_after: Option<Duration>,
    },
    /// Delivery impossible; closes as `APPROVAL_DM_UNAVAILABLE`.
    Unavailable,
    /// Delivery outcome uncertain; closes as `APPROVAL_DM_UNAVAILABLE` with no auto-repost.
    Held,
}

/// Confirmed DM delivery receipt for click verification; present only after `Posted`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardReceipt {
    pub team: String,
    pub dm: String,
    pub ts: String,
    pub owner: PrincipalId,
    pub profile: ProfileId,
    pub binding: BindingId,
    /// Originating Slack channel/thread for channel-started cards (`None` for HTTP-origin cards).
    pub source: Option<CardSource>,
}

/// Admission-time evidence inherited by HTTP-origin approval cards (absent for channel-origin cards).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardOrigin {
    /// Original API model binding (root binding even for delegated children).
    pub binding: BindingId,
    /// Requesting HTTP principal (distinct from the human approver).
    pub requester: PrincipalId,
    pub revision: Revision,
    /// Slack `team_id` verified at admission; must match the current installation before first send.
    pub team: Option<String>,
}

pub trait CardOutbox: Send + Sync {
    /// Claims pending jobs for the default transport with a lease; skips ambiguous in-flight `sending` jobs.
    fn claim(&self, limit: u32) -> Reply<'_, Vec<CardJob>>;
    /// Claims pending owner-DM card jobs scoped to a specific push transport.
    fn claim_for<'a>(
        &'a self,
        transport: &'a PushTransportId,
        limit: u32,
    ) -> Reply<'a, Vec<CardJob>>;
    /// Verifies lease, active set, validity, generation, and revision before marking `sending`; only the first sender gets `Ok`.
    fn begin(&self, key: CardKey, checked_revision: Revision) -> Reply<'_, ()>;
    /// Records the delivery outcome idempotently (`Posted` opens the card and enqueues `DM_SENT_NOTICE`; failures close it).
    fn settle(&self, key: CardKey, outcome: CardOutcome) -> Reply<'_, ()>;
    /// Returns fixed admission evidence for HTTP-origin cards only.
    fn origin(&self, nonce: ApprovalId) -> Reply<'_, Option<CardOrigin>>;
    /// Closes the job and approval without posting when the approval route drifted, recording `ApprovalBlock`.
    fn block(&self, key: CardKey, why: ApprovalBlock) -> Reply<'_, ()>;
}

/// Extracts `label: ` and its immediately following `"  "` continuation lines from strict card JSON.
/// Returns `None` on malformed JSON, non-string `approval.lines`, or if `label` does not appear exactly once.
pub fn extract_line(card: &str, label: &str) -> Option<String> {
    let value = crate::json::parse(card)?;
    let lines: Vec<&str> = value
        .get("approval")?
        .get("lines")?
        .as_array()?
        .iter()
        .map(serde_json::Value::as_str)
        .collect::<Option<_>>()?;
    let prefix = format!("{label}: ");
    let mut found: Vec<String> = Vec::new();
    let mut in_target = false;
    for line in lines {
        if let Some(first) = line.strip_prefix(&prefix) {
            found.push(first.to_owned());
            in_target = true;
        } else if in_target && let Some(chunk) = line.strip_prefix("  ") {
            if let Some(last) = found.last_mut() {
                last.push_str(chunk);
            }
        } else {
            in_target = false;
        }
    }
    match found.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// Splits `approval.title` and `approval.lines` into `(label, value)` rows for admin rendering.
/// If `joined_label` is given, collects its line and immediate `"  "` continuations separately and returns them only on a single match.
pub fn split_render(
    card: &str,
    joined_label: Option<&str>,
) -> (Vec<(String, String)>, Option<String>) {
    let Some(serde_json::Value::Object(map)) = crate::json::parse(card) else {
        return (vec![], None);
    };
    let Some(serde_json::Value::Object(approval)) = map.get("approval") else {
        return (vec![], None);
    };
    let mut out = Vec::new();
    if let Some(title) = approval.get("title").and_then(serde_json::Value::as_str) {
        out.push((LABEL_TITLE.to_owned(), title.to_owned()));
    }
    let lines: Vec<&str> = match approval.get("lines") {
        Some(value) => {
            let Some(arr) = value.as_array() else {
                return (vec![], None);
            };
            let Some(lines) = arr
                .iter()
                .map(serde_json::Value::as_str)
                .collect::<Option<Vec<_>>>()
            else {
                return (vec![], None);
            };
            lines
        }
        None => Vec::new(),
    };
    let prefix = joined_label.map(|label| format!("{label}: "));
    let mut joined: Vec<String> = Vec::new();
    let mut in_target = false;
    for line in lines {
        if let Some(prefix) = &prefix {
            if let Some(first) = line.strip_prefix(prefix.as_str()) {
                joined.push(first.to_owned());
                in_target = true;
                continue;
            }
            if in_target && let Some(chunk) = line.strip_prefix("  ") {
                if let Some(last) = joined.last_mut() {
                    last.push_str(chunk);
                }
                continue;
            }
        }
        in_target = false;
        out.push(match line.split_once(": ") {
            Some((label, value)) => (label.to_owned(), value.to_owned()),
            None => (String::new(), line.to_owned()),
        });
    }
    let single = match joined.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    };
    (out, single)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card_json(lines: &[&str]) -> String {
        json!({
            "approval": {
                "nonce": "n1",
                "title": "Approval request",
                "lines": lines,
            }
        })
        .to_string()
    }

    #[test]
    fn extract_line_joins_immediate_continuations_only() {
        let card = card_json(&[
            "Tool: mail.send",
            "Content: \"hello ",
            "  world\"",
            "Args: {\"a\":\"long-",
            "  tail\"}",
        ]);
        assert_eq!(
            extract_line(&card, LABEL_CONTENT).as_deref(),
            Some("\"hello world\"")
        );
        assert_eq!(
            extract_line(&card, LABEL_ARGS).as_deref(),
            Some("{\"a\":\"long-tail\"}")
        );
        assert_eq!(
            extract_line(&card, LABEL_TOOL).as_deref(),
            Some("mail.send")
        );
        assert_eq!(extract_line(&card, "Missing"), None);
    }

    #[test]
    fn extract_line_refuses_duplicate_labels_and_stray_continuations() {
        let dup = card_json(&["Args: {\"a\":1}", "Args: {\"a\":2}"]);
        assert_eq!(extract_line(&dup, LABEL_ARGS), None);
        let stray = card_json(&["Tool: x", "  stray", "Args: {\"ok\":true}"]);
        assert_eq!(
            extract_line(&stray, LABEL_ARGS).as_deref(),
            Some("{\"ok\":true}")
        );
        assert_eq!(extract_line(&stray, LABEL_TOOL).as_deref(), Some("xstray"));
        let non_str = json!({"approval":{"lines":["Args: 1", 42]}}).to_string();
        assert_eq!(extract_line(&non_str, LABEL_ARGS), None);
        let dup_key = r#"{"approval":{"lines":["Args: 1"]},"approval":{"lines":["Args: 2"]}}"#;
        assert_eq!(extract_line(dup_key, LABEL_ARGS), None);
    }

    #[test]
    fn split_render_does_not_corrupt_body_with_later_continuation_lines() {
        let card = card_json(&[
            "Tool: memory.share",
            "Body: \"secret ",
            "  body\"",
            "Note: first",
            "  second",
        ]);
        let (rows, joined) = split_render(&card, Some(LABEL_BODY));
        assert_eq!(joined.as_deref(), Some("\"secret body\""));
        assert_eq!(
            rows,
            vec![
                (LABEL_TITLE.into(), "Approval request".into()),
                (LABEL_TOOL.into(), "memory.share".into()),
                ("Note".into(), "first".into()),
                (String::new(), "  second".into()),
            ]
        );
        let dup = card_json(&["Body: \"a\"", "Body: \"b\""]);
        assert_eq!(split_render(&dup, Some(LABEL_BODY)).1, None);
        let bad = json!({"approval": {"title": "t", "lines": ["Body: \"a\"", 42]}}).to_string();
        assert_eq!(split_render(&bad, Some(LABEL_BODY)), (vec![], None));
    }
}
