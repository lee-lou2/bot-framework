//! Slack 어댑터. 접수 순서는 다른 입구와 같다: 인증된 설치 확인 → 정규화 → `Ingress::admit` → ack.
//!
//! - 소켓 하나는 **인증된 설치 하나** — (프로필, 워크스페이스 별칭)의 자격으로 연다. 그 소켓으로 온 이벤트는 같은 프로필의
//!   바인딩으로만 라우팅하고, 소켓 밖 프로필로 폴백하지 않는다. 같은 앱 토큰을 프로필 둘이 쓰면 이벤트가 나뉘어 한쪽이
//!   놓치므로 기동을 거절한다(토큰 값이 같다는 이유로 권한을 합치지 않는다).
//! - 회신은 outbox에서(`delivery::Deliverer` + `send::SlackSender`), 승인 버튼은 클릭한 Slack 사용자를 주체로
//!   `Broker::decide`에 넘기고 원장 상태로 카드를 갱신한다.
//! - 채널에서 시작한 예약의 승인 카드는 소유자의 1:1 DM으로 올린다(`cards::CardDeliverer`). 그 카드의 버튼은 영수증(DM 채널·메시지)과
//!   대조해 이 카드의 것일 때만 결정한다.
pub mod api;
pub mod blocks;
pub mod cards;
pub mod creds;
mod events;
pub mod frame;
mod handle;
mod human;
pub mod send;
pub mod socket;
pub mod ux;

use crate::ingress::Ingress;
use api::SlackApi;
use bot_core::{
    id::ProfileId,
    ledger::CallLedger,
    port::{Broker, RunAdmin, RunStore, SecretStore},
    slack::BotThreads,
};
use creds::{BotCheck, Credentials};
use socket::Installation;
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const RECONCILE_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Eq, PartialEq)]
pub struct SlackError(pub String);

impl fmt::Display for SlackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SlackError {}

pub struct SlackGateway {
    ingress: Arc<Ingress>,
    broker: Arc<dyn Broker>,
    ledger: Arc<dyn CallLedger>,
    secrets: Arc<dyn SecretStore>,
    creds: Arc<Credentials>,
    api: SlackApi,
    /// 봇이 참여한 스레드의 증거 저장소(없으면 `thread_reply`는 증거를 확인할 수 없어 어떤 답글도 받지 않는다)
    threads: Option<Arc<dyn BotThreads>>,
    /// 진행 상태(👀/✅ · `chat.startStream`)와 스레드 중단 제어기(생략하면 장식 호출 없음)
    ux: Option<Arc<ux::SlackUx>>,
}

impl SlackGateway {
    pub fn new(
        ingress: Arc<Ingress>,
        broker: Arc<dyn Broker>,
        ledger: Arc<dyn CallLedger>,
        secrets: Arc<dyn SecretStore>,
        api: SlackApi,
    ) -> Self {
        let creds = Arc::new(Credentials::new(
            secrets.clone(),
            ingress.host().clone(),
            api.clone(),
        ));
        Self {
            ingress,
            broker,
            ledger,
            secrets,
            creds,
            api,
            threads: None,
            ux: None,
        }
    }

    /// 스레드 증거 저장소를 붙인다(F2 `thread_reply`): 회신 발송 영수증과 인증된 자기 봇 글을 기록하고, 답글은 이 증거가 있는 스레드에서만 받는다.
    #[must_use]
    pub fn with_threads(mut self, threads: Arc<dyn BotThreads>) -> Self {
        self.threads = Some(threads);
        self
    }

    /// Slack 진행 상태(👀/✅ · 스트림 상태 카드)와 스레드 중단(버튼·"중단해줘") 제어기를 붙인다.
    #[must_use]
    pub fn with_ux(mut self, runs: Arc<dyn RunStore>, admin: Arc<dyn RunAdmin>) -> Self {
        self.ux = Some(Arc::new(ux::SlackUx::new(self.creds.clone(), runs, admin)));
        self
    }

    /// 시험 전용: 봇 토큰을 보관소에서 다시 읽는 간격을 줄인다(교체한 토큰이 다음 발송에 확인되는지 보려고).
    #[doc(hidden)]
    #[must_use]
    pub fn with_token_ttl(mut self, ttl: Duration) -> Self {
        self.creds = Arc::new(
            Credentials::new(
                self.secrets.clone(),
                self.ingress.host().clone(),
                self.api.clone(),
            )
            .with_ttl(ttl),
        );
        self
    }

    /// 회신 발송기(같은 자격·같은 API 클라이언트를 쓴다)
    pub fn sender(&self) -> send::SlackSender {
        let sender = send::SlackSender::new(self.api.clone(), self.creds.clone());
        let sender = match &self.threads {
            Some(threads) => sender.with_threads(threads.clone()),
            None => sender,
        };
        match &self.ux {
            Some(ux) => sender.with_ux(ux.clone()),
            None => sender,
        }
    }

    /// 소켓으로 받을 설치 목록을 만들고 모두 검사한다: 자격을 읽을 수 있는지, 토큰이 설정한 워크스페이스의 것인지,
    /// 같은 앱 토큰을 프로필 둘이 쓰지 않는지. 하나라도 어긋나면 Err(값은 싣지 않는다).
    pub async fn plan(&self) -> Result<Vec<Installation>, SlackError> {
        let routes = self
            .ingress
            .routes()
            .map_err(|error| SlackError(error.to_string()))?;
        let host = self.ingress.host().clone();
        let mut wanted: BTreeMap<(ProfileId, String), ()> = BTreeMap::new();
        for snapshot in routes.profiles() {
            for binding in snapshot.bindings.values() {
                use bot_core::profile::Adapter;
                if let Adapter::SlackChannel { workspace, .. } | Adapter::SlackDm { workspace } =
                    &binding.source
                {
                    wanted.insert((snapshot.id.clone(), workspace.clone()), ());
                }
            }
        }
        let mut installs = Vec::new();
        let mut tokens: HashMap<(String, String), ProfileId> = HashMap::new();
        for (profile, alias) in wanted.into_keys() {
            let who = format!("프로필 {} · 워크스페이스 {alias}", profile.0);
            let Some(ws) = host.slack.workspaces.get(&alias) else {
                return Err(SlackError(format!("{who}: 호스트에 등록되지 않았어요")));
            };
            // 소켓이 아닌 수신 방식은 이 설치를 받아 줄 곳이 없다: 조용히 건너뛰면 메시지·승인 버튼이 사라지므로 시작을 거절한다(설정 검증이 먼저 거른다)
            if ws.receive != crate::host::Receive::Socket {
                return Err(SlackError(format!(
                    "{who}: receive=http는 아직 지원하지 않아요 — socket(app_token)으로만 받을 수 있어요"
                )));
            }
            let Some(app) = self.creds.app_token(&profile, &alias).await else {
                return Err(SlackError(format!(
                    "{who}: 봇·앱 토큰을 읽지 못했어요(자격 이름만 확인하세요)"
                )));
            };
            if let Some(other) =
                tokens.insert((alias.clone(), app.as_str().to_owned()), profile.clone())
            {
                return Err(SlackError(format!(
                    "{who}: 프로필 {} 과 같은 앱 토큰을 써요 — 이벤트가 나뉘어 한쪽이 놓치니 프로필마다 별도 Slack 앱을 쓰세요",
                    other.0
                )));
            }
            // 봇 토큰은 발송·카드 갱신이 쓰는 같은 확인 길(`check_bot`)로 읽고 등록한 팀의 것인지 확인한다
            let auth = match self.creds.check_bot(&profile, &alias).await {
                BotCheck::Verified(_, auth) => auth,
                BotCheck::Unreadable => {
                    return Err(SlackError(format!(
                        "{who}: 봇·앱 토큰을 읽지 못했어요(자격 이름만 확인하세요)"
                    )));
                }
                BotCheck::Unverified => {
                    return Err(SlackError(format!(
                        "{who}: 봇 토큰으로 Slack에 인증하지 못했어요"
                    )));
                }
                BotCheck::WrongTeam => {
                    return Err(SlackError(format!(
                        "{who}: 토큰이 설정한 워크스페이스의 것이 아니에요"
                    )));
                }
            };
            installs.push(Installation {
                profile,
                alias,
                team: ws.team_id.clone(),
                app_id: ws.app_id.clone(),
                bot_user: auth.user_id,
            });
        }
        Ok(installs)
    }

    /// 설치마다 소켓을 열어 stop까지 받는다. 처음 계획이 검사를 통과하지 못하면 아무것도 열지 않고 Err.
    /// 활성 프로필 집합이 바뀌면(핫 리로드) 새 설치는 열고 없어진 설치는 닫는다. 새 집합의 계획이 틀리면 이전 설치를 그대로 둔다.
    pub async fn serve(self: Arc<Self>, stop: CancellationToken) -> Result<(), SlackError> {
        let desired = self.plan().await?;
        let mut seen = self.ingress.routes().ok();
        let mut running: HashMap<(ProfileId, String), Running> = HashMap::new();
        self.reconcile(&mut running, desired, &stop);
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = tokio::time::sleep(RECONCILE_EVERY) => {}
            }
            let current = self.ingress.routes().ok();
            let unchanged = match (&seen, &current) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if unchanged {
                continue;
            }
            seen = current;
            match self.plan().await {
                Ok(next) => self.reconcile(&mut running, next, &stop),
                Err(error) => {
                    tracing::error!(%error, "바뀐 프로필 집합의 Slack 설치를 만들지 못했어요 — 이전 설치를 계속 써요");
                }
            }
        }
        for running in running.into_values() {
            running.stop.cancel();
            let _ = running.task.await;
        }
        Ok(())
    }

    fn reconcile(
        self: &Arc<Self>,
        running: &mut HashMap<(ProfileId, String), Running>,
        desired: Vec<Installation>,
        parent: &CancellationToken,
    ) {
        let wanted: HashMap<(ProfileId, String), Installation> = desired
            .into_iter()
            .map(|install| ((install.profile.clone(), install.alias.clone()), install))
            .collect();
        // 없어졌거나 바뀐 설치는 닫는다(끝난 소켓은 치운다)
        running.retain(|key, run| {
            let keep = wanted.get(key) == Some(&run.install);
            if !keep {
                run.stop.cancel();
            }
            keep
        });
        for (key, install) in wanted {
            running
                .entry(key)
                .or_insert_with(|| self.spawn_socket(install, parent));
        }
    }

    fn spawn_socket(
        self: &Arc<Self>,
        install: Installation,
        parent: &CancellationToken,
    ) -> Running {
        let stop = parent.child_token();
        let handler: socket::Handler = {
            let (gateway, install) = (self.clone(), install.clone());
            Arc::new(move |frame| {
                let (gateway, install) = (gateway.clone(), install.clone());
                Box::pin(async move { gateway.on_frame(&install, frame).await })
            })
        };
        let task = tokio::spawn({
            let (api, creds, install, stop) = (
                self.api.clone(),
                self.creds.clone(),
                install.clone(),
                stop.clone(),
            );
            async move {
                if let Err(socket::Fatal(reason)) =
                    socket::run(api, creds, install, handler, stop).await
                {
                    // 다시 시도해도 소용없는 오류다. 이 설치는 닫아 둔 채로 설정을 고칠 때까지 둔다.
                    tracing::error!(%reason, "slack 소켓을 멈췄어요");
                }
            }
        });
        Running {
            install,
            stop,
            task,
        }
    }
}

struct Running {
    install: Installation,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
