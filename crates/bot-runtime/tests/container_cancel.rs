//! Canary tests with the real Apple `container` CLI for cancelling a booting VM and releasing runs cleanly on shutdown with `grace = 0`.
//! Run: `BOT_CONTAINER_CANARY=required BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test container_cancel -- --nocapture`

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
    let anchored = ContainerSandbox::start(env.config()).await.expect("start");
    let (relay_port, _rx, _relay) = vm::service(anchored.bind_addr(0));
    let sandbox = anchored.relay(relay_port).await.expect("relay");
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
        let mut process = sandbox.spawn(spec).await.expect("spawn");
        // Sweep kill timings across 0..55ms to hit the race where container creation arrives after delete.
        tokio::time::sleep(Duration::from_millis((round * 5) % 60)).await;
        process.kill().await;
        drop(process);
        let left = models(&env, &anchor);
        assert!(left.is_empty(), "round {round}: {left:?}");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(models(&env, &anchor).is_empty(), "{:?}", env.ids());
    sandbox.shutdown().await;
    assert!(env.ids().is_empty(), "{:?}", env.ids());
}

#[derive(Clone, Copy)]
enum When {
    Before,
    After,
}

struct Signalling {
    inner: Arc<ContainerSandbox>,
    shutdown: watch::Sender<bool>,
    when: When,
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

/// Mirrors production `Quiescer` order: reap the attempt VM, then revoke its relay session token.
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

/// Wraps `container` to log non-anchor `run` invocations and optionally delay `inspect` by 1s.
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

struct Outcome {
    took: Duration,
    runs: usize,
    created: usize,
}

async fn shutdown_while_starting(when: When, slow_checks: bool, tag: &str) -> Option<Outcome> {
    let w = world().await?;
    let program = vm::available()?;
    let _serial = vm::serial().await;
    let env = vm::Env::new(program.clone(), tag);
    let (log, flag) = (env.root.join("runs.log"), env.root.join("slow-inspect"));
    let mut config = env.config();
    config.program = wrapper(&env, &log, &flag).to_str().unwrap().to_owned();
    let anchored = ContainerSandbox::start(config).await.expect("start");
    let anchor = vm::running_anchor(&program, &env.instance).await;
    if slow_checks {
        std::fs::write(&flag, "").unwrap();
    }
    let run = w.accept(&format!("req-{tag}"), "Show today's agenda").await;
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
            let relay = Arc::new(anchored.relay(front.port()).await.expect("relay"));
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
            config.grace = Duration::ZERO;
            config.cleanup = Duration::from_secs(10);
        },
    )
    .await;
    let sandbox = holder.lock().unwrap().take().expect("sandbox");
    let worker = host.worker.with_quiesce(Arc::new(Reaper {
        sandbox: sandbox.clone(),
        sessions: host.relays.sessions.clone(),
    }));
    let began = Instant::now();
    tokio::time::timeout(Duration::from_secs(60), worker.run(shutdown_rx))
        .await
        .expect("worker must exit");
    let took = began.elapsed();

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
    assert!(env.ids().is_empty(), "{:?}", env.ids());
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
    assert_eq!(
        outcome.created, 0,
        "spawn must abort during readiness check"
    );
    assert_eq!(outcome.runs, 0, "no model VM should start");
    assert!(outcome.took < Duration::from_secs(8), "{:?}", outcome.took);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_while_the_vm_boots_releases_the_run_and_leaves_no_vm() {
    let Some(outcome) = shutdown_while_starting(When::After, false, "shutboot").await else {
        return;
    };
    assert_eq!(outcome.created, 1);
    assert!(outcome.runs <= 1, "{}", outcome.runs);
    assert!(outcome.took < Duration::from_secs(8), "{:?}", outcome.took);
}
