//! Built-in `memory.search`, `memory.save`, and `memory.forget` broker handlers.
//!
//! - `memory.search` runs hybrid (embedding + lexical) recall under run visibility/taint rules.
//! - `memory.save` and `memory.forget` are restricted to the authenticated human owner in 1:1 DM/CLI.
//! - `memory.save` requires owner approval only when `policy.approve` lists `memory.save`, and only
//!   computes embeddings after approval passes so unapproved text is never sent upstream early.

mod args;
pub(super) use args::judged;

use self::args::{parse_forget, parse_save, parse_search};
use super::{
    BrokerService,
    call::{Flow, Note},
    gate::Gate,
    share::share_approver,
};
use crate::{classify::classify, resolve::Resolved};
use bot_core::{
    builtin::{
        MEMORY_FORGET, MEMORY_FORGET_TOOL, MEMORY_SAVE, MEMORY_SAVE_TOOL, MEMORY_SEARCH,
        MEMORY_SEARCH_TOOL, REVISION,
    },
    command::{CallResult, JsonPayload, ToolCall},
    id::*,
    ledger::{CallPhase, Outcome, StartCall},
    memory::{MemoryQuery, MemorySave},
    port::Error,
    profile::{Snapshot, Tool},
    scope::authorize,
    state::{Risk, RunContext, Visibility},
};
use serde_json::{Value, json};

pub fn is_builtin(tool: &str) -> bool {
    matches!(
        tool,
        MEMORY_SEARCH_TOOL | MEMORY_SAVE_TOOL | MEMORY_FORGET_TOOL
    )
}

pub fn mutating_tool(tool: &str) -> Option<Tool> {
    match tool {
        MEMORY_SAVE_TOOL => Some(MEMORY_SAVE.declaration()),
        MEMORY_FORGET_TOOL => Some(MEMORY_FORGET.declaration()),
        _ => None,
    }
}

impl BrokerService {
    async fn refuse_memory(
        &self,
        attempt: &Attempt,
        ctx: &RunContext,
        tool: &ToolId,
        word: &'static str,
    ) -> Result<CallResult, Error> {
        let _ = self
            .note(
                &attempt.profile,
                &ctx.principal,
                "memory.refuse",
                tool,
                None,
                word,
            )
            .await;
        Err(Error::Denied)
    }

    async fn replayed_memory(
        &self,
        attempt: &Attempt,
        call: &CallId,
        tool: &ToolId,
        sha256: [u8; 32],
        current: &Snapshot,
    ) -> Result<Option<JsonPayload>, Error> {
        let Some(record) = self
            .parts
            .ledger
            .lookup(attempt.clone(), call.clone())
            .await?
        else {
            return Ok(None);
        };
        if record.tool != *tool || record.args_sha256 != sha256 {
            return Err(Error::Conflict);
        }
        let CallPhase::Done(result) = record.phase else {
            return Ok(None);
        };
        self.revision_current(attempt, current).await?;
        Ok(Some(result))
    }

    pub(super) async fn memory_flow(
        &self,
        req: ToolCall,
        note: &mut Note,
    ) -> Result<CallResult, Error> {
        match req.tool.0.as_str() {
            MEMORY_SEARCH_TOOL => self.memory_search_flow(req, note).await,
            MEMORY_SAVE_TOOL => self.memory_save_flow(req, note).await,
            MEMORY_FORGET_TOOL => self.memory_forget_flow(req, note).await,
            _ => Err(Error::Missing),
        }
    }

    async fn memory_search_flow(
        &self,
        req: ToolCall,
        note: &mut Note,
    ) -> Result<CallResult, Error> {
        let ToolCall {
            attempt,
            call,
            tool,
            args,
        } = req;
        let spec = MEMORY_SEARCH.declaration();
        note.risk = Some(Risk::Read);
        note.revision = Some(REVISION.to_owned());
        let ctx = self.parts.ledger.context(attempt.clone()).await?;
        let current = self
            .parts
            .catalog
            .current(&attempt.profile)
            .ok_or(Error::Denied)?;
        let pinned = self
            .parts
            .catalog
            .lookup(&attempt.profile, &ctx.work.revision)
            .ok_or(Error::Missing)?;
        if authorize(&spec.scopes, &ctx, &current, &pinned).is_err() {
            return self
                .refuse_memory(&attempt, &ctx, &tool, "scope_missing")
                .await;
        }
        let parsed = match parse_search(&args) {
            Ok(parsed) => parsed,
            Err(error) => {
                let _ = self
                    .note(
                        &attempt.profile,
                        &ctx.principal,
                        "call.invalid",
                        &tool,
                        None,
                        "args",
                    )
                    .await;
                return Err(error);
            }
        };
        if let Some(done) = self
            .replayed_memory(&attempt, &call, &tool, parsed.sealed.sha256, &current)
            .await?
        {
            return Ok(CallResult::Done(done));
        }
        let record = self
            .parts
            .ledger
            .begin(
                attempt.clone(),
                call.clone(),
                tool.clone(),
                parsed.sealed.sha256,
            )
            .await?;
        let flow = Flow {
            origin: None,
            attempt,
            call,
            tool,
            ctx,
            snapshot: current,
            pinned,
            approver: None,
            resolved: Resolved::new(ToolId(MEMORY_SEARCH_TOOL.into()), spec)
                .ok_or(Error::Invalid)?,
            sealed: parsed.sealed,
            env: Vec::new(),
            memory: None,
            delivery: None,
            route: None,
            card_args: None,
        };
        match &record.phase {
            CallPhase::Done(result) => {
                self.still_current(&flow).await?;
                return Ok(CallResult::Done(result.clone()));
            }
            CallPhase::Denied => return Err(Error::Denied),
            CallPhase::Open { .. } => {}
            CallPhase::Started { .. } | CallPhase::Held { .. } => {
                return Ok(CallResult::Held(flow.call.clone()));
            }
        }
        let _slot = self
            .limiter
            .admit(&flow.attempt.profile, &flow.ctx.principal)
            .map_err(|_| Error::RateLimited)?;
        let embedding = match &self.embedder {
            Some(embedder) => match embedder.embed(&flow.attempt.profile, &parsed.query).await {
                Ok(emb) => emb,
                Err(Error::Stale) => return Err(Error::Stale),
                Err(_) => None,
            },
            None => None,
        };
        self.still_current_local(&flow)?;
        let start = StartCall {
            checked_revision: flow.snapshot.revision.clone(),
            risk: Risk::Read,
            tool_revision: Revision(REVISION.into()),
            attempt: flow.attempt.clone(),
            call: flow.call.clone(),
            approval: None,
            private_recall: flow.ctx.private_memory_read,
            owner: format!(
                "memsearch.{}.{}.{}",
                flow.attempt.run.0, flow.attempt.generation.0, flow.call.0
            ),
        };
        self.parts.ledger.start(start).await?;
        let query = MemoryQuery {
            text: parsed.query,
            embedding,
            limit: parsed.limit,
        };
        match self
            .parts
            .memory
            .search_hybrid(flow.attempt.clone(), query)
            .await
        {
            Ok(hits) => {
                let items: Vec<Value> = hits
                    .into_iter()
                    .map(|h| {
                        json!({
                            "id": h.id.0,
                            "visibility": match h.visibility {
                                Visibility::Private => "private",
                                Visibility::Profile => "profile",
                            },
                            "text": h.text,
                        })
                    })
                    .collect();
                let payload = JsonPayload(json!({ "hits": items }).to_string());
                self.finish(&flow, Outcome::Done(payload.clone())).await?;
                self.still_current(&flow).await?;
                Ok(CallResult::Done(payload))
            }
            Err(error) => {
                let _ = self.finish(&flow, Outcome::Released).await;
                Err(error)
            }
        }
    }

    async fn memory_save_flow(&self, req: ToolCall, note: &mut Note) -> Result<CallResult, Error> {
        let ToolCall {
            attempt,
            call,
            tool,
            args,
        } = req;
        let spec = MEMORY_SAVE.declaration();
        note.risk = Some(Risk::Write);
        note.revision = Some(REVISION.to_owned());
        let ctx = self.parts.ledger.context(attempt.clone()).await?;
        let current = self
            .parts
            .catalog
            .current(&attempt.profile)
            .ok_or(Error::Denied)?;
        let pinned = self
            .parts
            .catalog
            .lookup(&attempt.profile, &ctx.work.revision)
            .ok_or(Error::Missing)?;
        let approver = share_approver(&ctx, &current);
        if approver.is_none() {
            return self.refuse_memory(&attempt, &ctx, &tool, "not_owner").await;
        }
        if authorize(&spec.scopes, &ctx, &current, &pinned).is_err() {
            return self
                .refuse_memory(&attempt, &ctx, &tool, "scope_missing")
                .await;
        }
        let parsed = match parse_save(&args) {
            Ok(parsed) => parsed,
            Err(error) => {
                let _ = self
                    .note(
                        &attempt.profile,
                        &ctx.principal,
                        "call.invalid",
                        &tool,
                        None,
                        "args",
                    )
                    .await;
                return Err(error);
            }
        };
        if let Some(done) = self
            .replayed_memory(&attempt, &call, &tool, parsed.sealed.sha256, &current)
            .await?
        {
            return Ok(CallResult::Done(done));
        }
        let record = self
            .parts
            .ledger
            .begin(
                attempt.clone(),
                call.clone(),
                tool.clone(),
                parsed.sealed.sha256,
            )
            .await?;
        let flow = Flow {
            origin: None,
            attempt,
            call,
            tool,
            ctx,
            snapshot: current,
            pinned,
            approver,
            resolved: Resolved::new(ToolId(MEMORY_SAVE_TOOL.into()), spec.clone())
                .ok_or(Error::Invalid)?,
            sealed: parsed.sealed,
            env: Vec::new(),
            memory: None,
            delivery: None,
            route: None,
            card_args: None,
        };
        let pointer = match &record.phase {
            CallPhase::Done(result) => {
                self.still_current(&flow).await?;
                return Ok(CallResult::Done(result.clone()));
            }
            CallPhase::Denied => return Err(Error::Denied),
            CallPhase::Open { approval } => approval.clone(),
            CallPhase::Started { .. } | CallPhase::Held { .. } => {
                return Ok(CallResult::Held(flow.call.clone()));
            }
        };
        let _slot = self
            .limiter
            .admit(&flow.attempt.profile, &flow.ctx.principal)
            .map_err(|_| Error::RateLimited)?;
        let class = classify(
            &spec,
            &flow.snapshot.policy.approve,
            flow.ctx.private_memory_read,
        );
        let approval = match self.gate(&flow, &class, pointer, None).await? {
            Gate::Go(approval) => approval,
            Gate::Return(result) => return Ok(result),
        };
        // Compute embedding only after approval passes so unapproved tainted text never goes upstream.
        let embedding = match &self.embedder {
            Some(embedder) => match embedder.embed(&flow.attempt.profile, &parsed.text).await {
                Ok(emb) => emb,
                Err(Error::Stale) => return Err(Error::Stale),
                Err(_) => None,
            },
            None => None,
        };
        self.still_current_local(&flow)?;
        let start = StartCall {
            checked_revision: flow.snapshot.revision.clone(),
            risk: class.risk,
            tool_revision: Revision(REVISION.into()),
            attempt: flow.attempt.clone(),
            call: flow.call.clone(),
            approval: approval.map(|boxed| *boxed),
            private_recall: flow.ctx.private_memory_read,
            owner: format!(
                "memsave.{}.{}.{}",
                flow.attempt.run.0, flow.attempt.generation.0, flow.call.0
            ),
        };
        let saved = self
            .parts
            .memory
            .save(
                start,
                MemorySave {
                    text: parsed.text,
                    kind: parsed.kind,
                    supersedes: parsed.supersedes,
                    embedding,
                },
            )
            .await?;
        let result = JsonPayload(
            json!({ "id": saved.id.0, "deduplicated": saved.deduplicated }).to_string(),
        );
        self.still_current(&flow).await?;
        Ok(CallResult::Done(result))
    }

    async fn memory_forget_flow(
        &self,
        req: ToolCall,
        note: &mut Note,
    ) -> Result<CallResult, Error> {
        let ToolCall {
            attempt,
            call,
            tool,
            args,
        } = req;
        let spec = MEMORY_FORGET.declaration();
        note.risk = Some(Risk::Write);
        note.revision = Some(REVISION.to_owned());
        let ctx = self.parts.ledger.context(attempt.clone()).await?;
        let current = self
            .parts
            .catalog
            .current(&attempt.profile)
            .ok_or(Error::Denied)?;
        let pinned = self
            .parts
            .catalog
            .lookup(&attempt.profile, &ctx.work.revision)
            .ok_or(Error::Missing)?;
        let approver = share_approver(&ctx, &current);
        if approver.is_none() {
            return self.refuse_memory(&attempt, &ctx, &tool, "not_owner").await;
        }
        if authorize(&spec.scopes, &ctx, &current, &pinned).is_err() {
            return self
                .refuse_memory(&attempt, &ctx, &tool, "scope_missing")
                .await;
        }
        let (id, sealed) = match parse_forget(&args) {
            Ok(parsed) => parsed,
            Err(error) => {
                let _ = self
                    .note(
                        &attempt.profile,
                        &ctx.principal,
                        "call.invalid",
                        &tool,
                        None,
                        "args",
                    )
                    .await;
                return Err(error);
            }
        };
        if let Some(done) = self
            .replayed_memory(&attempt, &call, &tool, sealed.sha256, &current)
            .await?
        {
            return Ok(CallResult::Done(done));
        }
        let record = self
            .parts
            .ledger
            .begin(attempt.clone(), call.clone(), tool.clone(), sealed.sha256)
            .await?;
        let flow = Flow {
            origin: None,
            attempt,
            call,
            tool,
            ctx,
            snapshot: current,
            pinned,
            approver,
            resolved: Resolved::new(ToolId(MEMORY_FORGET_TOOL.into()), spec.clone())
                .ok_or(Error::Invalid)?,
            sealed,
            env: Vec::new(),
            memory: None,
            delivery: None,
            route: None,
            card_args: None,
        };
        let pointer = match &record.phase {
            CallPhase::Done(result) => {
                self.still_current(&flow).await?;
                return Ok(CallResult::Done(result.clone()));
            }
            CallPhase::Denied => return Err(Error::Denied),
            CallPhase::Open { approval } => approval.clone(),
            CallPhase::Started { .. } | CallPhase::Held { .. } => {
                return Ok(CallResult::Held(flow.call.clone()));
            }
        };
        let _slot = self
            .limiter
            .admit(&flow.attempt.profile, &flow.ctx.principal)
            .map_err(|_| Error::RateLimited)?;
        let class = classify(
            &spec,
            &flow.snapshot.policy.approve,
            flow.ctx.private_memory_read,
        );
        let approval = match self.gate(&flow, &class, pointer, None).await? {
            Gate::Go(approval) => approval,
            Gate::Return(result) => return Ok(result),
        };
        self.still_current_local(&flow)?;
        let start = StartCall {
            checked_revision: flow.snapshot.revision.clone(),
            risk: class.risk,
            tool_revision: Revision(REVISION.into()),
            attempt: flow.attempt.clone(),
            call: flow.call.clone(),
            approval: approval.map(|boxed| *boxed),
            private_recall: flow.ctx.private_memory_read,
            owner: format!(
                "memforget.{}.{}.{}",
                flow.attempt.run.0, flow.attempt.generation.0, flow.call.0
            ),
        };
        let gone = self.parts.memory.forget(start, id).await?;
        let result = JsonPayload(json!({ "id": gone.0 }).to_string());
        self.still_current(&flow).await?;
        Ok(CallResult::Done(result))
    }
}
