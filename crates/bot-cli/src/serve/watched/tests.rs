use super::*;
use bot_core::port::Error;
use std::{collections::BTreeMap, sync::Mutex, time::Instant};

struct Rotating(Arc<Mutex<Option<Vec<u8>>>>);

struct Bound(Arc<Mutex<Option<Vec<u8>>>>);

impl SecretStore for Rotating {
    fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let value = self.0.clone();
        Box::pin(async move { Ok(Box::new(Bound(value)) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for Bound {
    fn resolve(&self, _: SecretName) -> Reply<'_, SecretValue> {
        let value = self.0.lock().unwrap().clone();
        Box::pin(async move { value.map(SecretValue).ok_or(Error::Missing) })
    }
}

fn setup(first: &str) -> (Watched, Arc<Mutex<Option<Vec<u8>>>>, Redactor) {
    let shared = Arc::new(Mutex::new(Some(first.as_bytes().to_vec())));
    let redactor = Redactor::default();
    let watched = Watched::new(Arc::new(Rotating(shared.clone())), redactor.clone());
    (watched, shared, redactor)
}

async fn read(watched: &Watched) -> Result<Vec<u8>, Error> {
    let handle = watched.for_profile(ProfileId("personal".into())).await?;
    handle
        .resolve(SecretName::parse("model-key").unwrap())
        .await
        .map(|v| v.0)
}

const OLD: &str = "provider-key-OLD-71c3d9";
const NEW: &str = "provider-key-NEW-5be802";

#[tokio::test]
async fn a_value_that_is_read_is_masked_from_then_on_and_the_caller_still_gets_it() {
    let (watched, _, redactor) = setup(OLD);
    assert!(!redactor.has_value(OLD), "unknown before read");
    assert_eq!(read(&watched).await.unwrap(), OLD.as_bytes());
    assert!(redactor.has_value(&format!("Bearer {OLD}")));
    assert!(!redactor.mask(&format!("Bearer {OLD}")).contains(OLD));
}

#[tokio::test]
async fn a_rotated_key_is_masked_as_soon_as_it_is_read_and_the_old_one_stays_masked() {
    let (watched, shared, redactor) = setup(OLD);
    read(&watched).await.unwrap();
    *shared.lock().unwrap() = Some(NEW.as_bytes().to_vec());
    assert!(!redactor.has_value(NEW));
    assert_eq!(read(&watched).await.unwrap(), NEW.as_bytes());
    assert!(redactor.has_value(NEW), "rotated value is masked");
    assert!(redactor.has_value(OLD), "previous value remains masked");
}

#[tokio::test]
async fn failed_reads_and_non_text_values_add_nothing() {
    let (watched, shared, redactor) = setup(OLD);
    *shared.lock().unwrap() = None;
    assert!(matches!(read(&watched).await, Err(Error::Missing)));
    *shared.lock().unwrap() = Some(vec![0xff, 0xfe, 0x00, 0x9f]);
    assert!(
        read(&watched).await.is_ok(),
        "non-UTF-8 secret bytes still pass through"
    );
    *shared.lock().unwrap() = Some(Vec::new());
    assert!(read(&watched).await.is_ok());
    assert!(!redactor.has_value("\u{0}") && !redactor.has_value(OLD));
    assert!(
        format!("{redactor:?}").contains("values: 0"),
        "{redactor:?}"
    );
}

#[tokio::test]
async fn the_masking_list_is_shared_with_every_clone() {
    let (watched, _, redactor) = setup(OLD);
    let other = redactor.clone();
    read(&watched).await.unwrap();
    assert!(other.has_value(OLD), "clones share the live masking list");
}

#[tokio::test]
async fn a_value_with_surrounding_whitespace_is_masked_as_read_and_as_the_gateway_trims_it() {
    for (stored, bare) in [
        ("slack-bot-ZZZ123456789\n", "slack-bot-ZZZ123456789"),
        ("slack-bot-ZZZ123456789\r\n", "slack-bot-ZZZ123456789"),
        ("  \tslack-bot-ZZZ123456789 \n\n", "slack-bot-ZZZ123456789"),
        ("slack-bot-ZZZ123456789", "slack-bot-ZZZ123456789"),
    ] {
        let (watched, _, redactor) = setup(stored);
        assert_eq!(
            read(&watched).await.unwrap(),
            stored.as_bytes(),
            "caller receives original bytes"
        );
        assert!(
            redactor.has_value(&format!("Authorization: Bearer {bare}")),
            "{stored:?}: trimmed form"
        );
        assert!(
            !redactor.mask(&format!("x {bare} y")).contains(bare),
            "{stored:?}: masked"
        );
        assert!(
            redactor.has_value(stored),
            "{stored:?}: untrimmed form also masked"
        );
        let kinds = if stored == bare { 1 } else { 2 };
        assert!(
            format!("{redactor:?}").contains(&format!("values: {kinds}")),
            "{stored:?}: {redactor:?}"
        );
    }
}

#[tokio::test]
async fn inner_whitespace_stays_and_a_value_of_only_whitespace_adds_nothing() {
    let (watched, _, redactor) = setup("two words\n");
    read(&watched).await.unwrap();
    assert!(
        redactor.has_value("a two words b"),
        "inner whitespace preserved"
    );
    assert!(
        !redactor.has_value("two") && !redactor.has_value("words"),
        "must not split into words"
    );
    for blank in ["", " ", "\n", " \t\r\n "] {
        let (watched, _, redactor) = setup(blank);
        read(&watched).await.unwrap();
        assert!(
            !redactor.has_value("a b\nc") && !redactor.has_value("x"),
            "{blank:?}: must never mask arbitrary whitespace"
        );
        assert!(
            format!("{redactor:?}").contains("values: 0"),
            "{blank:?}: {redactor:?}"
        );
    }
}

type Table = Arc<Mutex<BTreeMap<String, String>>>;

// Profile `silent` never returns a handle; secret `stuck` never resolves.
struct Shelf(Table);

struct ShelfHandle(Table);

impl SecretStore for Shelf {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let table = self.0.clone();
        Box::pin(async move {
            if profile.0 == "silent" {
                std::future::pending::<()>().await;
            }
            Ok(Box::new(ShelfHandle(table)) as Box<dyn ProfileSecrets>)
        })
    }
}

impl ProfileSecrets for ShelfHandle {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let found = self.0.lock().unwrap().get(name.as_str()).cloned();
        Box::pin(async move {
            if name.as_str() == "stuck" {
                std::future::pending::<()>().await;
            }
            found
                .map(|value| SecretValue(value.into_bytes()))
                .ok_or(Error::Missing)
        })
    }
}

fn shelf(entries: &[(&str, &str)]) -> (Arc<Watched>, Table, Redactor) {
    let table: Table = Arc::new(Mutex::new(
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    ));
    let redactor = Redactor::default();
    let watched = Arc::new(Watched::new(
        Arc::new(Shelf(table.clone())),
        redactor.clone(),
    ));
    (watched, table, redactor)
}

fn plan(entries: &[(&str, &[&str])]) -> Plan {
    entries
        .iter()
        .map(|(profile, names)| {
            (
                ProfileId((*profile).into()),
                names
                    .iter()
                    .map(|name| SecretName::parse(name).unwrap())
                    .collect(),
            )
        })
        .collect()
}

fn quick() -> Pace {
    Pace {
        every: Duration::from_millis(20),
        read: Duration::from_millis(200),
        gap: Duration::ZERO,
    }
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("{what}");
}

const A_VALUE: &str = "shelf-value-A-0123456789abcdef";
const B_VALUE: &str = "shelf-value-B-0123456789abcdef";

#[tokio::test]
async fn a_round_reads_every_declared_name_masks_what_it_reads_and_counts_the_rest() {
    let (watched, _, redactor) = shelf(&[("a", A_VALUE), ("b", B_VALUE)]);
    let round = read_round(
        &watched,
        &plan(&[("personal", &["a", "b", "gone"]), ("work", &["a"])]),
        quick(),
    )
    .await;
    assert_eq!(
        round,
        Round {
            read: 3,
            failed: 1,
            timed_out: 0
        },
        "3 read, 1 missing"
    );
    assert!(redactor.has_value(&format!("pre {A_VALUE} post")));
    assert!(redactor.has_value(B_VALUE));
    let shown = format!("{round:?}");
    assert!(
        !shown.contains(A_VALUE),
        "round summary must not leak values: {shown}"
    );
}

#[tokio::test]
async fn a_read_that_never_answers_is_cut_off_and_the_names_after_it_are_still_read() {
    let (watched, _, redactor) = shelf(&[("a", A_VALUE)]);
    let started = Instant::now();
    let round = timeout(
        Duration::from_secs(10),
        read_round(
            &watched,
            &plan(&[("silent", &["a", "b"]), ("personal", &["stuck", "a"])]),
            quick(),
        ),
    )
    .await
    .expect("unresponsive store blocked the reread round");
    assert_eq!(
        round,
        Round {
            read: 1,
            failed: 0,
            timed_out: 3
        }
    );
    assert!(redactor.has_value(A_VALUE), "subsequent name was read");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "bounded by read timeout"
    );
}

#[test]
fn repeated_timeouts_back_off_to_ten_periods_and_any_other_round_resets() {
    let every = Duration::from_secs(60);
    let slow = Round {
        timed_out: 1,
        ..Round::default()
    };
    let mut wait = every;
    let mut seen = Vec::new();
    for _ in 0..6 {
        wait = next_wait(every, wait, &slow);
        seen.push(wait.as_secs());
    }
    assert_eq!(seen, [120, 240, 480, 600, 600, 600]);
    let clean = Round {
        read: 3,
        failed: 1,
        timed_out: 0,
    };
    assert_eq!(
        next_wait(every, wait, &clean),
        every,
        "non-timeout failures do not back off; clean round resets interval"
    );
}

#[tokio::test]
async fn the_loop_picks_up_a_value_rotated_after_it_started_keeps_the_old_one_and_stops_on_cancel()
{
    let (watched, table, redactor) = shelf(&[("a", A_VALUE)]);
    let stop = CancellationToken::new();
    let task = tokio::spawn(reread(
        watched,
        plan(&[("personal", &["a"])]),
        quick(),
        stop.clone(),
    ));
    until("initial round did not read original value", || {
        redactor.has_value(A_VALUE)
    })
    .await;
    table.lock().unwrap().insert("a".into(), B_VALUE.into());
    until("reread loop did not pick up rotated value", || {
        redactor.has_value(B_VALUE)
    })
    .await;
    assert!(redactor.has_value(A_VALUE), "previous value remains masked");
    stop.cancel();
    timeout(Duration::from_secs(2), task)
        .await
        .expect("reread loop did not stop on cancel")
        .unwrap();
}

#[tokio::test]
async fn cancelling_stops_the_loop_even_in_the_middle_of_a_stuck_read_and_an_empty_plan_ends_at_once()
 {
    let (watched, _, _) = shelf(&[("a", A_VALUE)]);
    let patient = Pace {
        read: Duration::from_secs(60),
        ..quick()
    };
    let stop = CancellationToken::new();
    let task = tokio::spawn(reread(
        watched.clone(),
        plan(&[("personal", &["stuck"])]),
        patient,
        stop.clone(),
    ));
    sleep(Duration::from_millis(200)).await;
    assert!(!task.is_finished());
    stop.cancel();
    timeout(Duration::from_secs(2), task)
        .await
        .expect("reread loop did not stop during a stuck read")
        .unwrap();
    timeout(
        Duration::from_secs(1),
        reread(watched, Vec::new(), quick(), CancellationToken::new()),
    )
    .await
    .expect("empty plan did not exit immediately");
}

// Structural invariant: only `mod.rs`, `trace.rs`, and `watched.rs` may hold a raw `SecretStore`,
// and only `trace.rs` wraps one in `Watched`, so roles cannot bypass redaction.
#[test]
fn only_the_observation_setup_holds_a_raw_store_and_wraps_one() {
    use std::path::{Path, PathBuf};
    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "tests" {
                    sources(&path, out);
                }
            } else if name.ends_with(".rs") && name != "tests.rs" {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/serve"),
        &mut files,
    );
    assert!(
        files.len() > 10,
        "failed to locate serve sources: {files:?}"
    );
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let code = text.split("#[cfg(test)]").next().unwrap();
        if !matches!(name.as_str(), "mod.rs" | "trace.rs" | "watched.rs") {
            assert!(
                !code.contains("Arc<dyn SecretStore>"),
                "{name}: roles must accept `Arc<Watched>` (`Observe.secrets`), not a raw SecretStore"
            );
        }
        if !matches!(name.as_str(), "trace.rs" | "watched.rs") {
            assert!(
                !code.contains("Watched::new("),
                "{name}: only trace.rs may construct `Watched`"
            );
        }
    }
}
