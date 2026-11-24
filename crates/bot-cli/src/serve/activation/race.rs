//! 같은 집합을 두 호스트가 함께 올리다 한쪽이 먼저 게시하는 경합(실제 PG). 준비 뒤 게시 전에 옛 프로세스가 옛 판본의 실행을 접수하고 다른 프로세스가
//! 그 판본을 복원해 먼저 게시하면, 뒤 프로세스는 쓰기 충돌(`Conflict`) 뒤 같은 집합에 합류한다 — 합류하기 전에 그 판본도 복원해야 하고 못 하면 시작하지 않는다.
//! 호스트마다 따로 연 저장소 풀·레지스트리·준비 목록을 쓰고 단계를 직접 이어 프로세스 사이의 순서를 그대로 재현한다(프로세스 안에서 준비와 확정 사이를 멈출 방법이 없다).
//! 팀 시험 DB(`BOT_TEST_DATABASE_URL`)와 `psql`이 필요하고, 필수 모드(`BOT_TEST_REQUIRE_DB=1`)에서는 없으면 건너뛰지 않고 실패한다.
//! 시험은 다중 스레드 런타임에서 돌린다: 오류로 일찍 끝난 트랜잭션의 되돌리기는 풀이 연결을 돌려받을 때 따로 도는 작업이 끝내는데, 단일 스레드면 `TestDb`를 지우는 동기 `psql`이 그 작업을 막아 스키마 삭제가 잠금에서 멈춘다.

use super::*;
use bot_core::{
    command::{Accept, Envelope, JsonPayload, Target},
    id::*,
    port::RunStore,
    profile::ProfileCatalog,
    state::{Conversation, ConversationKind},
};
use bot_profile::{HostCatalog, Limits};
use bot_store::Config;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

/// 이 시험의 임시 스키마(내 PID가 붙은 것만 만들고 지운다).
struct TestDb {
    base: String,
    name: String,
}

impl TestDb {
    /// 스키마를 만들고 마이그레이션까지 한다. 시험 DB가 없으면 None(건너뜀)이지만 필수 모드에서는 어떤 실패든 패닉이다.
    async fn open() -> Option<TestDb> {
        let required = std::env::var("BOT_TEST_REQUIRE_DB").is_ok_and(|value| value == "1");
        let fail = |why: String| -> Option<TestDb> {
            assert!(
                !required,
                "BOT_TEST_REQUIRE_DB=1인데 시험 DB를 쓸 수 없어요: {why}"
            );
            eprintln!("skipped: {why}");
            None
        };
        let Ok(base) = std::env::var("BOT_TEST_DATABASE_URL") else {
            return fail("BOT_TEST_DATABASE_URL이 없어요".into());
        };
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "act_race_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        match Command::new("psql")
            .args([&base, "-qAtc", &format!("create schema {name}")])
            .stdout(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return fail(format!(
                    "스키마를 만들지 못했어요({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(error) => return fail(format!("psql을 실행하지 못했어요: {error}")),
        }
        let db = TestDb { base, name };
        db.connect().await.migrate().await.unwrap();
        Some(db)
    }

    /// 이 스키마에 붙는 저장소 하나(호스트마다 따로 연다 — 연결 풀이 따로라 서로 다른 프로세스와 같다).
    async fn connect(&self) -> Store {
        let url = format!("{}?options=-c%20search_path%3D{}", self.base, self.name);
        Store::connect(&url, Config::default()).await.unwrap()
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = Command::new("psql")
            .args([
                &self.base,
                "-qAtc",
                &format!("drop schema {} cascade", self.name),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn personal() -> ProfileId {
    ProfileId("personal".into())
}

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles")
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// 예시 프로필을 임시 폴더에 복사해(지침 끝에 `note`를 덧붙여) 레지스트리에 읽는다. 같은 `note`는 같은 판본이다.
fn deploy(registry: &Registry, root: &Path, note: &str) -> Arc<ProfileSet> {
    let dir = root.join("personal");
    let _ = std::fs::remove_dir_all(&dir);
    copy(&examples().join("personal"), &dir);
    let agent = dir.join("agents/main/AGENT.md");
    let mut text = std::fs::read_to_string(&agent).unwrap();
    text.push_str(note);
    std::fs::write(&agent, text).unwrap();
    registry
        .reload(root, &HostCatalog::demo(), &Limits::default())
        .unwrap_or_else(|report| panic!("{report}"))
}

fn publication(set: &ProfileSet, revision: &str) -> Publication {
    Publication {
        revision: Revision(revision.into()),
        profiles: set
            .active()
            .iter()
            .map(|(id, snapshot)| (id.clone(), snapshot.revision.clone()))
            .collect(),
        seeds: Vec::new(),
        schedules: BTreeMap::new(),
    }
}

/// 옛 프로세스가 옛 판본으로 받는 새 실행 접수(CLI 대화).
fn accept(request: &str, revision: &Revision) -> Accept {
    Accept {
        event: Envelope {
            profile: personal(),
            request: RequestId(request.into()),
            binding: BindingId("desk".into()),
            principal: PrincipalId("local:owner".into()),
            conversation: Conversation {
                kind: ConversationKind::Cli,
                id: "owner".into(),
                thread: None,
            },
            http: None,
            attachments: vec![],
            body: JsonPayload(serde_json::json!({"text": "hi"}).to_string()),
        },
        approver: None,
        revision: revision.clone(),
        target: Target::Agent {
            agent: AgentId("main".into()),
            instruction: "hi".into(),
        },
        scopes: BTreeSet::from(["run".to_owned()]),
    }
}

/// 호스트 하나(= `bot serve` 프로세스 하나): 자기 저장소 연결·레지스트리·준비 목록을 쥔다.
struct Host {
    store: Store,
    registry: Registry,
    set: Arc<ProfileSet>,
    publication: Publication,
    prepared: restore::Prepared,
}

impl Host {
    /// 폴더를 읽고 준비(새 집합의 묶음 보관 + 진행 중 작업이 고정한 옛 판본 복원)까지 끝낸 상태. 확정은 `commit`이 따로 한다.
    async fn prepare(db: &TestDb, root: &Path, note: &str, activation: &str) -> Host {
        let store = db.connect().await;
        let registry = Registry::new();
        let set = deploy(&registry, root, note);
        let publication = publication(&set, activation);
        let prepared = restore::prepare(
            &store,
            &registry,
            &set,
            &publication.profiles,
            &Limits::default(),
        )
        .await
        .unwrap();
        Host {
            store,
            registry,
            set,
            publication,
            prepared,
        }
    }

    async fn commit(&mut self, plan: &Activation) -> Result<bool, String> {
        commit_with_refresh(
            &self.store,
            &self.registry,
            &self.publication,
            plan,
            &self.set,
            &mut self.prepared,
        )
        .await
    }

    fn revision(&self) -> Revision {
        self.set.profile(&personal()).unwrap().revision.clone()
    }
}

fn act(revision: &str) -> Revision {
    Revision(revision.into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_that_loses_the_publish_race_restores_what_the_winner_restored_before_it_joins() {
    let Some(db) = TestDb::open().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    // 옛 집합(personal = R1)이 게시돼 있다(옛 호스트가 올렸다).
    let mut old = Host::prepare(&db, root.path(), "\n옛 판.\n", "act-1").await;
    assert!(old.commit(&Activation::Publish).await.unwrap());
    let r1 = old.revision();
    // 새 집합(R2)을 두 호스트가 함께 올린다. 둘 다 준비를 끝냈을 때는 옛 판본 R1을 아무도 참조하지 않는다.
    let mut a = Host::prepare(&db, root.path(), "\n새 판.\n", "act-2").await;
    let mut b = Host::prepare(&db, root.path(), "\n새 판.\n", "act-2").await;
    let r2 = a.revision();
    assert_eq!(r2, b.revision(), "같은 내용은 같은 판본이다");
    assert_ne!(r1, r2);
    for host in [&a, &b] {
        assert_eq!(
            host.prepared.covered(),
            &BTreeSet::from([(personal(), r2.clone())])
        );
        assert_eq!(host.prepared.restored(), 0);
    }
    // 준비 뒤 게시 전에 옛 프로세스(옛 집합이 아직 활성)가 옛 판본으로 새 실행을 접수한다.
    old.store.accept(accept("late", &r1)).await.unwrap();
    let plan = Activation::Replace(act("act-1"));
    // A가 먼저 게시한다: 준비한 목록 밖의 참조(Stale)를 만나 다시 준비해 R1을 복원한 뒤 게시한다.
    assert!(a.commit(&plan).await.unwrap(), "A가 게시했다");
    assert_eq!(a.prepared.restored(), 1);
    assert!(a.registry.lookup(&personal(), &r1).is_some());
    // B는 같은 집합이 이미 게시돼 쓰기가 충돌한다. 합류하기 전에 R1을 복원해야 한다(역할은 이 뒤에야 열린다).
    assert!(!b.commit(&plan).await.unwrap(), "B는 합류했다(쓰기 없음)");
    assert!(b.prepared.covered().contains(&(personal(), r1.clone())));
    assert_eq!(b.prepared.restored(), 1);
    assert!(
        b.registry.lookup(&personal(), &r1).is_some(),
        "B도 그 실행에 쓸 옛 판본을 등록했다"
    );
    // 게시가 보인 뒤에는 옛 판본으로 새 실행을 접수할 수 없다 — 필수 목록은 더 늘지 않아 합류 때의 대조 한 번으로 충분하다.
    assert!(matches!(
        old.store.accept(accept("later", &r1)).await,
        Err(Error::Stale)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_losing_host_that_cannot_restore_what_was_pinned_meanwhile_refuses_to_start() {
    let Some(db) = TestDb::open().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    // 옛 집합을 묶음 보관 없이 게시한다(묶음을 보관하지 않던 옛 버전이 올렸다): R1의 묶음은 저장소에 없다.
    let (old_registry, old_store) = (Registry::new(), db.connect().await);
    let old_set = deploy(&old_registry, root.path(), "\n옛 판.\n");
    let r1 = old_set.profile(&personal()).unwrap().revision.clone();
    let old_profiles = publication(&old_set, "act-1").profiles;
    old_store
        .replace(
            None,
            act("act-1"),
            old_profiles,
            Vec::new(),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let mut b = Host::prepare(&db, root.path(), "\n새 판.\n", "act-2").await;
    // 준비 뒤: 옛 프로세스가 R1의 실행을 접수하고, 다른 프로세스가 같은 집합을 먼저 게시했다.
    old_store.accept(accept("late", &r1)).await.unwrap();
    old_store
        .replace(
            Some(act("act-1")),
            act("act-2"),
            b.publication.profiles.clone(),
            Vec::new(),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    // B는 합류 전에 R1을 복원해야 하는데 묶음이 없다: 시작하지 않고 판본·프로필·안내를 알린다. 준비한 목록도 등록도 그대로다.
    let before = b.prepared.covered().clone();
    let message = b
        .commit(&Activation::Replace(act("act-1")))
        .await
        .unwrap_err();
    assert!(
        message.contains(&format!(
            "프로필 personal 판본 {}: 보관한 묶음이 없어요",
            r1.0
        )),
        "{message}"
    );
    assert!(message.contains("bot profile-bundle put"), "{message}");
    assert_eq!(b.prepared.covered(), &before);
    assert!(b.registry.lookup(&personal(), &r1).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_what_the_peer_published_needs_no_restore_when_nothing_new_is_pinned() {
    let Some(db) = TestDb::open().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let mut old = Host::prepare(&db, root.path(), "\n옛 판.\n", "act-1").await;
    assert!(old.commit(&Activation::Publish).await.unwrap());
    let mut a = Host::prepare(&db, root.path(), "\n새 판.\n", "act-2").await;
    let mut b = Host::prepare(&db, root.path(), "\n새 판.\n", "act-2").await;
    let plan = Activation::Replace(act("act-1"));
    assert!(a.commit(&plan).await.unwrap());
    // 새로 고정된 참조가 없으면 목록을 다시 준비하지 않고 바로 합류한다.
    assert!(!b.commit(&plan).await.unwrap());
    assert_eq!(b.prepared.restored(), 0);
    assert_eq!(
        b.prepared.covered(),
        &BTreeSet::from([(personal(), b.revision())])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_different_set_published_meanwhile_still_stops_the_start() {
    let Some(db) = TestDb::open().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let mut old = Host::prepare(&db, root.path(), "\n옛 판.\n", "act-1").await;
    assert!(old.commit(&Activation::Publish).await.unwrap());
    let mut a = Host::prepare(&db, root.path(), "\n새 판.\n", "act-2").await;
    let mut c = Host::prepare(&db, root.path(), "\n다른 판.\n", "act-3").await;
    let plan = Activation::Replace(act("act-1"));
    assert!(a.commit(&plan).await.unwrap());
    // 그 사이 다른 집합이 게시됐다: 합류가 아니므로 멈춘다(준비한 목록과 상관없다).
    let message = c.commit(&plan).await.unwrap_err();
    assert!(message.contains("활성 판본이 바뀌었어요"), "{message}");
}
