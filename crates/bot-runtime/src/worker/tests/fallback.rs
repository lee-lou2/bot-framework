//! Worker unit tests for provider fallback across Loop and Step runners and zero-duplicate-effect guards.

use super::fakes::*;
use crate::{
    host::{Models, Relays},
    model::{
        ModelError, ModelRequest, ModelRunner, ModelStep, RelayChannel, RelayGrant, RunnerKind,
        StepFuture,
    },
    worker::Outcome,
};
use bot_core::{
    command::{JsonPayload, Retry},
    id::{AgentId, Attempt, ProfileId},
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

struct LoopScript {
    outcomes: Mutex<VecDeque<Result<ModelStep, ModelError>>>,
    seen_models: Mutex<Vec<String>>,
}

impl LoopScript {
    fn new(outcomes: impl IntoIterator<Item = Result<ModelStep, ModelError>>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            seen_models: Mutex::new(Vec::new()),
        })
    }
}

impl ModelRunner for LoopScript {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        lock(&self.seen_models).push(request.model);
        let next = lock(&self.outcomes).pop_front();
        Box::pin(async move {
            next.unwrap_or(Err(ModelError::Failed {
                code: "no_script",
                transient: false,
            }))
        })
    }
}

struct ChainModels {
    runners: BTreeMap<String, Arc<dyn ModelRunner>>,
    chains: BTreeMap<String, Vec<String>>,
}

impl Models for ChainModels {
    fn runner(&self, _: &ProfileId, alias: &str) -> Option<Arc<dyn ModelRunner>> {
        self.runners.get(alias).cloned()
    }
    fn fallbacks(&self, _: &ProfileId, alias: &str) -> Vec<String> {
        self.chains.get(alias).cloned().unwrap_or_default()
    }
}

struct TrackRelays {
    opened: Mutex<Vec<String>>,
    revoked: Arc<Mutex<Vec<String>>>,
}

struct DropTag(String, Arc<Mutex<Vec<String>>>);
impl Drop for DropTag {
    fn drop(&mut self) {
        lock(&self.1).push(self.0.clone());
    }
}

impl Relays for TrackRelays {
    fn open(&self, _: &Attempt, _: &AgentId, alias: &str) -> Option<RelayGrant> {
        lock(&self.opened).push(alias.to_owned());
        Some(RelayGrant::new(
            RelayChannel {
                name: "relay".into(),
                base_url: "http://127.0.0.1:9999".into(),
                token: format!("tok-{alias}"),
            },
            DropTag(alias.to_owned(), self.revoked.clone()),
        ))
    }
}

fn loop_rig(
    primary: Arc<dyn ModelRunner>,
    backup: Arc<dyn ModelRunner>,
    relays: Arc<dyn Relays>,
) -> Rig {
    let models = Arc::new(ChainModels {
        runners: BTreeMap::from([
            ("assistant".to_owned(), primary),
            ("backup".to_owned(), backup),
        ]),
        chains: BTreeMap::from([("assistant".to_owned(), vec!["backup".to_owned()])]),
    });
    let mut rig = rig_models(models, config());
    let mut deps = (*rig.worker.deps).clone();
    deps.relays = relays;
    rig.worker = crate::worker::Worker::new(deps, config());
    rig
}

#[tokio::test]
async fn loop_model_failure_at_zero_steps_revokes_relay_and_falls_back_cleanly() {
    let primary = LoopScript::new([Err(ModelError::Failed {
        code: "rate_limited",
        transient: true,
    })]);
    let backup = LoopScript::new([say("answered by fallback")]);
    let relays = Arc::new(TrackRelays {
        opened: Mutex::new(Vec::new()),
        revoked: Arc::new(Mutex::new(Vec::new())),
    });
    let rig = loop_rig(primary.clone(), backup.clone(), relays.clone());
    let ctx = agent_run("r-fb-ok");
    *lock(&rig.ledger.ctx) = Some(ctx.clone());

    assert_eq!(rig.worker.execute(ctx).await, Outcome::Succeeded);
    assert_eq!(*lock(&primary.seen_models), vec!["assistant"]);
    assert_eq!(*lock(&backup.seen_models), vec!["backup"]);
    assert_eq!(*lock(&relays.opened), vec!["assistant", "backup"]);
    assert_eq!(*lock(&relays.revoked), vec!["assistant", "backup"]);
    assert!(
        lock(&rig.runs.state).finished[0]
            .result
            .0
            .contains("answered by fallback")
    );
}

#[tokio::test]
async fn loop_failure_after_committed_steps_fails_closed_without_fallback() {
    let primary = LoopScript::new([Err(ModelError::Failed {
        code: "rate_limited",
        transient: true,
    })]);
    let backup = LoopScript::new([say("must not run")]);
    let relays = Arc::new(TrackRelays {
        opened: Mutex::new(Vec::new()),
        revoked: Arc::new(Mutex::new(Vec::new())),
    });
    let rig = loop_rig(primary.clone(), backup.clone(), relays.clone());
    let ctx = agent_run("r-fb-committed");
    *lock(&rig.ledger.ctx) = Some(ctx.clone());
    lock(&rig.ledger.steps).insert("cli-group-0".into(), JsonPayload("{}".into()));

    assert_eq!(rig.worker.execute(ctx).await, Outcome::Failed(Retry::Dead));
    assert_eq!(
        lock(&rig.runs.state).failed,
        vec![bot_core::command::Failure::Transient("rate_limited")]
    );
    assert_eq!(*lock(&primary.seen_models), vec!["assistant"]);
    assert!(lock(&backup.seen_models).is_empty());
}

#[tokio::test]
async fn parked_loop_run_never_triggers_fallback() {
    let primary = LoopScript::new([Err(ModelError::Parked)]);
    let backup = LoopScript::new([say("must not run")]);
    let relays = Arc::new(TrackRelays {
        opened: Mutex::new(Vec::new()),
        revoked: Arc::new(Mutex::new(Vec::new())),
    });
    let rig = loop_rig(primary.clone(), backup.clone(), relays);
    let ctx = agent_run("r-fb-parked");
    *lock(&rig.ledger.ctx) = Some(ctx.clone());

    assert_eq!(rig.worker.execute(ctx).await, Outcome::Parked);
    assert!(lock(&backup.seen_models).is_empty());
}

#[tokio::test]
async fn step_to_step_fallback_resumes_from_committed_checkpoint() {
    let primary = ScriptedModel::new(vec![
        calls("checking agenda", &[("c1", "agenda.list", "{}")]),
        Err(ModelError::Failed {
            code: "rate_limited",
            transient: true,
        }),
    ]);
    let backup = ScriptedModel::new(vec![say("completed by step fallback")]);
    let models = Arc::new(ChainModels {
        runners: BTreeMap::from([
            (
                "assistant".to_owned(),
                primary.clone() as Arc<dyn ModelRunner>,
            ),
            ("backup".to_owned(), backup.clone() as Arc<dyn ModelRunner>),
        ]),
        chains: BTreeMap::from([("assistant".to_owned(), vec!["backup".to_owned()])]),
    });
    let rig = rig_models(models, config());
    let ctx = agent_run("r-fb-step");
    *lock(&rig.ledger.ctx) = Some(ctx.clone());

    assert_eq!(rig.worker.execute(ctx).await, Outcome::Succeeded);
    assert_eq!(rig.broker.calls().len(), 1);
    assert_eq!(primary.requests().len(), 2);
    assert_eq!(backup.requests().len(), 1);
    assert_eq!(backup.requests()[0].model, "backup");
    assert_eq!(backup.requests()[0].exchanges.len(), 1);
    assert!(
        lock(&rig.runs.state).finished[0]
            .result
            .0
            .contains("completed by step fallback")
    );
}

#[tokio::test]
async fn non_fallback_errors_never_invoke_backup() {
    let primary = LoopScript::new([Err(ModelError::Failed {
        code: "too_many_turns",
        transient: false,
    })]);
    let backup = LoopScript::new([say("must not run")]);
    let relays = Arc::new(TrackRelays {
        opened: Mutex::new(Vec::new()),
        revoked: Arc::new(Mutex::new(Vec::new())),
    });
    let rig = loop_rig(primary.clone(), backup.clone(), relays);
    let ctx = agent_run("r-fb-nonfb");
    *lock(&rig.ledger.ctx) = Some(ctx.clone());

    assert_eq!(rig.worker.execute(ctx).await, Outcome::Failed(Retry::Dead));
    assert!(lock(&backup.seen_models).is_empty());
}

#[tokio::test]
async fn multi_hop_fallback_chain_tries_each_provider_in_order() {
    let p0 = LoopScript::new([Err(ModelError::Failed {
        code: "rate_limited",
        transient: true,
    })]);
    let p1 = LoopScript::new([Err(ModelError::Failed {
        code: "provider_error",
        transient: true,
    })]);
    let p2 = LoopScript::new([say("answered by third hop")]);
    let relays = Arc::new(TrackRelays {
        opened: Mutex::new(Vec::new()),
        revoked: Arc::new(Mutex::new(Vec::new())),
    });
    let models = Arc::new(ChainModels {
        runners: BTreeMap::from([
            ("assistant".to_owned(), p0.clone() as Arc<dyn ModelRunner>),
            ("backup-1".to_owned(), p1.clone() as Arc<dyn ModelRunner>),
            ("backup-2".to_owned(), p2.clone() as Arc<dyn ModelRunner>),
        ]),
        chains: BTreeMap::from([(
            "assistant".to_owned(),
            vec!["backup-1".to_owned(), "backup-2".to_owned()],
        )]),
    });
    let mut rig = rig_models(models, config());
    let mut deps = (*rig.worker.deps).clone();
    deps.relays = relays.clone();
    rig.worker = crate::worker::Worker::new(deps, config());
    let ctx = agent_run("r-fb-hop");
    *lock(&rig.ledger.ctx) = Some(ctx.clone());

    assert_eq!(rig.worker.execute(ctx).await, Outcome::Succeeded);
    assert_eq!(
        *lock(&relays.opened),
        vec!["assistant", "backup-1", "backup-2"]
    );
    assert_eq!(
        *lock(&relays.revoked),
        vec!["assistant", "backup-1", "backup-2"]
    );
    assert!(
        lock(&rig.runs.state).finished[0]
            .result
            .0
            .contains("answered by third hop")
    );
}
