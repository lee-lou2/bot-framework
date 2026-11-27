//! Route re-verification tests for explicit-destination user schedules before firing.

use super::user_tests::{FakeUsers, due, scheduler, snapshot, utc};
use crate::scheduler::UserTick;
use bot_core::{
    id::{BindingId, ProfileId, RunId, ScheduleId},
    port::Error,
    schedule::{
        AdvanceSchedule, CloseReason, DeliveryTarget, DueSchedule, NextOccurrence, ScheduleRoutes,
        Timing,
    },
    state::{Conversation, ConversationKind},
};
use std::sync::{Arc, Mutex};

fn target(id: &str) -> DeliveryTarget {
    DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind: ConversationKind::Channel,
            id: id.into(),
            thread: Some("t1".into()),
        },
    }
}

struct Routes {
    answer: Mutex<Result<DeliveryTarget, Error>>,
    asked: Mutex<Vec<(String, String)>>,
}

impl Routes {
    fn new(answer: Result<DeliveryTarget, Error>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl ScheduleRoutes for Routes {
    fn resolve(
        &self,
        profile: &ProfileId,
        target: &DeliveryTarget,
    ) -> Result<DeliveryTarget, Error> {
        self.asked
            .lock()
            .unwrap()
            .push((profile.0.clone(), target.conversation.id.clone()));
        match &*self.answer.lock().unwrap() {
            Ok(found) => Ok(found.clone()),
            Err(Error::Denied) => Err(Error::Denied),
            Err(Error::Invalid) => Err(Error::Invalid),
            Err(_) => Err(Error::Unavailable),
        }
    }
}

fn destined(id: &str) -> DueSchedule {
    let mut schedule = due(Timing::At(utc(2026, 11, 24, 9, 0)), utc(2026, 11, 24, 8, 0));
    schedule.spec.destination = Some(target(id));
    schedule
}

async fn pass(
    schedule: DueSchedule,
    routes: Option<Arc<Routes>>,
) -> (UserTick, Vec<AdvanceSchedule>) {
    let users = FakeUsers::new(utc(2026, 11, 24, 10, 0), vec![schedule]);
    *users.fires.lock().unwrap() = Some(RunId("run-1".into()));
    let mut s = scheduler(&users, Some(snapshot(&["local:owner"], true)), 10);
    if let Some(routes) = routes {
        s = s.with_routes(routes);
    }
    let tick = s.tick_users().await.unwrap();
    let advanced = std::mem::take(&mut *users.advanced.lock().unwrap());
    (tick, advanced)
}

#[tokio::test]
async fn a_destination_that_is_still_sealed_the_same_fires() {
    let routes = Routes::new(Ok(target("T1/C9")));
    let (tick, advanced) = pass(destined("T1/C9"), Some(routes.clone())).await;
    assert_eq!(tick.fired, 1);
    assert!(matches!(
        advanced[..],
        [AdvanceSchedule {
            next: NextOccurrence::Fire { .. },
            ..
        }]
    ));
    assert_eq!(
        *routes.asked.lock().unwrap(),
        [("personal".to_owned(), "T1/C9".to_owned())],
        "resolves once before firing"
    );
}

#[tokio::test]
async fn a_destination_that_is_gone_or_points_elsewhere_closes_the_schedule_without_firing() {
    let mut rebound = target("T1/C9");
    rebound.binding = BindingId("other".into());
    let mut thread = target("T1/C9");
    thread.conversation.thread = None;
    for (answer, why) in [
        (Err(Error::Denied), "removed"),
        (Err(Error::Invalid), "invalid"),
        (Ok(target("T2/C9")), "different team"),
        (Ok(target("T1/C1")), "different channel"),
        (Ok(rebound), "different binding"),
        (Ok(thread), "different thread"),
    ] {
        let (tick, advanced) = pass(destined("T1/C9"), Some(Routes::new(answer))).await;
        assert_eq!((tick.fired, tick.closed), (0, 1), "{why}");
        assert!(
            matches!(
                advanced[..],
                [AdvanceSchedule {
                    next: NextOccurrence::Close(CloseReason::Unrouted),
                    ..
                }]
            ),
            "{why}: should write a single Unrouted close"
        );
    }
}

#[tokio::test]
async fn an_unconfirmed_destination_neither_fires_nor_closes() {
    for routes in [Some(Routes::new(Err(Error::Unavailable))), None] {
        let (tick, advanced) = pass(destined("T1/C9"), routes).await;
        assert_eq!((tick.fired, tick.closed, tick.waiting), (0, 0, 0));
        assert!(advanced.is_empty(), "advance must not be called");
    }
}

#[tokio::test]
async fn schedules_for_the_original_conversation_never_ask_the_registry() {
    let routes = Routes::new(Err(Error::Denied));
    let plain = due(Timing::At(utc(2026, 11, 24, 9, 0)), utc(2026, 11, 24, 8, 0));
    let (tick, _) = pass(plain, Some(routes.clone())).await;
    assert_eq!(tick.fired, 1);
    assert!(routes.asked.lock().unwrap().is_empty());
    let mut waiting = destined("T1/C9");
    waiting.spec.timing = Timing::At(utc(2026, 11, 24, 12, 0));
    let (tick, _) = pass(waiting, Some(routes.clone())).await;
    assert_eq!(tick.waiting, 1);
    assert!(routes.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_held_destination_is_asked_once_per_pass_even_when_a_storage_error_makes_the_page_repeat()
{
    let mut held = destined("T1/C9");
    held.id = ScheduleId("a-held".into());
    let mut plain = due(Timing::At(utc(2026, 11, 24, 9, 0)), utc(2026, 11, 24, 8, 1));
    plain.id = ScheduleId("b-plain".into());
    let users = FakeUsers::new(utc(2026, 11, 24, 10, 0), vec![held, plain]);
    users.reply.lock().unwrap().push(Err(Error::Unavailable));
    let routes = Routes::new(Err(Error::Unavailable));
    let s =
        scheduler(&users, Some(snapshot(&["local:owner"], true)), 10).with_routes(routes.clone());
    assert!(matches!(s.tick_users().await, Err(Error::Unavailable)));
    s.tick_users().await.unwrap();
    assert_eq!(
        routes.asked.lock().unwrap().len(),
        1,
        "held schedule should only be probed once in the same pass"
    );
}
