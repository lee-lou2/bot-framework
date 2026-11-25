//! `bot serve --role …` — 역할을 조립해 시작한다. 시작 전에 모든 검증(프로필·호스트 등록부·입구 표·자격·저장소)을 끝내고
//! 하나라도 어긋나면 일·발송 0으로 멈춘다. 종료 신호(SIGINT·SIGTERM)는 여기서 소유해 모든 역할에 같은 토큰으로 전한다.
//!
//! 조립된 역할: `gateway`·`admin`. 나머지(scheduler·worker·broker)는 각 크레이트의 시작 함수가 생기면 연결한다 —
//! 그 전에는 요청한 역할 중 하나라도 조립되지 않았으면 이름과 이유를 밝히고 아무것도 시작하지 않는다(`all`도 같다).
mod activation;
mod admin;
pub mod claude;
mod decide;
mod gateway;
mod guard;
mod models;
mod quiesce;
mod restore;
mod runner;
mod scheduler;
pub mod seed;
mod token_file;
mod tools;
mod trace;
mod unusable;
#[doc(hidden)]
pub mod upstream;
mod watched;
mod worker;

use self::activation::{commit_with_refresh, plan_activation};
use crate::{
    cli::Role,
    host::HostFile,
    load::{check_entrances, load, secret_store},
    settings::Settings,
};
use bot_core::{
    command::{MemorySeed, TriggerSpec},
    id::{ProfileId, Revision},
    port::{Error, SecretStore},
    profile::{ActiveRevisions, ProfileCatalog},
    schedule::ScheduleRoutes,
};
use bot_gateway::{
    active::{ActiveCheck, ActiveSet},
    schedule_routes::HostRoutes,
};
use bot_profile::ProfileSet;
use bot_runtime::schedule_set;
use bot_store::{Config, Store};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::IsTerminal, sync::Arc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// 시작하지 않는 역할과 이유. 브로커(도구 실행·승인)는 worker 안에서 같은 프로세스로 돌기 때문에 별도 역할이 아니다 —
/// 요청하면 이유를 밝히고 거절한다.
fn not_assembled(role: Role) -> Option<&'static str> {
    match role {
        Role::Broker => {
            Some("브로커(도구 실행·승인)는 worker에 포함돼 있어요 — --role worker를 쓰세요")
        }
        Role::All | Role::Gateway | Role::Scheduler | Role::Worker | Role::Admin => None,
    }
}

fn explain(error: Error) -> &'static str {
    match error {
        Error::Conflict => "다른 판본이 이미 게시돼 있어요",
        Error::Unavailable => "저장소에 닿지 못했어요",
        Error::Invalid => "형식이 올바르지 않아요",
        _ => "저장소가 거절했어요",
    }
}

/// 게시할 활성 집합. 활성화 판본은 프로필 집합 판본에 호스트의 **권한 설정 지문**(비밀 값·호스트별 경로 제외)을 묶은 값이다 —
/// 게이트웨이의 실제 신원·입구 매핑·인증 설정(CLI 계정이 가리키는 OS 사용자·Slack 워크스페이스/채널 ID·HTTP 발급자/대상/키 이름)과
/// 프로필별 모델 허용·모델 실행 방법(상류 목적지·자격 이름)·egress 상한·서버 설치 판본·VM 이미지와 망 이름. 프로필은 그대로여도 이것이
/// 바뀌면 활성화 사건이 되어, 옛 설정으로 뜬 프로세스(게이트웨이·워커)는 구판으로 닫히고 설정이 다른 프로세스는 `--activate` 없이
/// 합류하지 못한다.
pub struct Publication {
    pub revision: Revision,
    /// 프로필별 판본(스냅샷 판본). 접수·발송이 프로필마다 대조한다.
    pub profiles: BTreeMap<ProfileId, Revision>,
    /// 이 집합의 기억 시드 전체. 판본 교체와 한 트랜잭션으로 게시한다(어느 한 건이라도 거절되면 판본도 그대로다).
    pub seeds: Vec<MemorySeed>,
    /// 프로필별 정적 예약(cron 트리거) 정의. 판본 교체와 같은 커밋으로 게시한다 — 새 판본에서 사라진 트리거는 그 시각에 닫히고
    /// 전환 전 발생은 따라잡는다. 트리거가 없는 프로필은 넣지 않는다(없는 키는 빈 정의).
    pub schedules: BTreeMap<ProfileId, Vec<TriggerSpec>>,
}

/// 활성화 판본 계산의 버전. **게시하는 내용이 달라지면 올린다**(판본 계산이나 `replace`에 함께 올리는 것 — 예: 정적 예약 정의). 같은
/// 프로필 집합이라도 옛 코드가 게시한 판본과 달라져, 업그레이드 뒤 첫 `--activate`가 한 번 교체해 새 내용을 올린다 — 같은 판본으로
/// 보여 `Join`에 지나치면 새 게시 내용(예약 정의)이 저장소에 없는 채로 남는다.
/// v1: 예약 정의를 판본과 함께 올리지 않던 때. v2: 정적 예약 정의를 같은 커밋에 올린다.
const ACTIVATION_VERSION: &[u8] = b"bot-active-v2\0";

/// 프로필 집합 판본과 매핑 지문을 묶는다(각 조각 길이를 앞에 붙여 경계를 옮길 수 없다).
pub fn activation_revision(set: &Revision, mapping: &[u8; 32]) -> Revision {
    let mut hash = Sha256::new();
    hash.update(ACTIVATION_VERSION);
    hash.update((set.0.len() as u64).to_be_bytes());
    hash.update(set.0.as_bytes());
    hash.update(mapping);
    Revision(format!("{:x}", hash.finalize()))
}

impl Publication {
    /// `revisions`는 호스트 `servers`의 설치 판본(별칭 → 판본)이다.
    pub fn of(
        set: &ProfileSet,
        host: &HostFile,
        revisions: &BTreeMap<String, Revision>,
    ) -> Result<Self, String> {
        let authority = host.authority_fingerprint(revisions)?;
        Ok(Self {
            revision: activation_revision(set.revision(), &authority),
            profiles: set
                .active()
                .iter()
                .map(|(id, snapshot)| (id.clone(), snapshot.revision.clone()))
                .collect(),
            seeds: set.active().values().flat_map(|s| seed::seeds(s)).collect(),
            schedules: set
                .active()
                .iter()
                // 효력 시각은 저장소가 DB 시각 한 번으로 정한다 — 여기서 넘기는 시각은 쓰지 않는다
                .map(|(id, snapshot)| (id.clone(), schedule_set(snapshot).triggers))
                .filter(|(_, triggers)| !triggers.is_empty())
                .collect(),
        })
    }
}

/// 종료 신호를 토큰으로 바꾼다.
fn watch_signals(stop: CancellationToken) {
    tokio::spawn(async move {
        let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term {
                    Ok(mut term) => { term.recv().await; }
                    Err(_) => std::future::pending::<()>().await,
                }
            } => {}
        }
        stop.cancel();
    });
}

/// 시험이 넣는 대체: 모델 실행 조각(합성 샌드박스·가짜 상류)과 자격 보관소·종료 신호·가짜 Slack 주소. 운영 실행(`run`)은 모두 비어 있다 —
/// 제품 CLI에는 이것을 고르는 설정이 없고, 모델 CLI의 운영 조각은 호스트 설정(`sandbox.apple`)에서 만든 Apple VM 어댑터와 HTTPS 상류 전송뿐이다.
#[doc(hidden)]
#[derive(Default)]
pub struct Overrides {
    pub claude: Option<claude::ClaudeParts>,
    pub secrets: Option<Arc<dyn SecretStore>>,
    pub stop: Option<CancellationToken>,
    /// 가짜 Slack 서버 주소(시험 전용). 제품 `run`은 항상 `None`이라 실제 Slack으로 간다.
    pub slack_base: Option<String>,
    /// 운영 조각(호스트 설정에서 만든 Apple VM 어댑터)은 그대로 쓰고 상류 전송만 바꾼다(시험 전용: 실제 VM에 가짜 상류). 제품 `run`은 `None`이다.
    pub upstream: Option<Arc<dyn bot_broker::model::Upstream>>,
    /// 저장소 설정(시험 전용: 시도 한도 같은 값을 바꿔 본다). 제품 `run`은 `None`이라 기본값이다.
    pub store_config: Option<Config>,
}

pub async fn run(settings: &Settings, roles: &[Role], explicit: bool) -> Result<(), String> {
    run_with(settings, roles, explicit, Overrides::default()).await
}

#[doc(hidden)]
pub async fn run_with(
    settings: &Settings,
    roles: &[Role],
    explicit: bool,
    overrides: Overrides,
) -> Result<(), String> {
    let roles = Role::expand(roles);
    let blocked: Vec<String> = roles
        .iter()
        .filter_map(|role| not_assembled(*role).map(|why| format!("  {}: {why}", role.name())))
        .collect();
    if !blocked.is_empty() {
        return Err(format!(
            "시작할 수 없는 역할이 있어 아무것도 시작하지 않아요:\n{}",
            blocked.join("\n")
        ));
    }
    // 1) 검증: 프로필·호스트 등록부·입구 표
    let loaded = load(settings)?;
    check_entrances(loaded.set.active().clone(), &loaded.host)?;
    if loaded.host.demo {
        eprintln!(
            "BOT_HOST_CONFIG가 없어 내장 데모 호스트를 써요(CLI 계정 owner·team = 지금 사용자)"
        );
    }
    // 저장소가 받지 못할 시드가 있으면 아무것도 쓰기 전에 멈춘다(시드 거절이 활성 판본 교체 뒤에 드러나지 않게)
    let seed_problems: Vec<String> = loaded
        .set
        .active()
        .values()
        .flat_map(|snapshot| seed::problems(snapshot))
        .collect();
    if !seed_problems.is_empty() {
        return Err(seed_problems.join("\n"));
    }
    // 도구 서버: 승인 결정만 하는 역할은 설치 판본만 읽는다(복사 없음). 도구를 실행하는 역할(broker·worker)이 조립되면
    // 그쪽이 `install::install`로 읽기 전용 설치본을 만들어 같은 판본으로 실행한다.
    let revisions = crate::install::measure(&loaded.host.servers)?;
    // 자격 보관소와 관측: 모든 역할이 같은 수집기(Exporter)를 쓴다(단독 gateway도). 잘못된 수집기 설정이면 여기서 멈춘다.
    let secrets = match overrides.secrets {
        Some(secrets) => secrets,
        None => secret_store(settings)?,
    };
    let observe = Arc::new(trace::start(&loaded.host, &loaded.registry, secrets).await?);
    // 이후 모든 역할은 값을 가림 장치에 더해 주는 보관소를 쓴다(키를 교체해도 새 값이 쓰이기 전에 가려진다)
    let secrets = observe.secrets.clone();
    // 2) 저장소: 접속·마이그레이션, 그리고 활성 판본 결정(읽기만 — 거절이면 여기서 끝난다)
    let url = settings
        .database_url
        .as_deref()
        .ok_or("BOT_DATABASE_URL이 필요해요(PostgreSQL 접속 주소)")?;
    let store = Store::connect(url, overrides.store_config.clone().unwrap_or_default())
        .await
        .map_err(|e| format!("저장소에 접속하지 못했어요: {}", explain(e.into_core())))?;
    store.migrate().await.map_err(|e| {
        format!(
            "저장소 마이그레이션에 실패했어요: {}",
            explain(e.into_core())
        )
    })?;
    let publication = Publication::of(&loaded.set, &loaded.host, &revisions)?;
    let plan = plan_activation(&store, &publication, explicit).await?;
    // 판본 복원: 새 집합의 묶음을 먼저 보관하고(활성 집합이 보일 때 묶음이 이미 있어야 한다), 진행 중 작업이 고정한 옛 판본을 묶음에서 복원해 등록한다.
    // 역할을 열기 전·판본을 게시하기 전에 끝내므로 실패하면 활성 판본·실행·대기 카드는 그대로다. 복원한 판본은 `restored`가 이 함수가 끝날 때까지 쥔다(등록은 약한 참조).
    let mut restored = restore::prepare(
        &store,
        &loaded.registry,
        &loaded.set,
        &publication.profiles,
        &bot_profile::Limits::default(),
    )
    .await?;
    // 외부 요청을 받는 역할(gateway·admin)과 승인 결정 브로커는 이 활성화 판본에 묶은 저장소 핸들을 쓴다 —
    // 새 접수·승인·발송·관리 판정이 DB 커밋까지 같은 판본인지 확인받는다(교체와 같은 잠금). 판본 게시는 원래 핸들로 한다.
    let fenced = Arc::new(store.for_activation(publication.revision.clone()));
    let store = Arc::new(store);
    // 이 프로세스의 활성화 판본이 저장소의 활성 판본과 같은지 확인한다(구판 관리 응답·구판 워커를 닫는 근거)
    let active: Arc<dyn ActiveCheck> = Arc::new(ActiveSet::new(
        fenced.clone() as Arc<dyn ActiveRevisions>,
        publication.revision.clone(),
    ));
    // 수집(OTLP)을 거두거나 바꾸면(수집 설정은 활성화 지문에 든다) 옛 프로세스는 구판이 된다: 구판의 내보내기 문을 닫아, 큐에 남은 스팬까지 옛 허락(목적지·내용·
    // 식별자 표기)으로 더 내보내지 않는다. 저장소를 확인하지 못하는 동안도 닫힌 것으로 본다.
    observe.exporter.set_gate(
        {
            let active = active.clone();
            Arc::new(move || {
                let active = active.clone();
                Box::pin(async move { active.verify().await.is_ok() })
            })
        },
        std::time::Duration::from_secs(3),
    );
    // 3) 역할 준비: 자격·입구·리스너 점검을 모두 끝낸다. 아직 아무것도 열지 않았고 저장소의 운영 판본도 그대로다.
    // 명시 목적지(다른 수신자) 등록부: worker·결정 브로커·scheduler가 **같은** 활성 프로필과 호스트 등록부로 판정한다(생성·발화·발송이 각자 지금 값을 본다)
    let routes: Arc<dyn ScheduleRoutes> = Arc::new(HostRoutes::new(
        loaded.registry.clone() as Arc<dyn ProfileCatalog>,
        Arc::new(loaded.host.gateway.clone()),
    ));
    let decisions = decide::broker(
        loaded.registry.clone(),
        fenced.clone(),
        &revisions,
        observe.tracer(),
        routes.clone(),
    );
    let own_signals = overrides.stop.is_none();
    let stop = overrides.stop.unwrap_or_default();
    let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();
    let prepared_gateway = if roles.contains(&Role::Gateway) {
        let deps = gateway::Deps {
            settings: settings.clone(),
            host: loaded.host.clone(),
            registry: loaded.registry.clone(),
            store: fenced.clone(),
            secrets: secrets.clone(),
            uid: loaded.uid,
            stop: stop.clone(),
            set_revision: publication.revision.clone(),
            decisions: decisions.clone(),
            tracer: observe.tracer(),
            slack_base: overrides.slack_base.clone(),
        };
        Some(gateway::prepare(&deps).await?)
    } else {
        None
    };
    let prepared_admin = if roles.contains(&Role::Admin) {
        let deps = admin::Deps {
            host: loaded.host.clone(),
            registry: loaded.registry.clone(),
            store: fenced.clone(),
            observe: observe.clone(),
            uid: loaded.uid,
            stop: stop.clone(),
            decisions: decisions.clone(),
            token_file: settings.admin_token_file.clone(),
            terminal: std::io::stdout().is_terminal(),
            active: active.clone(),
        };
        Some(admin::prepare(&deps).await?)
    } else {
        None
    };
    let prepared_scheduler = roles.contains(&Role::Scheduler).then(|| {
        scheduler::prepare(&scheduler::Deps {
            registry: loaded.registry.clone(),
            // 활성화 판본에 묶은 핸들: 판본이 바뀐 뒤의 예약 접수는 DB 잠금 안에서 거절된다
            store: fenced.clone(),
            observe: observe.clone(),
            stop: stop.clone(),
            routes: routes.clone(),
        })
    });
    let prepared_worker = if roles.contains(&Role::Worker) {
        let deps = worker::Deps {
            host: loaded.host.clone(),
            settings: settings.clone(),
            registry: loaded.registry.clone(),
            // 활성화 판본에 묶은 핸들: 먼저 확인한 뒤 DB의 claim·도구 시작까지의 틈에 권한 설정이 바뀌어도 같은 DB 잠금 안에서 대조한다
            store: fenced.clone(),
            secrets: secrets.clone(),
            observe: observe.clone(),
            uid: loaded.uid,
            stop: stop.clone(),
            active: active.clone(),
            claude: overrides.claude.clone(),
            upstream: overrides.upstream.clone(),
            routes: routes.clone(),
        };
        Some(worker::prepare(&deps).await?)
    } else {
        None
    };
    // 입구를 열 때 필요한 쓰기 중 실패할 수 있는 것은 판본을 바꾸기 전에 끝낸다: 관리 비밀 파일을 발행한다(기존 파일은 옆에 보관).
    // 이후 확정이 실패해 `prepared_admin`이 버려지면 발행을 되돌려 기존 파일을 복구한다.
    let mut prepared_admin = prepared_admin;
    // 4) 확정: 활성 판본·기억 시드·정적 예약 정의를 한 트랜잭션으로 게시한다. 준비가 모두 끝난 뒤에만 쓴다(준비가 실패하면 운영 판본은
    //    바뀌지 않는다). 같은 판본에 합류할 때는 아무것도 쓰지 않는다. 여기서 실패하면 이미 띄운 모델 VM(닻)을 치우고 돌아간다.
    let committed = async {
        if let Some(admin) = prepared_admin.as_mut() {
            admin.publish()?;
        }
        commit_with_refresh(
            &store,
            &loaded.registry,
            &publication,
            &plan,
            &loaded.set,
            &mut restored,
        )
        .await
    }
    .await;
    let wrote = match committed {
        Ok(wrote) => wrote,
        Err(error) => {
            if let Some(prepared) = prepared_worker {
                prepared.discard().await;
            }
            return Err(error);
        }
    };
    if wrote {
        tracing::info!("활성 판본을 게시했어요");
    }
    if restored.restored() > 0 {
        tracing::info!(
            count = restored.restored(),
            "진행 중 작업이 쓰는 옛 판본을 복원했어요"
        );
    }
    // 5) 열기
    if let Some(prepared) = prepared_gateway {
        prepared.open(&mut tasks);
    }
    if let Some(prepared) = prepared_admin {
        prepared.open(&mut tasks);
    }
    if let Some(prepared) = prepared_scheduler {
        prepared.open(&mut tasks);
    }
    if let Some(prepared) = prepared_worker {
        prepared.open(&mut tasks);
    }
    if own_signals {
        watch_signals(stop.clone());
    }
    // 시작 뒤에 교체된 값도 가림 목록에 들어가게 선언된 비밀 이름 전체를 주기적으로 다시 읽는다. 역할을 나눠 띄우면 이 프로세스가 모델 키·도구 자격을
    // 읽을 일이 없어(관리 프로세스) 다른 프로세스가 쓰는 새 값을 스스로는 알 수 없다. 값은 가림에만 쓰고 쥐지도 기록하지도 않는다. 역할 작업이 아니라
    // 따로 띄워 끝에서 거둔다(이것이 남아 있다고 서버가 끝나지 않는 일이 없게).
    let rereader = settings.secrets_reread.map(|every| {
        tokio::spawn(watched::reread(
            secrets.clone(),
            watched::declared(&loaded.host, &loaded.registry),
            watched::Pace::new(every),
            stop.clone(),
        ))
    });
    let mut failure: Option<String> = None;
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            joined = tasks.join_next() => match joined {
                None => break,
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(error))) => { failure = Some(error); stop.cancel(); break; }
                Some(Err(_)) => { failure = Some("역할 작업이 비정상 종료했어요".into()); stop.cancel(); break; }
            }
        }
    }
    // 정리: 진행 중인 요청이 끝나도록 기다린다. worker는 유예 뒤 남은 run의 프로세스 종료·세션 회수를 확인하고 대기열로 돌려주는 정리까지 하므로
    // 이 기한이 그 정리를 중간에 끊지 않게 유예 + 정리 시간보다 길게 잡는다.
    let drain = async { while tasks.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(worker::SHUTDOWN_WAIT, drain).await;
    if let Some(rereader) = rereader {
        rereader.abort();
    }
    // 남은 추적 큐를 비운다(모든 역할이 같은 수집기를 쓴다)
    observe.flush().await;
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(text: &str) -> Revision {
        Revision(text.into())
    }

    #[test]
    fn the_activation_revision_binds_the_profile_set_and_the_mapping_fingerprint() {
        let set = rev("set-1");
        let original = activation_revision(&set, &[1; 32]);
        assert_eq!(original, activation_revision(&set, &[1; 32]), "결정적");
        assert_ne!(
            original,
            activation_revision(&set, &[2; 32]),
            "매핑이 다르면 다른 판본"
        );
        assert_ne!(
            original,
            activation_revision(&rev("set-2"), &[1; 32]),
            "프로필 집합이 다르면 다른 판본"
        );
        assert_eq!(original.0.len(), 64);
        // 조각 경계를 옮겨도 같은 판본이 되지 않는다
        assert_ne!(
            activation_revision(&rev("a"), &[0; 32]),
            activation_revision(&rev("a\0"), &[0; 32])
        );
    }
}
