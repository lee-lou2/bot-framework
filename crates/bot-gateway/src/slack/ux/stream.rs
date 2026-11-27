//! Per-thread Slack progress stream ownership, sequential handoff across overlapping runs, and clean closure.
//!
//! At most one stream card is open per `(channel, thread_ts)`. The owning run keeps the stream until it finishes
//! or posts a reply, while other concurrent runs record their latest status in arrival order and take over once
//! the owner retires. Lock order: `ThreadStream` slot mutex → `SlackUx::active` leaf mutex.
use super::{SlackUx, TrackedRun};
use crate::slack::api::{Body, Raw};
use bot_core::id::{ProfileId, RunId};
use serde_json::json;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct OpenStream {
    run: RunId,
    ts: String,
    title: String,
    profile: ProfileId,
    workspace: String,
}

struct QueuedRun {
    seq: u64,
    run: RunId,
    profile: ProfileId,
    workspace: String,
    team_id: String,
    user_id: String,
    status: Option<String>,
}

#[derive(Default)]
pub(super) struct ThreadStream {
    open: Option<OpenStream>,
    // Active display candidates ordered by arrival `seq` (oldest first).
    queue: Vec<QueuedRun>,
    // Runs whose final reply has started or finished; blocks late polls from re-entering the queue or opening a card.
    closed: HashSet<RunId>,
    // In-flight Slack replies in this thread; defers opening the next run's stream until posted below the reply.
    posting: Arc<AtomicUsize>,
}

impl ThreadStream {
    pub(super) fn is_empty(&self) -> bool {
        self.open.is_none()
            && self.queue.is_empty()
            && self.closed.is_empty()
            && self.posting.load(Ordering::Acquire) == 0
    }
}

/// Defers reopening the next queued run's progress stream until an in-flight Slack reply finishes posting.
pub(in crate::slack) struct ReplyStreamGuard {
    ux: Option<Arc<SlackUx>>,
    channel: String,
    thread_ts: String,
    posting: Option<Arc<AtomicUsize>>,
}

impl ReplyStreamGuard {
    fn noop() -> Self {
        Self {
            ux: None,
            channel: String::new(),
            thread_ts: String::new(),
            posting: None,
        }
    }

    pub(in crate::slack) async fn finish(mut self) {
        if let Some(posting) = self.posting.take() {
            posting.fetch_sub(1, Ordering::AcqRel);
        }
        let Some(ux) = self.ux.take() else { return };
        ux.resume_stream(&self.channel, &self.thread_ts).await;
    }
}

impl Drop for ReplyStreamGuard {
    fn drop(&mut self) {
        if let Some(posting) = self.posting.take() {
            posting.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn log_passive_failure(method: &str, raw: &Raw) {
    match raw {
        Raw::Ok(_) => {}
        Raw::Error(code) => {
            tracing::debug!(method, error = %code, "Slack progress API call rejected");
        }
        Raw::RateLimited(wait) => {
            tracing::debug!(
                method,
                wait_ms = wait.as_millis() as u64,
                "Slack progress API rate limited"
            );
        }
        Raw::BeforeWrite | Raw::TokenRejected | Raw::Unknown => {
            tracing::debug!(method, ?raw, "Slack progress API call failed");
        }
    }
}

fn task_chunk(title: &str, status: &str) -> serde_json::Value {
    json!({
        "type": "task_update",
        "id": "status",
        "title": title,
        "status": status
    })
}

impl SlackUx {
    /// Records `info.run`'s latest progress status and updates the thread's single stream if this run owns it.
    pub(super) async fn update_status(&self, seq: u64, info: &TrackedRun, status: Option<&str>) {
        let status = status.filter(|s| !s.is_empty()).map(str::to_owned);
        let slot = self.stream_slot(&info.channel, &info.thread_ts);
        let mut guard = slot.lock().await;
        if guard.closed.contains(&info.run) {
            return;
        }
        if let Some(entry) = guard.queue.iter_mut().find(|q| q.run == info.run) {
            entry.status = status;
        } else {
            let pos = guard
                .queue
                .iter()
                .position(|q| q.seq > seq)
                .unwrap_or(guard.queue.len());
            guard.queue.insert(
                pos,
                QueuedRun {
                    seq,
                    run: info.run.clone(),
                    profile: info.profile.clone(),
                    workspace: info.workspace.clone(),
                    team_id: info.team.clone(),
                    user_id: info.user.clone(),
                    status,
                },
            );
        }
        self.sync_stream(&mut guard, &info.channel, &info.thread_ts, true)
            .await;
    }

    /// Closes `run`'s progress stream (if it owns the thread's open card) before posting a reply and defers
    /// opening the next queued run's card until `ReplyStreamGuard::finish` runs after the reply is posted.
    pub(in crate::slack) async fn begin_reply(
        self: &Arc<Self>,
        channel: &str,
        thread_ts: &str,
        run: &RunId,
        mark_closed: bool,
    ) -> ReplyStreamGuard {
        let slot = if mark_closed && self.active_by_run(&run.0).is_some() {
            Some(self.stream_slot(channel, thread_ts))
        } else {
            self.existing_stream_slot(channel, thread_ts)
        };
        let Some(slot) = slot else {
            return ReplyStreamGuard::noop();
        };
        let mut guard = slot.lock().await;
        if mark_closed {
            guard.queue.retain(|q| q.run != *run);
            if self.active_by_run(&run.0).is_some() {
                guard.closed.insert(run.clone());
            }
        }
        let posting = guard.posting.clone();
        posting.fetch_add(1, Ordering::AcqRel);
        let reply_guard = ReplyStreamGuard {
            ux: Some(self.clone()),
            channel: channel.to_owned(),
            thread_ts: thread_ts.to_owned(),
            posting: Some(posting),
        };
        if guard.open.as_ref().is_some_and(|o| o.run == *run)
            && let Some(open) = guard.open.take()
        {
            let keep_processing = !mark_closed
                || !guard.queue.is_empty()
                || self.has_other_unclosed_active(channel, thread_ts, run, &guard.closed);
            self.close_and_delete_stream(&open, channel, keep_processing)
                .await;
        }
        drop(guard);
        drop(slot);
        reply_guard
    }

    async fn resume_stream(&self, channel: &str, thread_ts: &str) {
        let Some(slot) = self.existing_stream_slot(channel, thread_ts) else {
            return;
        };
        let mut guard = slot.lock().await;
        self.sync_stream(&mut guard, channel, thread_ts, true).await;
        drop(guard);
        drop(slot);
        self.prune_stream_slot(channel, thread_ts);
    }

    /// Retires `run` when its watcher exits, closing its owned stream and handing off to the next queued run
    /// unless `reopen` is false (graceful shutdown).
    pub(super) async fn retire_run(
        &self,
        channel: &str,
        thread_ts: &str,
        run: &RunId,
        reopen: bool,
    ) {
        let Some(slot) = self.existing_stream_slot(channel, thread_ts) else {
            return;
        };
        let mut guard = slot.lock().await;
        guard.queue.retain(|q| q.run != *run);
        guard.closed.remove(run);
        if guard.open.as_ref().is_some_and(|o| o.run == *run)
            && let Some(open) = guard.open.take()
        {
            let keep_processing = reopen
                && (!guard.queue.is_empty()
                    || self.has_other_unclosed_active(channel, thread_ts, run, &guard.closed));
            self.close_and_delete_stream(&open, channel, keep_processing)
                .await;
        }
        if reopen {
            self.sync_stream(&mut guard, channel, thread_ts, true).await;
        }
        drop(guard);
        drop(slot);
        self.prune_stream_slot(channel, thread_ts);
    }

    async fn sync_stream(
        &self,
        guard: &mut ThreadStream,
        channel: &str,
        thread_ts: &str,
        allow_open: bool,
    ) {
        if let Some(open) = guard.open.as_ref() {
            let desired = guard
                .queue
                .iter()
                .find(|q| q.run == open.run)
                .and_then(|q| q.status.clone());
            match desired {
                Some(title) if title == open.title => return,
                Some(title) => {
                    let Some(token) = self.creds.bot_token(&open.profile, &open.workspace).await
                    else {
                        return;
                    };
                    let ts = open.ts.clone();
                    let body = json!({
                        "channel": channel,
                        "ts": ts,
                        "chunks": [task_chunk(&title, "in_progress")]
                    });
                    let raw = self
                        .creds
                        .call_passive("chat.appendStream", &token, Body::Json(&body))
                        .await;
                    match raw {
                        Raw::Ok(_) => {
                            if let Some(open) = guard.open.as_mut() {
                                open.title = title;
                            }
                        }
                        Raw::Error(_) => {
                            log_passive_failure("chat.appendStream", &raw);
                            guard.open = None;
                            let del = json!({"channel": channel, "ts": ts});
                            let del_raw = self
                                .creds
                                .call_passive("chat.delete", &token, Body::Json(&del))
                                .await;
                            log_passive_failure("chat.delete", &del_raw);
                        }
                        _ => {
                            log_passive_failure("chat.appendStream", &raw);
                        }
                    }
                    return;
                }
                None => {
                    if let Some(open) = guard.open.take() {
                        let keep_processing = allow_open
                            && (guard.queue.iter().any(|q| q.status.is_some())
                                || self.has_other_unclosed_active(
                                    channel,
                                    thread_ts,
                                    &open.run,
                                    &guard.closed,
                                ));
                        self.close_and_delete_stream(&open, channel, keep_processing)
                            .await;
                    }
                }
            }
        }
        if !allow_open || guard.posting.load(Ordering::Acquire) > 0 {
            return;
        }
        let Some((run, profile, workspace, team_id, user_id, title)) =
            guard.queue.iter().find_map(|q| {
                q.status.as_ref().map(|title| {
                    (
                        q.run.clone(),
                        q.profile.clone(),
                        q.workspace.clone(),
                        q.team_id.clone(),
                        q.user_id.clone(),
                        title.clone(),
                    )
                })
            })
        else {
            return;
        };
        let Some(token) = self.creds.bot_token(&profile, &workspace).await else {
            return;
        };
        let mut body = json!({
            "channel": channel,
            "thread_ts": thread_ts,
            "chunks": [task_chunk(&title, "in_progress")]
        });
        if !team_id.is_empty() {
            body["recipient_team_id"] = json!(team_id);
        }
        if !user_id.is_empty() {
            body["recipient_user_id"] = json!(user_id);
        }
        let raw = self
            .creds
            .call_passive("chat.startStream", &token, Body::Json(&body))
            .await;
        match raw {
            Raw::Ok(val) => {
                if let Some(ts) = val["ts"].as_str() {
                    guard.open = Some(OpenStream {
                        run,
                        ts: ts.to_owned(),
                        title,
                        profile,
                        workspace,
                    });
                }
            }
            _ => log_passive_failure("chat.startStream", &raw),
        }
    }

    async fn close_and_delete_stream(
        &self,
        open: &OpenStream,
        channel: &str,
        keep_processing: bool,
    ) {
        let Some(token) = self.creds.bot_token(&open.profile, &open.workspace).await else {
            return;
        };
        // Mark the task complete using chunks mode so Slack does not flash an error icon or reject with streaming_mode_mismatch.
        let mut stop_body = json!({
            "channel": channel,
            "ts": open.ts,
            "chunks": [task_chunk(&open.title, "complete")]
        });
        if keep_processing {
            stop_body["session_status"] = json!("processing");
        }
        let stop_raw = self
            .creds
            .call_passive("chat.stopStream", &token, Body::Json(&stop_body))
            .await;
        log_passive_failure("chat.stopStream", &stop_raw);
        let del_body = json!({"channel": channel, "ts": open.ts});
        let del_raw = self
            .creds
            .call_passive("chat.delete", &token, Body::Json(&del_body))
            .await;
        log_passive_failure("chat.delete", &del_raw);
    }
}
