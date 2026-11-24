//! 읽어 쓰는 비밀 값은 쓰기 전에 가림 장치에 들어간다: 키를 교체해도 새 값이 곧바로 가려지고, 교체 전 값도 계속 가려진다. 스스로 읽지 않는 값은
//! 선언된 이름을 주기적으로 다시 읽어 따라잡는다(속도 제한·시간 초과·되돌림 포함). 역할은 날 보관소를 받지 못한다.

use super::*;
use bot_core::port::Error;
use std::{collections::BTreeMap, sync::Mutex, time::Instant};

/// 값을 바꿀 수 있는 보관소(키 교체를 흉내 낸다). 값이 없으면 `Missing`.
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
    assert!(!redactor.has_value(OLD), "읽기 전에는 모른다");
    assert_eq!(read(&watched).await.unwrap(), OLD.as_bytes());
    assert!(redactor.has_value(&format!("Bearer {OLD}")));
    assert!(!redactor.mask(&format!("Bearer {OLD}")).contains(OLD));
}

#[tokio::test]
async fn a_rotated_key_is_masked_as_soon_as_it_is_read_and_the_old_one_stays_masked() {
    let (watched, shared, redactor) = setup(OLD);
    read(&watched).await.unwrap();
    // 운영자가 키를 교체했다: 새 값은 아직 읽히기 전이다
    *shared.lock().unwrap() = Some(NEW.as_bytes().to_vec());
    assert!(!redactor.has_value(NEW));
    // 새 값을 읽는 순간(쓰이기 전) 목록에 들어간다
    assert_eq!(read(&watched).await.unwrap(), NEW.as_bytes());
    assert!(redactor.has_value(NEW), "새 값이 가려진다");
    assert!(redactor.has_value(OLD), "교체 전 값도 계속 가려진다");
}

#[tokio::test]
async fn failed_reads_and_non_text_values_add_nothing() {
    let (watched, shared, redactor) = setup(OLD);
    *shared.lock().unwrap() = None;
    assert!(matches!(read(&watched).await, Err(Error::Missing)));
    *shared.lock().unwrap() = Some(vec![0xff, 0xfe, 0x00, 0x9f]);
    assert!(
        read(&watched).await.is_ok(),
        "글이 아닌 값도 그대로 넘겨 준다"
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
    assert!(
        other.has_value(OLD),
        "복제본(내보내기·응답 검사)도 같은 목록을 본다"
    );
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
            "호출한 쪽은 읽은 그대로 받는다"
        );
        assert!(
            redactor.has_value(&format!("Authorization: Bearer {bare}")),
            "{stored:?}: 떼어 쓰는 모양"
        );
        assert!(
            !redactor.mask(&format!("x {bare} y")).contains(bare),
            "{stored:?}: 가려진다"
        );
        assert!(
            redactor.has_value(stored),
            "{stored:?}: 읽은 그대로의 모양도 가려진다"
        );
        // 목록에는 읽은 그대로와 뗀 모양이 따로 든다(둘이 같으면 하나)
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
        "안쪽 공백은 그대로 둔 모양"
    );
    assert!(
        !redactor.has_value("two") && !redactor.has_value("words"),
        "단어로 쪼개 더하지 않는다"
    );
    for blank in ["", " ", "\n", " \t\r\n "] {
        let (watched, _, redactor) = setup(blank);
        read(&watched).await.unwrap();
        assert!(
            !redactor.has_value("a b\nc") && !redactor.has_value("x"),
            "{blank:?}: 어떤 글의 공백이든 가리면 안 된다"
        );
        assert!(
            format!("{redactor:?}").contains("values: 0"),
            "{blank:?}: {redactor:?}"
        );
    }
}

// ---- 선언된 이름을 주기적으로 다시 읽기 ----

type Table = Arc<Mutex<BTreeMap<String, String>>>;

/// 이름마다 값을 주는 보관소. 이름 `stuck`은 영원히 답하지 않고, 프로필 `silent`는 핸들도 주지 않는다.
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

/// 시험용 빠른 속도(운영은 `Pace::new`: 읽기 5초·쉼 25ms)
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
        "읽은 것 3(a·b·work의 a)·없는 것 1"
    );
    assert!(redactor.has_value(&format!("앞 {A_VALUE} 뒤")));
    assert!(redactor.has_value(B_VALUE));
    let shown = format!("{round:?}");
    assert!(!shown.contains(A_VALUE), "결과에는 값이 없다: {shown}");
}

#[tokio::test]
async fn a_read_that_never_answers_is_cut_off_and_the_names_after_it_are_still_read() {
    let (watched, _, redactor) = shelf(&[("a", A_VALUE)]);
    let started = Instant::now();
    let round = timeout(
        Duration::from_secs(10),
        read_round(
            &watched,
            // 핸들을 주지 않는 프로필 하나(이름 둘), 응답 없는 이름 하나, 읽히는 이름 하나
            &plan(&[("silent", &["a", "b"]), ("personal", &["stuck", "a"])]),
            quick(),
        ),
    )
    .await
    .expect("응답 없는 보관소가 바퀴를 붙들었어요");
    assert_eq!(
        round,
        Round {
            read: 1,
            failed: 0,
            timed_out: 3
        }
    );
    assert!(redactor.has_value(A_VALUE), "뒤의 이름은 읽었다");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "줄 수 있는 시간만 썼다"
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
        "없는 값(실패)은 늘리지 않고, 시간 초과가 없으면 원래 주기"
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
    until("첫 바퀴가 옛 값을 읽지 못했어요", || {
        redactor.has_value(A_VALUE)
    })
    .await;
    // 운영자가 키를 교체했다. 이 프로세스는 그 값을 읽어 쓸 일이 없다 — 주기 안에 다시 읽어 목록에 더한다
    table.lock().unwrap().insert("a".into(), B_VALUE.into());
    until("교체된 새 값을 다시 읽지 못했어요", || {
        redactor.has_value(B_VALUE)
    })
    .await;
    assert!(redactor.has_value(A_VALUE), "교체 전 값도 계속 가려진다");
    stop.cancel();
    timeout(Duration::from_secs(2), task)
        .await
        .expect("stop이 와도 끝나지 않았어요")
        .unwrap();
}

#[tokio::test]
async fn cancelling_stops_the_loop_even_in_the_middle_of_a_stuck_read_and_an_empty_plan_ends_at_once()
 {
    let (watched, _, _) = shelf(&[("a", A_VALUE)]);
    // 읽기에 줄 시간이 길어도(60초) stop이 오면 바로 끝난다
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
    sleep(Duration::from_millis(200)).await; // 바퀴가 응답 없는 읽기에 들어갈 시간
    assert!(!task.is_finished());
    stop.cancel();
    timeout(Duration::from_secs(2), task)
        .await
        .expect("읽기 도중 stop이 와도 끝나지 않았어요")
        .unwrap();
    // 읽을 이름이 하나도 없으면 작업이 바로 끝난다
    timeout(
        Duration::from_secs(1),
        reread(watched, Vec::new(), quick(), CancellationToken::new()),
    )
    .await
    .expect("빈 계획이 끝나지 않았어요");
}

/// 소스를 훑는 구조 시험: 역할은 날 보관소를 받지 못하고(`Arc<dyn SecretStore>`는 시작 조립인 `mod.rs`·`trace.rs`·이 파일에만 있다), `Watched`를 만드는 곳은 `trace.rs`
/// 하나다. 이 시험이 막는 것: 새 역할 입력을 `Arc<dyn SecretStore>`로 되돌리거나 아무 데서나 보관소를 감싸 가림 장치를 거치지 않는 길을 만드는 일.
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
    assert!(files.len() > 10, "serve 소스를 찾지 못했어요: {files:?}");
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        // 인라인 시험 모듈(파일 끝)은 제외한다
        let code = text.split("#[cfg(test)]").next().unwrap();
        if !matches!(name.as_str(), "mod.rs" | "trace.rs" | "watched.rs") {
            assert!(
                !code.contains("Arc<dyn SecretStore>"),
                "{name}: 역할 입력은 날 보관소를 받으면 안 돼요 — `Arc<Watched>`(Observe.secrets)를 쓰세요"
            );
        }
        if !matches!(name.as_str(), "trace.rs" | "watched.rs") {
            assert!(
                !code.contains("Watched::new("),
                "{name}: 보관소를 가림 장치로 감싸는 곳은 trace.rs 하나예요"
            );
        }
    }
}
