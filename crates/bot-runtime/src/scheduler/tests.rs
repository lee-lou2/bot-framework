//! Static cron scheduler tests: catch-up ordering, beats, segments, DST, error handling, convergence, and ScheduleSet building.

use super::*;
use crate::host::SchedulerDeps;
use bot_core::{
    command::{ScheduleSnapshot, Step},
    id::{AgentId, PrincipalId, ProfileId, Revision, RunId},
    port::{Reply, ScheduleStore},
    profile::{Agent, Policy, Trigger},
    trace::NoopTracer,
};
use chrono::{TimeZone, Utc};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Mutex,
};

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> SystemTime {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().into()
}

fn ms(t: SystemTime) -> i64 {
    unix_ms(t)
}

fn scopes(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn spec(cron: &str, zone: &str, beat: bool) -> TriggerSpec {
    TriggerSpec {
        approver: None,
        trigger: "morning".into(),
        cron: cron.into(),
        timezone: zone.into(),
        beat,
        principal: PrincipalId("service:morning".into()),
        scopes: scopes(&["calendar.read"]),
        target: Target::Tool {
            agent: AgentId("main".into()),
            tool: bot_core::id::ToolId("agenda.list".into()),
            args: JsonPayload("{}".into()),
        },
    }
}

fn segment(
    revision: &str,
    from: SystemTime,
    until: Option<SystemTime>,
    spec: TriggerSpec,
) -> Segment {
    Segment {
        revision: Revision(revision.into()),
        from,
        until,
        spec,
    }
}

fn trigger(cursor: SystemTime, segments: Vec<Segment>) -> ScheduledTrigger {
    ScheduledTrigger {
        profile: ProfileId("personal".into()),
        agent: AgentId("main".into()),
        trigger: "morning".into(),
        cursor,
        segments,
    }
}

#[derive(Default)]
struct Store {
    now: Option<SystemTime>,
    triggers: Vec<ScheduledTrigger>,
    accepted: Vec<(Occurrence, Accept)>,
    errors: VecDeque<Error>,
}

struct FakeSchedule(Mutex<Store>);

fn lock(m: &Mutex<Store>) -> std::sync::MutexGuard<'_, Store> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FakeSchedule {
    fn new(now: SystemTime, triggers: Vec<ScheduledTrigger>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Store {
            now: Some(now),
            triggers,
            ..Store::default()
        })))
    }
    fn accepted(&self) -> Vec<(Occurrence, Accept)> {
        lock(&self.0).accepted.clone()
    }
    fn times(&self) -> Vec<i64> {
        self.accepted()
            .iter()
            .map(|(o, _)| ms(o.scheduled_at))
            .collect()
    }
}

impl ScheduleStore for FakeSchedule {
    fn accept_occurrence(&self, occurrence: Occurrence, command: Accept) -> Reply<'_, RunId> {
        let mut store = lock(&self.0);
        let result = (|| {
            if let Some(error) = store.errors.pop_front() {
                return Err(error);
            }
            let at = occurrence.scheduled_at;
            let position = store
                .accepted
                .iter()
                .position(|(o, _)| o.trigger == occurrence.trigger && o.scheduled_at == at);
            if let Some(n) = position {
                return Ok(RunId(format!("run-{n}")));
            }
            let cursor = store
                .triggers
                .iter_mut()
                .find(|t| t.trigger == occurrence.trigger);
            let Some(cursor) = cursor else {
                return Err(Error::Missing);
            };
            if at <= cursor.cursor {
                return Err(Error::Stale);
            }
            cursor.cursor = at;
            store.accepted.push((occurrence, command));
            Ok(RunId(format!("run-{}", store.accepted.len() - 1)))
        })();
        Box::pin(async move { result })
    }
    fn snapshot(&self) -> Reply<'_, ScheduleSnapshot> {
        let store = lock(&self.0);
        let snapshot = ScheduleSnapshot {
            now: store.now.unwrap(),
            triggers: store.triggers.clone(),
        };
        Box::pin(async move { Ok(snapshot) })
    }
}

pub(super) struct EmptySchedule;

impl ScheduleStore for EmptySchedule {
    fn accept_occurrence(&self, _: Occurrence, _: Accept) -> Reply<'_, RunId> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn snapshot(&self) -> Reply<'_, ScheduleSnapshot> {
        let now = SystemTime::now();
        Box::pin(async move {
            Ok(ScheduleSnapshot {
                now,
                triggers: vec![],
            })
        })
    }
}

fn scheduler(store: &Arc<FakeSchedule>, batch: usize) -> Scheduler {
    Scheduler::new(
        SchedulerDeps {
            schedule: store.clone(),
            user: Arc::new(super::user_tests::NoUsers),
            catalog: Arc::new(super::user_tests::FixedCatalog::default()),
            tracer: Arc::new(NoopTracer),
        },
        SchedulerConfig {
            interval: Duration::from_millis(5),
            batch,
        },
    )
}

#[tokio::test]
async fn missed_occurrences_are_accepted_oldest_first_within_the_batch_until_caught_up() {
    let from = utc(2026, 11, 24, 0, 0);
    let store = FakeSchedule::new(
        utc(2026, 11, 24, 0, 55),
        vec![trigger(
            from,
            vec![segment(
                "r1",
                from,
                None,
                spec("*/10 * * * *", "UTC", false),
            )],
        )],
    );
    let s = scheduler(&store, 3);
    let first = s.tick().await.unwrap();
    assert_eq!((first.accepted, first.backlog), (3, true));
    assert_eq!(
        store.times(),
        [10, 20, 30].map(|m| ms(utc(2026, 11, 24, 0, m)))
    );
    let second = s.tick().await.unwrap();
    assert_eq!((second.accepted, second.backlog), (2, false));
    assert_eq!(store.times().len(), 5);
    assert_eq!(
        s.tick().await.unwrap(),
        Tick::default(),
        "no work once caught up"
    );
    assert_eq!(store.times().last(), Some(&ms(utc(2026, 11, 24, 0, 50))));
}

#[tokio::test]
async fn a_beat_catches_up_with_only_the_latest_missed_occurrence() {
    let from = utc(2026, 11, 24, 0, 0);
    let store = FakeSchedule::new(
        utc(2026, 11, 24, 0, 55),
        vec![trigger(
            from,
            vec![segment("r1", from, None, spec("*/10 * * * *", "UTC", true))],
        )],
    );
    let tick = scheduler(&store, 100).tick().await.unwrap();
    assert_eq!((tick.accepted, tick.backlog), (1, false));
    assert_eq!(store.times(), [ms(utc(2026, 11, 24, 0, 50))]);
}

#[tokio::test]
async fn a_beat_after_a_very_long_outage_still_takes_the_true_latest_occurrence() {
    // 150 days of per-minute occurrences (>210k) must still select the latest instant.
    let from = utc(2026, 1, 1, 0, 0);
    let now = utc(2026, 5, 31, 12, 34);
    let store = FakeSchedule::new(
        now,
        vec![trigger(
            from,
            vec![segment("r1", from, None, spec("* * * * *", "UTC", true))],
        )],
    );
    let tick = scheduler(&store, 100).tick().await.unwrap();
    assert_eq!(tick.accepted, 1);
    assert_eq!(store.times(), [ms(utc(2026, 5, 31, 12, 34))]);
}

#[tokio::test]
async fn each_segment_uses_its_own_definition_and_the_boundary_belongs_to_the_earlier_segment() {
    let from = utc(2026, 11, 24, 0, 0);
    let change = utc(2026, 11, 24, 0, 30);
    let store = FakeSchedule::new(
        utc(2026, 11, 24, 1, 0),
        vec![trigger(
            from,
            vec![
                segment("r1", from, Some(change), spec("*/10 * * * *", "UTC", false)),
                segment("r2", change, None, spec("*/20 * * * *", "UTC", false)),
            ],
        )],
    );
    scheduler(&store, 100).tick().await.unwrap();
    let got: Vec<_> = store
        .accepted()
        .iter()
        .map(|(o, a)| (ms(o.scheduled_at), a.revision.0.clone()))
        .collect();
    let m = |min: u32| ms(utc(2026, 11, 24, 0, min));
    assert_eq!(
        got,
        [
            (m(10), "r1".into()),
            (m(20), "r1".into()),
            (m(30), "r1".into()),
            (m(40), "r2".into()),
            (ms(utc(2026, 11, 24, 1, 0)), "r2".into()),
        ]
    );
}

#[tokio::test]
async fn the_command_follows_the_stored_definition() {
    let from = utc(2026, 11, 24, 8, 59);
    let mut definition = spec("0 9 * * *", "Asia/Seoul", false);
    definition.approver = Some(PrincipalId("local:owner".into()));
    definition.target = Target::Agent {
        agent: AgentId("main".into()),
        instruction: "Show today's agenda".into(),
    };
    let store = FakeSchedule::new(
        utc(2026, 11, 25, 1, 0), // 10:00 KST
        vec![trigger(
            from,
            vec![segment("r7", from, None, definition.clone())],
        )],
    );
    scheduler(&store, 10).tick().await.unwrap();
    let (occurrence, command) = store.accepted().remove(0);
    let at = ms(utc(2026, 11, 25, 0, 0)); // 09:00 KST
    assert_eq!(ms(occurrence.scheduled_at), at);
    assert_eq!(command.event.request.0, format!("cron:morning:{at}"));
    assert_eq!(command.event.binding.0, "@cron");
    assert_eq!(command.event.principal, definition.principal);
    assert_eq!(command.event.conversation.kind, ConversationKind::Scheduled);
    assert_eq!(command.event.conversation.id, "morning");
    assert_eq!(command.event.body.0, "{}");
    assert_eq!(command.revision.0, "r7");
    assert_eq!(command.approver, definition.approver);
    assert_eq!(command.scopes, definition.scopes);
    assert_eq!(command.target, definition.target);
}

#[tokio::test]
async fn a_dst_gap_is_skipped_and_a_fold_runs_twice() {
    let spring = utc(2026, 3, 8, 0, 0);
    let store = FakeSchedule::new(
        utc(2026, 3, 8, 12, 0),
        vec![trigger(
            spring,
            vec![segment(
                "r1",
                spring,
                None,
                spec("30 2 * * *", "America/New_York", false),
            )],
        )],
    );
    scheduler(&store, 10).tick().await.unwrap();
    assert!(store.times().is_empty(), "nonexistent time must be skipped");

    let fall = utc(2026, 11, 1, 0, 0);
    let store = FakeSchedule::new(
        utc(2026, 11, 1, 12, 0),
        vec![trigger(
            fall,
            vec![segment(
                "r1",
                fall,
                None,
                spec("30 1 * * *", "America/New_York", false),
            )],
        )],
    );
    scheduler(&store, 10).tick().await.unwrap();
    assert_eq!(
        store.times(),
        [ms(utc(2026, 11, 1, 5, 30)), ms(utc(2026, 11, 1, 6, 30))],
        "ambiguous time must emit both UTC occurrences"
    );
}

#[tokio::test]
async fn an_unparsable_segment_is_skipped_and_the_other_triggers_keep_running() {
    let from = utc(2026, 11, 24, 0, 0);
    let mut good = trigger(
        from,
        vec![segment(
            "r1",
            from,
            None,
            spec("*/30 * * * *", "UTC", false),
        )],
    );
    good.trigger = "good".into();
    let mut bad_cron = trigger(
        from,
        vec![segment("r1", from, None, spec("not a cron", "UTC", false))],
    );
    bad_cron.trigger = "bad-cron".into();
    let mut bad_zone = trigger(
        from,
        vec![segment(
            "r1",
            from,
            None,
            spec("* * * * *", "Mars/Base", false),
        )],
    );
    bad_zone.trigger = "bad-zone".into();
    let store = FakeSchedule::new(utc(2026, 11, 24, 1, 0), vec![bad_cron, bad_zone, good]);
    let tick = scheduler(&store, 10).tick().await.unwrap();
    assert_eq!((tick.accepted, tick.skipped), (2, 2));
    assert!(store.accepted().iter().all(|(o, _)| o.trigger == "good"));
}

#[tokio::test]
async fn store_answers_decide_whether_to_continue_stop_this_trigger_or_retry_later() {
    let from = utc(2026, 11, 24, 0, 0);
    let mut a = trigger(
        from,
        vec![segment(
            "r1",
            from,
            None,
            spec("*/10 * * * *", "UTC", false),
        )],
    );
    a.trigger = "a".into();
    let mut b = a.clone();
    b.trigger = "b".into();
    let store = FakeSchedule::new(utc(2026, 11, 24, 0, 30), vec![a, b]);
    lock(&store.0).errors.push_back(Error::Denied);
    let tick = scheduler(&store, 10).tick().await.unwrap();
    assert_eq!(tick.accepted, 3);
    assert!(store.accepted().iter().all(|(o, _)| o.trigger == "b"));
    lock(&store.0).errors.push_back(Error::Unavailable);
    assert!(scheduler(&store, 10).tick().await.is_err());
    let tick = scheduler(&store, 10).tick().await.unwrap();
    assert_eq!(
        tick.accepted, 3,
        "trigger a resumes on the next tick with all three occurrences"
    );
}

#[tokio::test]
async fn two_schedulers_converge_on_one_run_per_occurrence() {
    let from = utc(2026, 11, 24, 0, 0);
    let store = FakeSchedule::new(
        utc(2026, 11, 24, 2, 0),
        vec![trigger(
            from,
            vec![segment(
                "r1",
                from,
                None,
                spec("*/10 * * * *", "UTC", false),
            )],
        )],
    );
    let (one, two) = (scheduler(&store, 100), scheduler(&store, 100));
    let (a, b) = tokio::join!(one.tick(), two.tick());
    assert!(a.is_ok() && b.is_ok());
    let times = store.times();
    assert_eq!(
        times.len(),
        12,
        "12 distinct 10-minute occurrences in 2 hours"
    );
    assert_eq!(times.iter().collect::<BTreeSet<_>>().len(), 12);
}

#[tokio::test]
async fn the_loop_keeps_ticking_until_shutdown() {
    let from = utc(2026, 11, 24, 0, 0);
    let store = FakeSchedule::new(
        utc(2026, 11, 24, 0, 30),
        vec![trigger(
            from,
            vec![segment(
                "r1",
                from,
                None,
                spec("*/10 * * * *", "UTC", false),
            )],
        )],
    );
    let s = scheduler(&store, 2);
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let serving = tokio::spawn(async move { s.run(shutdown).await });
    let started = std::time::Instant::now();
    while store.times().len() < 3 {
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn the_schedule_set_comes_from_the_snapshots_cron_triggers() {
    let mut main = Agent {
        description: String::new(),
        parent: None,
        model: "assistant".into(),
        instructions: String::new(),
        env: BTreeMap::new(),
        scopes: scopes(&["run", "calendar.read", "calendar.delete"]),
        builtin: BTreeSet::new(),
        skills: BTreeMap::new(),
        knowledge: vec![],
        memory_seeds: vec![],
        triggers: vec![],
    };
    main.triggers.push(Trigger {
        id: "morning".into(),
        on: TriggerSource::Cron {
            schedule: "0 9 * * *".into(),
            beat: false,
            principal: PrincipalId("service:morning".into()),
            approver: Some(PrincipalId("local:owner".into())),
        },
        order: 0,
        conditions: BTreeMap::new(),
        input: BTreeMap::new(),
        private: false,
        private_pass: false,
        target: TriggerTarget::Instruction("Show today's agenda".into()),
        preapproved: None,
        human_approval: None,
    });
    main.triggers.push(Trigger {
        id: "heartbeat".into(),
        on: TriggerSource::Cron {
            schedule: "*/5 * * * *".into(),
            beat: true,
            principal: PrincipalId("service:morning".into()),
            approver: None,
        },
        order: 1,
        conditions: BTreeMap::new(),
        input: BTreeMap::new(),
        private: false,
        private_pass: false,
        target: TriggerTarget::Tool {
            tool: bot_core::id::ToolId("agenda.list".into()),
            args: JsonPayload("{}".into()),
        },
        preapproved: None,
        human_approval: None,
    });
    main.triggers.push(Trigger {
        id: "on-message".into(),
        on: TriggerSource::Event {
            binding: bot_core::id::BindingId("desk".into()),
            event: "message".into(),
        },
        order: 2,
        conditions: BTreeMap::new(),
        input: BTreeMap::new(),
        private: false,
        private_pass: false,
        target: TriggerTarget::Instruction("x".into()),
        preapproved: None,
        human_approval: None,
    });
    let snapshot = Snapshot {
        id: ProfileId("personal".into()),
        revision: Revision("r9".into()),
        policy_revision: Revision("p1".into()),
        timezone: "Asia/Seoul".into(),
        model: "assistant".into(),
        root: AgentId("main".into()),
        agents: BTreeMap::from([(AgentId("main".into()), main)]),
        bindings: BTreeMap::new(),
        roles: BTreeMap::from([("reader".into(), scopes(&["calendar.read", "admin.x"]))]),
        members: BTreeMap::from([(
            PrincipalId("service:morning".into()),
            BTreeSet::from(["reader".to_owned()]),
        )]),
        super_admins: BTreeSet::new(),
        policy: Policy {
            scopes: scopes(&["run", "calendar.read", "calendar.delete"]),
            agents: BTreeMap::new(),
            approve: BTreeSet::new(),
        },
        secrets: vec![],
    };
    let set = schedule_set(&snapshot);
    assert_eq!(
        (set.profile.0.as_str(), set.revision.0.as_str()),
        ("personal", "r9")
    );
    assert_eq!(set.triggers.len(), 2, "cron triggers only");
    let morning = set
        .triggers
        .iter()
        .find(|t| t.trigger == "morning")
        .unwrap();
    assert_eq!(
        (
            morning.cron.as_str(),
            morning.timezone.as_str(),
            morning.beat
        ),
        ("0 9 * * *", "Asia/Seoul", false)
    );
    assert_eq!(morning.approver, Some(PrincipalId("local:owner".into())));
    assert_eq!(
        morning.scopes,
        scopes(&["calendar.read"]),
        "effective scopes must intersect role, agent, and profile policy"
    );
    assert!(
        matches!(&morning.target, Target::Agent { instruction, .. } if instruction == "Show today's agenda")
    );
    let beat = set
        .triggers
        .iter()
        .find(|t| t.trigger == "heartbeat")
        .unwrap();
    assert!(
        beat.beat && matches!(&beat.target, Target::Tool { tool, .. } if tool.0 == "agenda.list")
    );
    let _ = Step {
        key: String::new(),
        body: JsonPayload(String::new()),
    };
}
