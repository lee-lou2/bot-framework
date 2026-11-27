//! PostgreSQL concurrency tests for multi-host activation publish races and grant issuance.
// Uses multi-threaded tokio runtime because synchronous `psql` schema drops in `Drop` can deadlock
// against background pool rollbacks on a current-thread runtime.

use super::*;
use crate::serve::grants::{self, fixture};
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

struct TestDb {
    base: String,
    name: String,
}

impl TestDb {
    async fn open() -> Option<TestDb> {
        let required = std::env::var("BOT_TEST_REQUIRE_DB").is_ok_and(|value| value == "1");
        let fail = |why: String| -> Option<TestDb> {
            assert!(
                !required,
                "BOT_TEST_REQUIRE_DB=1 but test DB is unavailable: {why}"
            );
            eprintln!("skipped: {why}");
            None
        };
        let Ok(base) = std::env::var("BOT_TEST_DATABASE_URL") else {
            return fail("BOT_TEST_DATABASE_URL is not set".into());
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
                    "failed to create schema ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(error) => return fail(format!("failed to run psql: {error}")),
        }
        let db = TestDb { base, name };
        db.connect().await.migrate().await.unwrap();
        Some(db)
    }

    fn query(&self, sql: &str) -> String {
        let out = Command::new("psql")
            .args([&self.base, "-Atc", &sql.replace("{s}", &self.name)])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

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
        grants: Vec::new(),
    }
}

fn accept(request: &str, revision: &Revision) -> Accept {
    Accept {
        reply_transport: None,
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

struct Host {
    store: Store,
    registry: Registry,
    set: Arc<ProfileSet>,
    publication: Publication,
    prepared: restore::Prepared,
}

impl Host {
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
    let mut old = Host::prepare(&db, root.path(), "\nold rev.\n", "act-1").await;
    assert!(old.commit(&Activation::Publish).await.unwrap());
    let r1 = old.revision();
    let mut a = Host::prepare(&db, root.path(), "\nnew rev.\n", "act-2").await;
    let mut b = Host::prepare(&db, root.path(), "\nnew rev.\n", "act-2").await;
    let r2 = a.revision();
    assert_eq!(r2, b.revision(), "identical contents yield same revision");
    assert_ne!(r1, r2);
    for host in [&a, &b] {
        assert_eq!(
            host.prepared.covered(),
            &BTreeSet::from([(personal(), r2.clone())])
        );
        assert_eq!(host.prepared.restored(), 0);
    }
    // After preparation and before commit, the old host accepts a run pinned to R1.
    old.store.accept(accept("late", &r1)).await.unwrap();
    let plan = Activation::Replace(act("act-1"));
    assert!(a.commit(&plan).await.unwrap(), "A published");
    assert_eq!(a.prepared.restored(), 1);
    assert!(a.registry.lookup(&personal(), &r1).is_some());
    // B hits Conflict on the same set and must restore R1 before joining.
    assert!(!b.commit(&plan).await.unwrap(), "B joined without writing");
    assert!(b.prepared.covered().contains(&(personal(), r1.clone())));
    assert_eq!(b.prepared.restored(), 1);
    assert!(
        b.registry.lookup(&personal(), &r1).is_some(),
        "B adopted the historical revision for the in-flight run"
    );
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
    // Simulate a legacy host publishing R1 without storing its bundle.
    let (old_registry, old_store) = (Registry::new(), db.connect().await);
    let old_set = deploy(&old_registry, root.path(), "\nold rev.\n");
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
    let mut b = Host::prepare(&db, root.path(), "\nnew rev.\n", "act-2").await;
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
    let before = b.prepared.covered().clone();
    let message = b
        .commit(&Activation::Replace(act("act-1")))
        .await
        .unwrap_err();
    assert!(
        message.contains(&format!(
            "profile personal revision {}: stored bundle is missing",
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
    let mut old = Host::prepare(&db, root.path(), "\nold rev.\n", "act-1").await;
    assert!(old.commit(&Activation::Publish).await.unwrap());
    let mut a = Host::prepare(&db, root.path(), "\nnew rev.\n", "act-2").await;
    let mut b = Host::prepare(&db, root.path(), "\nnew rev.\n", "act-2").await;
    let plan = Activation::Replace(act("act-1"));
    assert!(a.commit(&plan).await.unwrap());
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
    let mut old = Host::prepare(&db, root.path(), "\nold rev.\n", "act-1").await;
    assert!(old.commit(&Activation::Publish).await.unwrap());
    let mut a = Host::prepare(&db, root.path(), "\nnew rev.\n", "act-2").await;
    let mut c = Host::prepare(&db, root.path(), "\nother rev.\n", "act-3").await;
    let plan = Activation::Replace(act("act-1"));
    assert!(a.commit(&plan).await.unwrap());
    let message = c.commit(&plan).await.unwrap_err();
    assert!(
        message.to_ascii_lowercase().contains("active revision"),
        "{message}"
    );
}

async fn prepare_granted(
    db: &TestDb,
    root: &Path,
    activation: &str,
    explicit: bool,
) -> (Host, usize) {
    let store = db.connect().await;
    let registry = Registry::new();
    let set = fixture::load_into(&registry, root, fixture::NOTIFY);
    let installed = BTreeMap::from([("demo-agenda".to_owned(), Revision("img-1".into()))]);
    let mut publication = publication(&set, activation);
    publication.grants = grants::grants_of(&set, &installed).unwrap();
    let (publication, unissued) = publication.for_run(explicit);
    let prepared = restore::prepare(
        &store,
        &registry,
        &set,
        &publication.profiles,
        &Limits::default(),
    )
    .await
    .unwrap();
    let host = Host {
        store,
        registry,
        set,
        publication,
        prepared,
    };
    (host, unissued)
}

impl Host {
    async fn start(&mut self, explicit: bool) -> (Activation, bool) {
        let plan = plan_activation(&self.store, &self.publication, explicit)
            .await
            .unwrap();
        let wrote = self.commit(&plan).await.unwrap();
        (plan, wrote)
    }
}

const ACTIVE_GRANTS: &str = "select count(*) from {s}.bot_grants where state = 'active'";
const ACTIVE_GRANT_ID: &str = "select id from {s}.bot_grants where state = 'active'";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_start_never_issues_a_grant_and_only_an_explicit_activation_does() {
    let Some(db) = TestDb::open().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let (mut plain, unissued) = prepare_granted(&db, root.path(), "act-1", false).await;
    assert_eq!(unissued, 1, "declared grant must stay unissued");
    assert_eq!(
        plain.start(false).await,
        (Activation::Publish, true),
        "initial publish"
    );
    assert_eq!(
        db.query("select count(*) from {s}.bot_active_profiles"),
        "1"
    );
    assert_eq!(
        db.query(ACTIVE_GRANTS),
        "0",
        "plain first publish must not issue pre-approval grants"
    );
    let (mut again, _) = prepare_granted(&db, root.path(), "act-1", false).await;
    assert_eq!(again.start(false).await, (Activation::Join, false));
    assert_eq!(db.query(ACTIVE_GRANTS), "0");
    let (mut explicit, unissued) = prepare_granted(&db, root.path(), "act-1", true).await;
    assert_eq!(unissued, 0);
    assert_eq!(explicit.start(true).await, (Activation::Join, false));
    assert_eq!(db.query(ACTIVE_GRANTS), "1");
    let id = db.query(ACTIVE_GRANT_ID);
    assert_eq!(
        db.query("select count(*) from {s}.bot_audit where action = 'preapproval.issue'"),
        "1"
    );
    let (mut repeat, _) = prepare_granted(&db, root.path(), "act-1", true).await;
    assert_eq!(repeat.start(true).await, (Activation::Join, false));
    let (mut restart, _) = prepare_granted(&db, root.path(), "act-1", false).await;
    assert_eq!(restart.start(false).await, (Activation::Join, false));
    assert_eq!(db.query(ACTIVE_GRANT_ID), id);
    assert_eq!(
        db.query("select count(*) from {s}.bot_audit where action like 'preapproval.%'"),
        "1"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_first_publish_issues_the_grant_in_the_same_commit() {
    let Some(db) = TestDb::open().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let (mut first, unissued) = prepare_granted(&db, root.path(), "act-1", true).await;
    assert_eq!(unissued, 0);
    assert_eq!(first.start(true).await, (Activation::Publish, true));
    assert_eq!(db.query(ACTIVE_GRANTS), "1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_activation_that_loses_the_first_publish_race_to_the_same_set_still_issues_the_grant()
 {
    let Some(db) = TestDb::open().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let (mut plain, _) = prepare_granted(&db, root.path(), "act-1", false).await;
    let (mut explicit, _) = prepare_granted(&db, root.path(), "act-1", true).await;
    let plan_plain = plan_activation(&plain.store, &plain.publication, false)
        .await
        .unwrap();
    let plan_explicit = plan_activation(&explicit.store, &explicit.publication, true)
        .await
        .unwrap();
    assert_eq!(
        (&plan_plain, &plan_explicit),
        (&Activation::Publish, &Activation::Publish)
    );
    assert!(plain.commit(&plan_plain).await.unwrap());
    assert_eq!(db.query(ACTIVE_GRANTS), "0");
    // `--activate` lost the CAS to the same set, so it still issues the grant on that revision.
    assert!(!explicit.commit(&plan_explicit).await.unwrap());
    assert_eq!(db.query(ACTIVE_GRANTS), "1");
    assert_eq!(
        db.query("select count(*) from {s}.bot_audit where action = 'preapproval.issue'"),
        "1"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_start_that_loses_the_first_publish_race_to_an_explicit_one_leaves_the_grant_alone()
{
    let Some(db) = TestDb::open().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let (mut plain, _) = prepare_granted(&db, root.path(), "act-1", false).await;
    let (mut explicit, _) = prepare_granted(&db, root.path(), "act-1", true).await;
    let plan_plain = plan_activation(&plain.store, &plain.publication, false)
        .await
        .unwrap();
    let plan_explicit = plan_activation(&explicit.store, &explicit.publication, true)
        .await
        .unwrap();
    assert!(explicit.commit(&plan_explicit).await.unwrap());
    let id = db.query(ACTIVE_GRANT_ID);
    assert!(!plain.commit(&plan_plain).await.unwrap());
    assert_eq!(db.query(ACTIVE_GRANTS), "1");
    assert_eq!(db.query(ACTIVE_GRANT_ID), id);
    assert_eq!(
        db.query("select count(*) from {s}.bot_audit where action like 'preapproval.%'"),
        "1"
    );
}
