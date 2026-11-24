//! 진짜 Apple `container` CLI로 확인하는 «VM이 뜨는 중의 취소»: 끊어도 VM이 남지 않고, 종료 반납 신호가 뜨는 중에 와도 유예 안에 run이 대기열로 돌아온다.
//! - 어댑터: 시작 직후(0~55ms)에 `kill`해도 모델 VM이 하나도 남지 않는다. 예전에는 «만들기» 요청이 지우기보다 늦게 도착해 `stopped` VM이 약 7% 남았다.
//! - 워커(진짜 PG·진짜 프런트): 종료 신호가 점검 중이거나 VM이 뜨는 중일 때 워커가 `grace` 0으로 run을 끊고 정리를 확인한 뒤 반납한다(run `ready`·시도 0).
//!   점검 중이면 VM은 아예 뜨지 않는다(`container run` 0회), 어느 쪽이든 끝난 뒤 닻만 남고 종료는 닻도 치운다.
//!
//! `container`·서비스·로컬 `alpine:3.20`이 없으면 건너뛴다. `BOT_CONTAINER_CANARY=required`이면 실패한다.
//! 실행: `BOT_CONTAINER_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test container_cancel -- --nocapture`

mod support;
mod vm;

use bot_broker::{front::SessionTable, testing::FakeUpstream};
use bot_core::{
    port::Reply,
    sandbox::{ModelProcess, ModelSandbox, ModelSpec, Sandbox},
};
use bot_runtime::{Quiesce, model::container::ContainerSandbox};
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use support::{cli::*, *};
use tokio::sync::watch;

/// 모델 VM(닻이 아닌 것) ID들.
fn models(env: &vm::Env, anchor: &str) -> Vec<String> {
    env.ids().into_iter().filter(|id| id != anchor).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_a_booting_vm_leaves_nothing_behind() {
    let Some(program) = vm::available() else {
        return;
    };
    let _serial = vm::serial().await;
    let env = vm::Env::new(program.clone(), "kill");
    let anchored = ContainerSandbox::start(env.config()).await.expect("시작");
    let (relay_port, _rx, _relay) = vm::service(anchored.bind_addr(0));
    let sandbox = anchored.relay(relay_port).await.expect("릴레이");
    let anchor = vm::running_anchor(&program, &env.instance).await;
    for round in 0..60u64 {
        let mut spec = vm::spec(
            &env.root,
            &format!("k{round}"),
            "echo up; sleep 120",
            vec![],
        );
        spec.env.clear();
        spec.stdin = None;
        let mut process = sandbox.spawn(spec).await.expect("시작");
        // 시작 직후(0~55ms)에 끊는 시각을 바꿔 가며: 클라이언트가 «만들기» 요청을 보내는 순간이라 지우기보다 늦게 도착하기 쉽다(예전에는 60번 중 한 번 넘게 남았다)
        tokio::time::sleep(Duration::from_millis((round * 5) % 60)).await;
        process.kill().await;
        drop(process);
        // `kill`이 돌아온 순간 이미 남은 것이 없다(사라진 것을 확인하고 돌아온다)
        let left = models(&env, &anchor);
        assert!(left.is_empty(), "round {round}: {left:?}");
    }
    // 목록이 늦게 보여 주는 VM도 없다
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(models(&env, &anchor).is_empty(), "{:?}", env.ids());
    sandbox.shutdown().await;
    assert!(env.ids().is_empty(), "{:?}", env.ids());
}

/// 시작할 때(`Before`: 점검 전) 또는 프로세스를 받은 직후(`After`: VM이 뜨는 중) 워커에 종료 신호를 보내는 샌드박스.
#[derive(Clone, Copy)]
enum When {
    Before,
    After,
}

struct Signalling {
    inner: Arc<ContainerSandbox>,
    shutdown: watch::Sender<bool>,
    when: When,
    /// 시작이 끝나 VM 프로세스를 만든 횟수(점검 중에 시작을 버렸다면 0이다)
    created: Arc<AtomicUsize>,
}

impl Sandbox<ModelSpec> for Signalling {
    type Process = Box<dyn ModelProcess>;

    fn spawn(&self, spec: ModelSpec) -> Reply<'_, Self::Process> {
        Box::pin(async move {
            if matches!(self.when, When::Before) {
                let _ = self.shutdown.send(true);
            }
            let process = self.inner.spawn(spec).await?;
            self.created.fetch_add(1, Ordering::SeqCst);
            if matches!(self.when, When::After) {
                let _ = self.shutdown.send(true);
            }
            Ok(process)
        })
    }

    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        self.inner.reap(owner)
    }
}

/// 호스트의 종료 정리 확인(`bot-cli`의 `Quiescer`와 같은 순서): 이 시도의 모델 VM을 `reap`으로 끝내고 확인한 뒤 세션 토큰을 거둔다.
struct Reaper {
    sandbox: Arc<ContainerSandbox>,
    sessions: Arc<SessionTable>,
}

impl Quiesce for Reaper {
    fn quiesce(&self, attempt: bot_core::id::Attempt) -> Reply<'_, ()> {
        Box::pin(async move {
            self.sandbox
                .reap(&ContainerSandbox::owner_of(&attempt))
                .await?;
            self.sessions.revoke_attempt(&attempt);
            Ok(())
        })
    }
}

/// 진짜 `container`를 감싼 실행 파일: 모델 VM을 띄우는 `run`을 로그에 적고, 깃발 파일이 있으면 `inspect`를 1초 늦춘다(점검이 느린 기계).
fn wrapper(env: &vm::Env, log: &Path, flag: &Path) -> std::path::PathBuf {
    let path = env.root.join("container-wrapper");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  run) case " $* " in *bot.owner=anchor.*) ;; *) echo run >> "{log}" ;; esac ;;
  network|image) if [ "$2" = inspect ] && [ -e "{flag}" ]; then sleep 1; fi ;;
esac
exec "{real}" "$@"
"#,
        log = log.display(),
        flag = flag.display(),
        real = env.program.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// 시나리오 하나의 결과.
struct Outcome {
    /// 워커가 끝나기까지 걸린 시간
    took: Duration,
    /// 래퍼가 본 모델 VM `run` 횟수
    runs: usize,
    /// 시작이 끝나 VM 프로세스를 만든 횟수
    created: usize,
}

/// 종료 신호 시각을 고른 시나리오 하나.
async fn shutdown_while_starting(when: When, slow_checks: bool, tag: &str) -> Option<Outcome> {
    let w = world().await?;
    let program = vm::available()?;
    let _serial = vm::serial().await;
    let env = vm::Env::new(program.clone(), tag);
    let (log, flag) = (env.root.join("runs.log"), env.root.join("slow-inspect"));
    let mut config = env.config();
    config.program = wrapper(&env, &log, &flag).to_str().unwrap().to_owned();
    let anchored = ContainerSandbox::start(config).await.expect("시작");
    let anchor = vm::running_anchor(&program, &env.instance).await;
    if slow_checks {
        std::fs::write(&flag, "").unwrap();
    }
    let run = w.accept(&format!("req-{tag}"), "일정을 알려줘").await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let holder: Mutex<Option<Arc<ContainerSandbox>>> = Mutex::new(None);
    let signal = shutdown_tx.clone();
    let created = Arc::new(AtomicUsize::new(0));
    let host = host_tuned(
        &w,
        w.pg.peer(&format!("worker-{tag}")).await,
        Arc::new(FakeUpstream::new()),
        anchored.bind_addr(0),
        async |front| {
            let relay = Arc::new(anchored.relay(front.port()).await.expect("릴레이"));
            *holder.lock().unwrap() = Some(relay.clone());
            Arc::new(Signalling {
                inner: relay,
                shutdown: signal,
                when,
                created: created.clone(),
            }) as Arc<ModelSandbox>
        },
        &env.root.join("ws"),
        w.broker.clone(),
        |config| {
            // 유예 없이 곧바로 끊는다. 정리는 10초 안이다
            config.grace = Duration::ZERO;
            config.cleanup = Duration::from_secs(10);
        },
    )
    .await;
    let sandbox = holder.lock().unwrap().take().expect("샌드박스");
    let worker = host.worker.with_quiesce(Arc::new(Reaper {
        sandbox: sandbox.clone(),
        sessions: host.relays.sessions.clone(),
    }));
    let began = Instant::now();
    tokio::time::timeout(Duration::from_secs(60), worker.run(shutdown_rx))
        .await
        .expect("워커가 끝나야 한다");
    let took = began.elapsed();

    // 정리를 확인하고 반납했다: run은 대기열로 돌아왔고(시도를 쓰지 않았다) 이 시도의 VM은 하나도 없다
    w.pg.assert_state(&run, "ready").await;
    assert_eq!(
        w.pg.scalar::<i32>(&format!(
            "select attempts from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        0
    );
    assert!(models(&env, &anchor).is_empty(), "{:?}", env.ids());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(models(&env, &anchor).is_empty(), "{:?}", env.ids());
    let runs = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .count();
    sandbox.shutdown().await;
    assert!(env.ids().is_empty(), "닻까지 치운다: {:?}", env.ids());
    Some(Outcome {
        took,
        runs,
        created: created.load(Ordering::SeqCst),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_during_a_slow_readiness_check_starts_no_vm_and_releases_the_run() {
    let Some(outcome) = shutdown_while_starting(When::Before, true, "shutready").await else {
        return;
    };
    // 점검이 1초 넘게 걸리는 동안 종료 신호가 왔다: 시작을 버려 VM 프로세스를 만들지도 않았다(예전에는 시작이 끝나기를 기다렸다가 띄운 뒤 끊었다)
    assert_eq!(outcome.created, 0, "시작이 끝나지 않았다");
    assert_eq!(outcome.runs, 0, "모델 VM을 띄우지 않았다");
    assert!(outcome.took < Duration::from_secs(8), "{:?}", outcome.took);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_while_the_vm_boots_releases_the_run_and_leaves_no_vm() {
    let Some(outcome) = shutdown_while_starting(When::After, false, "shutboot").await else {
        return;
    };
    // VM 프로세스를 막 받았다: 끊고 지웠다. 래퍼가 `run`을 적기 전에 끊길 수도 있어 많아야 한 번이다(남은 VM이 없다는 것은 시나리오 안에서 이미 확인했다)
    assert_eq!(outcome.created, 1);
    assert!(outcome.runs <= 1, "{}", outcome.runs);
    assert!(outcome.took < Duration::from_secs(8), "{:?}", outcome.took);
}
