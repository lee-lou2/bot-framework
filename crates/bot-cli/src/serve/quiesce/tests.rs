//! 종료 정리 확인: 세션 회수 → 모델 샌드박스 `reap` → 시작해 둔 도구 호출마다 `reap`. 하나라도 확인하지 못하면 오류, 새 호출이 계속 생기면 확인하지 못한 것이다.

use super::*;
use bot_broker::{
    ports::{Ran, ToolJob},
    sys::OsEntropy,
};
use bot_core::{
    id::{ClaimEpoch, Generation, ProfileId, RunId},
    sandbox::{ModelProcess, ModelSpec, Sandbox},
};
use std::{collections::VecDeque, sync::Mutex};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn attempt() -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        claim: Some(ClaimEpoch(3)),
        generation: Generation(2),
    }
}

type Log = Arc<Mutex<Vec<String>>>;

/// 훑을 때마다 정해 둔 소유 표지 목록을 낸다(다 쓰면 빈 목록).
struct Passes(Mutex<VecDeque<Vec<String>>>);

impl StartedCalls for Passes {
    fn owners(&self, _: Attempt) -> Reply<'_, Vec<String>> {
        let next = lock(&self.0).pop_front().unwrap_or_default();
        Box::pin(async move { Ok(next) })
    }
}

fn passes(lists: &[&[&str]]) -> Arc<Passes> {
    Arc::new(Passes(Mutex::new(
        lists
            .iter()
            .map(|owners| owners.iter().map(|o| (*o).to_owned()).collect())
            .collect(),
    )))
}

/// `reap`한 소유 표지를 적고, `refuse`에 든 것은 확인하지 못한다.
struct Tools {
    log: Log,
    refuse: Vec<&'static str>,
}

impl ToolRunner for Tools {
    fn installed(&self, _: &str) -> Option<String> {
        None
    }
    fn run(&self, _: ToolJob) -> Reply<'_, Ran> {
        Box::pin(std::future::ready(Ok(Ran::NotSent)))
    }
    fn reap(&self, owner: String) -> Reply<'_, ()> {
        lock(&self.log).push(format!("tool:{owner}"));
        let refused = self.refuse.contains(&owner.as_str());
        Box::pin(async move {
            if refused {
                Err(Error::Unavailable)
            } else {
                Ok(())
            }
        })
    }
}

struct Models {
    log: Log,
    refuse: bool,
}

impl Sandbox<ModelSpec> for Models {
    type Process = Box<dyn ModelProcess>;

    fn spawn(&self, _: ModelSpec) -> Reply<'_, Self::Process> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        lock(&self.log).push(format!("model:{owner}"));
        let refused = self.refuse;
        Box::pin(async move {
            if refused {
                Err(Error::Unavailable)
            } else {
                Ok(())
            }
        })
    }
}

fn quiescer(
    started: Arc<Passes>,
    tool_refuse: Vec<&'static str>,
    model_refuse: Option<bool>,
) -> (Quiescer, Log) {
    let log = Log::default();
    let model = model_refuse.map(|refuse| {
        (
            SessionTable::new(Arc::new(OsEntropy)),
            Arc::new(Models {
                log: log.clone(),
                refuse,
            }) as Arc<ModelSandbox>,
        )
    });
    let tools = Arc::new(Tools {
        log: log.clone(),
        refuse: tool_refuse,
    });
    (Quiescer::new(started, tools, model), log)
}

#[tokio::test]
async fn the_model_vm_is_reaped_first_and_then_every_started_tool_call() {
    let (q, log) = quiescer(passes(&[&["r1.2.c1", "r1.2.c2"]]), vec![], Some(false));
    q.quiesce(attempt()).await.unwrap();
    assert_eq!(
        *lock(&log),
        [
            "model:personal.r1.g2.c3".to_owned(),
            "tool:r1.2.c1".to_owned(),
            "tool:r1.2.c2".to_owned()
        ],
        "모델 VM(소유 표지는 어댑터의 것) → 도구 호출 순서"
    );
}

#[tokio::test]
async fn a_host_without_a_model_cli_only_reaps_tools() {
    let (q, log) = quiescer(passes(&[&["r1.2.c1"]]), vec![], None);
    q.quiesce(attempt()).await.unwrap();
    assert_eq!(*lock(&log), ["tool:r1.2.c1".to_owned()]);
}

#[tokio::test]
async fn nothing_started_means_nothing_to_reap() {
    let (q, log) = quiescer(passes(&[]), vec![], None);
    q.quiesce(attempt()).await.unwrap();
    assert!(lock(&log).is_empty());
}

#[tokio::test]
async fn an_unconfirmed_model_vm_stops_before_any_tool_is_touched() {
    let (q, log) = quiescer(passes(&[&["r1.2.c1"]]), vec![], Some(true));
    assert!(matches!(
        q.quiesce(attempt()).await,
        Err(Error::Unavailable)
    ));
    assert_eq!(*lock(&log), ["model:personal.r1.g2.c3".to_owned()]);
}

#[tokio::test]
async fn an_unconfirmed_tool_process_is_an_error() {
    let (q, log) = quiescer(passes(&[&["r1.2.c1", "r1.2.c2"]]), vec!["r1.2.c1"], None);
    assert!(matches!(
        q.quiesce(attempt()).await,
        Err(Error::Unavailable)
    ));
    assert_eq!(
        *lock(&log),
        ["tool:r1.2.c1".to_owned()],
        "확인하지 못한 곳에서 멈춘다"
    );
}

#[tokio::test]
async fn a_call_that_started_after_the_first_look_is_reaped_on_the_next_pass() {
    // 첫 훑기에서 c1만 보였고, 세션을 거둔 뒤에도 이미 들어와 있던 요청이 c2를 시작했다
    let (q, log) = quiescer(
        passes(&[
            &["r1.2.c1"],
            &["r1.2.c1", "r1.2.c2"],
            &["r1.2.c1", "r1.2.c2"],
        ]),
        vec![],
        None,
    );
    q.quiesce(attempt()).await.unwrap();
    assert_eq!(
        *lock(&log),
        ["tool:r1.2.c1".to_owned(), "tool:r1.2.c2".to_owned()],
        "이미 끝낸 표지는 다시 reap하지 않는다"
    );
}

#[tokio::test]
async fn calls_that_keep_appearing_are_never_confirmed() {
    let (q, _) = quiescer(
        passes(&[&["a"], &["a", "b"], &["a", "b", "c"], &["a", "b", "c", "d"]]),
        vec![],
        None,
    );
    assert!(
        matches!(q.quiesce(attempt()).await, Err(Error::Unavailable)),
        "세 번 훑고도 새 호출이 생기면 확인하지 못한 것이다"
    );
}
