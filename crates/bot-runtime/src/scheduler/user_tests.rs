//! 사용자 예약 발화 시험(가짜 저장소): 현재 판본으로 다시 보는 멤버·바인딩·에이전트·권한, at·cron의 다음 일, 놓친 발생의 순서, 수렴.

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

/// 저장소가 없는 구성(정적 시험이 사용자 예약을 보지 않는다).
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

/// 프로필 판본을 주는 카탈로그(없으면 비활성).
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

/// 확인할 예약을 주고 받은 `advance`를 기록하는 가짜 사용자 예약 저장소.
pub(super) struct FakeUsers {
    now: SystemTime,
    pub(super) due: Mutex<Vec<DueSchedule>>,
    pub(super) advanced: Mutex<Vec<AdvanceSchedule>>,
    pub(super) reply: Mutex<Vec<Result<Option<RunId>, Error>>>,
    /// 있으면 실제 저장소처럼 답한다: 발화 명령에만 이 run을 주고 닫기·기다림에는 None을 준다(`reply`는 쓰지 않는다)
    pub(super) fires: Mutex<Option<RunId>>,
    /// 있으면 `due`가 이 오류로 답한다(옛 집합에 묶인 핸들)
    pub(super) due_error: Mutex<Option<Error>>,
    /// `due`가 받은 `after`(이어 읽을 자리)를 부른 순서대로
    pub(super) asked: Mutex<Vec<Option<DueKey>>>,
    /// 저장된 cron을 읽지 못한 행: 바퀴의 첫 쪽에 실려 오고, 닫기에 성공하면 사라진다
    pub(super) unreadable: Mutex<Vec<UnreadableSchedule>>,
    /// 켜면 행이 실제 저장소처럼 움직인다: 발화하면 커서가 그 발생으로 앞서고 다음 확인 시각(`at` = 이 가짜의 정렬 키)이 다음 발생으로 가며, 다음 확인 시각이 지금보다 뒤이거나 마지막 발생이거나 닫히거나 기다리게 되면 더는 확인 대상이 아니다
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

    /// 저장소가 성공으로 받은 명령이 행에 미치는 영향. 닫은 손상 행은 사라지고, `moves`가 켜져 있으면 확인 대상 행이 움직인다.
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
        // 실제 저장소와 같다: 바퀴의 끝은 바퀴를 시작할 때 마지막 행으로 고정하고, 한 행을 더 읽어 남았는지 본다
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
            instruction: "확인".into(),
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
        _ => panic!("발화여야 한다"),
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
    assert_eq!((fired, after), (at, None), "한 번짜리는 마지막 발생");
    assert_eq!(
        got,
        scopes(&["run", "calendar.read"]),
        "만든 때의 상한(calendar.delete 포함) ∩ 지금 소유자 권한"
    );
    // 호스트 시계가 앞서 가면 예정 시각까지 기다린다
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
    // 첫 발생이 미래: 커서를 넘기지 않고 그 시각까지 기다린다
    let (_, next) = plan(
        &due(daily.clone(), utc(2026, 11, 24, 3, 0)),
        Some(&snap),
        utc(2026, 11, 24, 3, 0),
    );
    assert!(matches!(next, NextOccurrence::WaitUntil(t) if t == utc(2026, 11, 24, 9, 0)));
    // 사흘 밀렸다: 가장 오래된 발생부터 하나씩, 다음 확인은 그다음 발생
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
    // 프로필이 비활성이면 판본 없이 닫는다
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
            "닫는 판정도 확인한 판본으로 한다"
        );
        matches!(next, NextOccurrence::Close(_))
    };
    assert!(closed(&snapshot(&[], true), one_shot()), "멤버 탈퇴");
    assert!(
        closed(&snapshot(&["local:owner"], false), one_shot()),
        "바인딩 삭제"
    );
    let mut other = one_shot();
    other.agent = AgentId("gone".into());
    assert!(
        closed(&snapshot(&["local:owner"], true), other),
        "에이전트 삭제"
    );
    let mut no_run = one_shot();
    no_run.ceiling = scopes(&["calendar.read"]);
    assert!(
        closed(&snapshot(&["local:owner"], true), no_run),
        "run 없는 상한"
    );
    let bad_zone = due(cron("0 9 * * *", "Mars/Olympus"), utc(2026, 11, 24, 0, 0));
    assert!(
        closed(&snapshot(&["local:owner"], true), bad_zone),
        "해석할 수 없는 시간대"
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
    users.reply.lock().unwrap().extend([
        Ok(None),                        // s3: 닫음
        Ok(None),                        // s2: 기다림
        Ok(Some(RunId("run-1".into()))), // s1: 접수
    ]);
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
    // 한 번에 다 못 보면(배치보다 많다) 곧바로 다시 점검한다
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
    assert_eq!(tick.fired, 1, "앞의 둘은 건너뛰고 셋째는 접수한다");
    assert_eq!(users.advanced.lock().unwrap().len(), 3);
    // 저장소가 응답하지 못하면 오류(다음 점검에서 이어 간다)
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
