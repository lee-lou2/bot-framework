//! User-schedule firing tests covering current-revision re-checks, one-shot/cron planning, and fake store harnesses.

use super::{Scheduler, UserTick, user::plan};
use crate::host::{SchedulerConfig, SchedulerDeps};
use bot_core::{
    command::Step,
    cron::Cron as CoreCron,
    id::{AgentId, BindingId, PrincipalId, ProfileId, Revision, RunId, ScheduleId},
    ledger::StartCall,
    port::Error,
    port::Reply,
    profile::Snapshot,
    profile::{Adapter, Agent, Binding, Policy, ProfileCatalog},
    schedule::{AdvanceSchedule, CloseReason, DueKey, DuePos, DueSchedule, NextOccurrence, Timing},
    schedule::{
        DeliveryTarget, DuePage, ScheduleSpec, ScheduleSummary, UnreadableSchedule, UserSchedules,
        Wake,
    },
    state::{Conversation, ConversationKind},
    trace::NoopTracer,
};
use chrono::{TimeZone, Utc};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

pub(super) fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> SystemTime {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().into()
}

fn scopes(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

pub(super) struct NoUsers;

impl UserSchedules for NoUsers {
    fn create(&self, _: StartCall, _: ScheduleSpec) -> Reply<'_, ScheduleId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn list(&self, _: bot_core::id::Attempt, _: Revision) -> Reply<'_, Vec<ScheduleSummary>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn cancel(&self, _: StartCall, _: ScheduleId) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn finish_with_wake(
        &self,
        _: bot_core::id::Attempt,
        _: bot_core::id::CallId,
        _: Revision,
        _: bot_core::command::JsonPayload,
        _: Wake,
    ) -> Reply<'_, ScheduleId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn due(&self, _: Option<DueKey>, _: u32) -> Reply<'_, DuePage> {
        let now = SystemTime::now();
        Box::pin(async move {
            Ok(DuePage {
                now,
                schedules: vec![],
                unreadable: vec![],
                next: None,
            })
        })
    }
    fn advance(&self, _: AdvanceSchedule) -> Reply<'_, Option<RunId>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

#[derive(Default)]
pub(super) struct FixedCatalog(pub Mutex<Option<Arc<Snapshot>>>);

impl ProfileCatalog for FixedCatalog {
    fn active(&self) -> Arc<BTreeMap<ProfileId, Arc<Snapshot>>> {
        let current = self.0.lock().unwrap().clone();
        Arc::new(
            current
                .into_iter()
                .map(|s| (s.id.clone(), s))
                .collect::<BTreeMap<_, _>>(),
        )
    }
    fn lookup(&self, _: &ProfileId, _: &Revision) -> Option<Arc<Snapshot>> {
        None
    }
}

pub(super) struct FakeUsers {
    now: SystemTime,
    pub(super) due: Mutex<Vec<DueSchedule>>,
    pub(super) advanced: Mutex<Vec<AdvanceSchedule>>,
    pub(super) reply: Mutex<Vec<Result<Option<RunId>, Error>>>,
    /// When set, returns `Ok(Some(run))` for `Fire` commands and `Ok(None)` otherwise (bypassing `reply`).
    pub(super) fires: Mutex<Option<RunId>>,
    pub(super) due_error: Mutex<Option<Error>>,
    pub(super) asked: Mutex<Vec<Option<DueKey>>>,
    pub(super) unreadable: Mutex<Vec<UnreadableSchedule>>,
    /// When true, `advance` updates due rows in place like the real store (re-sorting fired rows forward).
    pub(super) moves: Mutex<bool>,
}

impl FakeUsers {
    pub(super) fn new(now: SystemTime, due: Vec<DueSchedule>) -> Arc<Self> {
        Arc::new(Self {
            now,
            due: Mutex::new(due),
            advanced: Mutex::new(Vec::new()),
            reply: Mutex::new(Vec::new()),
            fires: Mutex::new(None),
            due_error: Mutex::new(None),
            asked: Mutex::new(Vec::new()),
            unreadable: Mutex::new(Vec::new()),
            moves: Mutex::new(false),
        })
    }

    fn settle(&self, command: &AdvanceSchedule) {
        let same =
            |profile: &ProfileId, id: &ScheduleId| *profile == command.profile && *id == command.id;
        if matches!(command.next, NextOccurrence::Close(_)) {
            self.unreadable
                .lock()
                .unwrap()
                .retain(|u| !same(&u.profile, &u.id));
        }
        if !*self.moves.lock().unwrap() {
            return;
        }
        let mut due = self.due.lock().unwrap();
        let Some(row) = due.iter().position(|s| same(&s.profile, &s.id)) else {
            return;
        };
        match &command.next {
            NextOccurrence::Fire {
                at,
                next: Some(next),
                ..
            } if *next <= self.now => {
                due[row].cursor = *at;
                due[row].at = *next;
            }
            _ => {
                due.remove(row);
            }
        }
    }
}

impl UserSchedules for FakeUsers {
    fn create(&self, s: StartCall, p: ScheduleSpec) -> Reply<'_, ScheduleId> {
        NoUsers.create(s, p)
    }
    fn list(&self, a: bot_core::id::Attempt, r: Revision) -> Reply<'_, Vec<ScheduleSummary>> {
        NoUsers.list(a, r)
    }
    fn cancel(&self, s: StartCall, i: ScheduleId) -> Reply<'_, ()> {
        NoUsers.cancel(s, i)
    }
    fn finish_with_wake(
        &self,
        a: bot_core::id::Attempt,
        c: bot_core::id::CallId,
        r: Revision,
        j: bot_core::command::JsonPayload,
        w: Wake,
    ) -> Reply<'_, ScheduleId> {
        NoUsers.finish_with_wake(a, c, r, j, w)
    }
    fn due(&self, after: Option<DueKey>, limit: u32) -> Reply<'_, DuePage> {
        self.asked.lock().unwrap().push(after.clone());
        let mut due = self.due.lock().unwrap().clone();
        let pos = |s: &DueSchedule| DuePos {
            at: s.at,
            profile: s.profile.clone(),
            id: s.id.clone(),
        };
        let order = |p: &DuePos| (p.at, p.profile.clone(), p.id.clone());
        due.sort_by_key(|s| order(&pos(s)));
        let end = match &after {
            Some(key) => Some(key.end.clone()),
            None => due.last().map(pos),
        };
        if let Some(end) = &end {
            due.retain(|s| order(&pos(s)) <= order(end));
        }
        if let Some(key) = &after {
            due.retain(|s| order(&pos(s)) > order(&key.after));
        }
        let more = due.len() > limit as usize;
        due.truncate(limit as usize);
        let next = due
            .last()
            .filter(|_| more)
            .zip(end)
            .map(|(last, end)| DueKey {
                after: pos(last),
                end,
            });
        let now = self.now;
        let error = self.due_error.lock().unwrap().as_ref().map(|e| match e {
            Error::Stale => Error::Stale,
            _ => Error::Unavailable,
        });
        let unreadable = if after.is_none() {
            self.unreadable.lock().unwrap().clone()
        } else {
            Vec::new()
        };
        Box::pin(async move {
            if let Some(error) = error {
                return Err(error);
            }
            Ok(DuePage {
                now,
                schedules: due,
                unreadable,
                next,
            })
        })
    }
    fn advance(&self, command: AdvanceSchedule) -> Reply<'_, Option<RunId>> {
        let like_a_store = self
            .fires
            .lock()
            .unwrap()
            .clone()
            .map(|run| Ok(matches!(command.next, NextOccurrence::Fire { .. }).then_some(run)));
        let reply =
            like_a_store.unwrap_or_else(|| self.reply.lock().unwrap().pop().unwrap_or(Ok(None)));
        if reply.is_ok() {
            self.settle(&command);
        }
        self.advanced.lock().unwrap().push(command);
        Box::pin(async move { reply })
    }
}

pub(super) fn snapshot(members: &[&str], with_binding: bool) -> Arc<Snapshot> {
    let main = Agent {
        description: String::new(),
        builtin: BTreeSet::new(),
        parent: None,
        model: "assistant".into(),
        instructions: String::new(),
        env: BTreeMap::new(),
        scopes: scopes(&["run", "calendar.read"]),
        skills: BTreeMap::new(),
        knowledge: vec![],
        memory_seeds: vec![],
        triggers: vec![],
    };
    Arc::new(Snapshot {
        id: ProfileId("personal".into()),
        revision: Revision("r5".into()),
        policy_revision: Revision("p1".into()),
        timezone: "Asia/Seoul".into(),
        model: "assistant".into(),
        root: AgentId("main".into()),
        agents: BTreeMap::from([(AgentId("main".into()), main)]),
        bindings: if with_binding {
            BTreeMap::from([(
                BindingId("desk".into()),
                Binding {
                    agent: AgentId("main".into()),
                    source: Adapter::Cli {
                        account: "owner".into(),
                    },
                },
            )])
        } else {
            BTreeMap::new()
        },
        roles: BTreeMap::from([("owner".into(), scopes(&["run", "calendar.read"]))]),
        members: members
            .iter()
            .map(|m| {
                (
                    PrincipalId((*m).into()),
                    BTreeSet::from(["owner".to_owned()]),
                )
            })
            .collect(),
        super_admins: BTreeSet::new(),
        policy: Policy {
            scopes: scopes(&["run", "calendar.read"]),
            agents: BTreeMap::new(),
            approve: BTreeSet::new(),
        },
        secrets: vec![],
    })
}

pub(super) fn due(timing: Timing, cursor: SystemTime) -> DueSchedule {
    DueSchedule {
        profile: ProfileId("personal".into()),
        id: ScheduleId("s1".into()),
        owner: PrincipalId("local:owner".into()),
        agent: AgentId("main".into()),
        origin: DeliveryTarget {
            binding: BindingId("desk".into()),
            conversation: Conversation {
                kind: ConversationKind::Direct,
                id: "c1".into(),
                thread: None,
            },
        },
        ceiling: scopes(&["run", "calendar.read", "calendar.delete"]),
        spec: ScheduleSpec {
            timing,
            instruction: "Check status".into(),
            quiet: false,
            destination: None,
        },
        cursor,
        at: cursor,
    }
}

pub(super) fn cron(expression: &str, zone: &str) -> Timing {
    Timing::Cron {
        expression: CoreCron::parse(expression).unwrap(),
        timezone: zone.into(),
    }
}

fn fire(next: NextOccurrence) -> (SystemTime, Option<SystemTime>, BTreeSet<String>) {
    match next {
        NextOccurrence::Fire { at, next, scopes } => (at, next, scopes),
        _ => panic!("expected NextOccurrence::Fire"),
    }
}

#[test]
fn a_one_shot_fires_with_the_scopes_it_still_has_and_waits_if_early() {
    let snap = snapshot(&["local:owner"], true);
    let now = utc(2026, 11, 24, 3, 0);
    let at = utc(2026, 11, 24, 2, 59);
    let (revision, next) = plan(
        &due(Timing::At(at), utc(2026, 11, 24, 2, 0)),
        Some(&snap),
        now,
    );
    assert_eq!(revision, Some(Revision("r5".into())));
    let (fired, after, got) = fire(next);
    assert_eq!((fired, after), (at, None), "one-shot has no next firing");
    assert_eq!(
        got,
        scopes(&["run", "calendar.read"]),
        "ceiling intersected with current owner scopes"
    );
    let early = utc(2026, 11, 24, 3, 5);
    let (_, next) = plan(
        &due(Timing::At(early), utc(2026, 11, 24, 2, 0)),
        Some(&snap),
        now,
    );
    assert!(matches!(next, NextOccurrence::WaitUntil(t) if t == early));
}

#[test]
fn a_cron_schedule_waits_for_a_future_first_occurrence_then_fires_oldest_first() {
    let snap = snapshot(&["local:owner"], true);
    let daily = cron("0 9 * * *", "UTC");
    let (_, next) = plan(
        &due(daily.clone(), utc(2026, 11, 24, 3, 0)),
        Some(&snap),
        utc(2026, 11, 24, 3, 0),
    );
    assert!(matches!(next, NextOccurrence::WaitUntil(t) if t == utc(2026, 11, 24, 9, 0)));
    let (_, next) = plan(
        &due(daily, utc(2026, 11, 21, 9, 0)),
        Some(&snap),
        utc(2026, 11, 24, 10, 0),
    );
    let (at, after, _) = fire(next);
    assert_eq!(at, utc(2026, 11, 22, 9, 0));
    assert_eq!(after, Some(utc(2026, 11, 23, 9, 0)));
}

#[test]
fn a_revoked_member_binding_agent_or_run_permission_and_an_unreadable_zone_close_the_schedule() {
    let now = utc(2026, 11, 24, 3, 0);
    let one_shot = || {
        due(
            Timing::At(utc(2026, 11, 24, 2, 59)),
            utc(2026, 11, 24, 2, 0),
        )
    };
    let (revision, next) = plan(&one_shot(), None, now);
    assert_eq!(revision, None);
    assert!(matches!(
        next,
        NextOccurrence::Close(CloseReason::ProfileInactive)
    ));
    let closed = |snap: &Snapshot, schedule: DueSchedule| {
        let (revision, next) = plan(&schedule, Some(snap), now);
        assert_eq!(
            revision,
            Some(Revision("r5".into())),
            "close decision must carry the checked revision"
        );
        matches!(next, NextOccurrence::Close(_))
    };
    assert!(closed(&snapshot(&[], true), one_shot()), "member removed");
    assert!(
        closed(&snapshot(&["local:owner"], false), one_shot()),
        "binding removed"
    );
    let mut other = one_shot();
    other.agent = AgentId("gone".into());
    assert!(
        closed(&snapshot(&["local:owner"], true), other),
        "agent removed"
    );
    let mut no_run = one_shot();
    no_run.ceiling = scopes(&["calendar.read"]);
    assert!(
        closed(&snapshot(&["local:owner"], true), no_run),
        "ceiling lacks run scope"
    );
    let bad_zone = due(cron("0 9 * * *", "Mars/Olympus"), utc(2026, 11, 24, 0, 0));
    assert!(
        closed(&snapshot(&["local:owner"], true), bad_zone),
        "unparsable timezone"
    );
}

pub(super) fn scheduler(
    users: &Arc<FakeUsers>,
    snap: Option<Arc<Snapshot>>,
    batch: usize,
) -> Scheduler {
    let catalog = Arc::new(FixedCatalog::default());
    *catalog.0.lock().unwrap() = snap;
    Scheduler::new(
        SchedulerDeps {
            schedule: Arc::new(super::tests::EmptySchedule),
            user: users.clone(),
            catalog,
            tracer: Arc::new(NoopTracer),
        },
        SchedulerConfig {
            interval: Duration::from_millis(5),
            batch,
        },
    )
}

#[tokio::test]
async fn each_pass_advances_every_due_schedule_with_the_cursor_it_read_and_counts_outcomes() {
    let now = utc(2026, 11, 24, 10, 0);
    let one = due(Timing::At(utc(2026, 11, 24, 9, 0)), utc(2026, 11, 24, 8, 0));
    let mut two = due(cron("0 18 * * *", "UTC"), utc(2026, 11, 24, 8, 0));
    two.id = ScheduleId("s2".into());
    let mut three = due(Timing::At(utc(2026, 11, 24, 9, 0)), utc(2026, 11, 24, 8, 0));
    three.id = ScheduleId("s3".into());
    three.owner = PrincipalId("local:gone".into());
    let users = FakeUsers::new(now, vec![one.clone(), two, three]);
    users
        .reply
        .lock()
        .unwrap()
        .extend([Ok(None), Ok(None), Ok(Some(RunId("run-1".into())))]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    let tick = s.tick_users().await.unwrap();
    assert_eq!(
        tick,
        UserTick {
            fired: 1,
            waiting: 1,
            closed: 1,
            backlog: false
        }
    );
    {
        let advanced = users.advanced.lock().unwrap();
        assert_eq!(advanced.len(), 3);
        assert!(
            advanced
                .iter()
                .all(|a| a.expected_cursor == utc(2026, 11, 24, 8, 0))
        );
        assert_eq!(advanced[0].checked_revision, Some(Revision("r5".into())));
    }
    let mut third = one.clone();
    third.id = ScheduleId("s9".into());
    let users = FakeUsers::new(now, vec![one.clone(), one, third]);
    users.reply.lock().unwrap().extend([
        Ok(Some(RunId("run-2".into()))),
        Ok(Some(RunId("run-3".into()))),
    ]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 2);
    assert!(s.tick_users().await.unwrap().backlog);
}

#[tokio::test]
async fn a_stale_or_refused_advance_does_not_stop_the_other_schedules_but_unavailable_does() {
    let now = utc(2026, 11, 24, 10, 0);
    let mut a = due(Timing::At(utc(2026, 11, 24, 9, 0)), utc(2026, 11, 24, 8, 0));
    a.id = ScheduleId("a".into());
    let mut b = a.clone();
    b.id = ScheduleId("b".into());
    let mut c = a.clone();
    c.id = ScheduleId("c".into());
    let users = FakeUsers::new(now, vec![a, b, c]);
    users.reply.lock().unwrap().extend([
        Ok(Some(RunId("run-c".into()))),
        Err(Error::Denied),
        Err(Error::Stale),
    ]);
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    let tick = s.tick_users().await.unwrap();
    assert_eq!(tick.fired, 1, "first two skipped, third admitted");
    assert_eq!(users.advanced.lock().unwrap().len(), 3);
    let users = FakeUsers::new(
        now,
        vec![due(
            Timing::At(utc(2026, 11, 24, 9, 0)),
            utc(2026, 11, 24, 8, 0),
        )],
    );
    users.reply.lock().unwrap().push(Err(Error::Unavailable));
    let s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    assert!(matches!(s.tick_users().await, Err(Error::Unavailable)));
    let _ = Step {
        key: String::new(),
        body: bot_core::command::JsonPayload(String::new()),
    };
}
