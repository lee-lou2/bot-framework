//! admin 역할: 관리 화면·API(조회·보류 판정·승인 결정)와 OTLP 추적 내보내기. docs/admin.md «조립(bot-cli)»을 따른다.
//!
//! 운영자 비밀은 시작할 때 무작위로 만들어 메모리에만 두고, **터미널(대화형 사용자)과 운영자가 지정한 0600 파일로만** 전달한다.
//! 파이프·로그로 흘러가는 표준출력에는 싣지 않으며, 터미널도 파일도 없으면 시작하지 않는다(받을 사람이 없는 화면을 열지 않는다).
//! 운영자 신원은 이 프로세스를 실행한 OS 사용자에 연결된 CLI 계정의 principal(`local:<계정>`)이다.
//! 승인 판정은 설치 판본을 아는 브로커(`decide`)에 맡기고, 설치 판본을 알 수 없는 호스트에서는 `unavailable`로 답한다(카드 상태는 그대로다).
use super::{token_file::TokenFile, trace::Observe};
use crate::host::HostFile;
use bot_admin::{
    AdminDeps, StaticTokens,
    auth::{new_token, viewer},
    port::{AdminAct, AdminRead},
};
use bot_core::{port::Broker, profile::ProfileCatalog};
use bot_gateway::{active::ActiveCheck, host::resolve_uid, identity};
use bot_profile::Registry;
use bot_store::Store;
use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub struct Deps {
    pub host: Arc<HostFile>,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    /// 이 프로세스의 모든 역할이 함께 쓰는 관측 입력(수집기·알려진 비밀 값)
    pub observe: Arc<Observe>,
    /// 이 프로세스를 실행한 OS 사용자
    pub uid: u32,
    pub stop: CancellationToken,
    /// 승인 판정을 맡길 브로커(설치 판본을 알면 실제 브로커, 모르면 결정을 맡기지 않는 것)
    pub decisions: Arc<dyn Broker>,
    /// 운영자 비밀을 받을 0600 파일(`BOT_ADMIN_TOKEN_FILE`). 터미널이 없는 기동의 전달 수단이다.
    pub token_file: Option<PathBuf>,
    /// 표준출력이 터미널인가
    pub terminal: bool,
    /// 이 프로세스의 활성화 판본이 저장소의 활성 판본과 같은지 확인한다(구판 관리 응답을 닫는 훅의 근거)
    pub active: Arc<dyn ActiveCheck>,
}

type Serving = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>;

/// 운영자 비밀을 받는 곳
#[derive(Debug, Eq, PartialEq)]
pub struct Delivery {
    pub terminal: bool,
    pub file: Option<PathBuf>,
}

/// 운영자 비밀의 전달 수단을 정한다. 터미널과 운영자가 지정한 절대 경로 파일만 받는다. 둘 다 없으면 Err.
pub fn delivery(terminal: bool, file: Option<&Path>) -> Result<Delivery, String> {
    if let Some(path) = file
        && !path.is_absolute()
    {
        return Err("BOT_ADMIN_TOKEN_FILE은 절대 경로여야 해요".to_owned());
    }
    if !terminal && file.is_none() {
        return Err(
            "관리 화면의 운영자 비밀을 전달할 곳이 없어요 — 터미널에서 실행하거나 BOT_ADMIN_TOKEN_FILE(0600 파일의 절대 경로)을 지정하세요"
                .to_owned(),
        );
    }
    Ok(Delivery {
        terminal,
        file: file.map(Path::to_owned),
    })
}

/// 점검을 마치고 주소를 확보한 관리 화면. 서버 작업은 만들어 두기만 했고(`open`이 띄운다) 그 전에는 요청을 받지 않는다.
pub struct Prepared {
    serving: Serving,
    addr: SocketAddr,
    /// 메모리에만 둔다. 터미널이면 보여 주고(`terminal`), 파일 전달이면 `open`이 발행한다. 표준출력(파이프)에는 싣지 않는다.
    token: String,
    terminal: bool,
    /// 독점만 해 둔 비밀 파일(발행은 기동이 확정된 뒤 `open`에서 한다)
    token_file: Option<TokenFile>,
}

/// 운영자 principal: 호스트가 이 OS 사용자에 연결한 CLI 계정(여럿이면 이름순 첫 번째).
fn operator_principal(host: &HostFile, uid: u32) -> Result<String, String> {
    host.gateway
        .cli
        .accounts
        .iter()
        .find(|(_, account)| resolve_uid(account).is_ok_and(|resolved| resolved == uid))
        .and_then(|(alias, _)| identity::local(alias))
        .map(|principal| principal.0)
        .ok_or_else(|| {
            format!(
                "관리 화면의 운영자 신원이 없어요 — 호스트 설정의 gateway.cli.accounts에 이 OS 사용자(uid {uid})의 계정을 등록하세요"
            )
        })
}

/// 점검하고 주소를 확보한다. 하나라도 실패하면 Err — 아직 아무것도 열리지 않았다(만든 비밀 파일도 지워진다).
pub async fn prepare(deps: &Deps) -> Result<Prepared, String> {
    let settings = deps.host.admin.clone();
    let principal = operator_principal(&deps.host, deps.uid)?;
    let delivery = delivery(deps.terminal, deps.token_file.as_deref())?;
    let token = new_token();
    // 경로 독점만 먼저 한다(이미 있는 파일은 기동이 확정될 때까지, 실패하면 끝까지 그대로다)
    let token_file = match &delivery.file {
        Some(path) => Some(TokenFile::claim(path, deps.uid)?),
        None => None,
    };
    let auth = StaticTokens::new().with(&token, viewer(&principal, true));
    let listener = bot_admin::bind(settings.bind, settings.remote)
        .await
        .map_err(|e| format!("관리 화면을 열지 못했어요({}): {e}", settings.bind))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("관리 화면 주소를 읽지 못했어요: {e}"))?;
    // 응답을 내보내기 직전에 같은 집합인지 확인한다: 다르거나 읽지 못하면 본문 없이 503(옛 매핑으로 지도·카드를 내주지 않는다)
    let active = deps.active.clone();
    let config = settings
        .config(addr)
        .traces(deps.observe.exporter.clone())
        .leak_redactor(deps.observe.redactor.clone())
        .fence(move || {
            let active = active.clone();
            Box::pin(async move { active.verify().await.is_ok() })
        });
    let app = bot_admin::router(
        config,
        AdminDeps {
            read: deps.store.clone() as Arc<dyn AdminRead>,
            act: deps.store.clone() as Arc<dyn AdminAct>,
            broker: deps.decisions.clone(),
            auth: Arc::new(auth),
            profiles: deps.registry.clone() as Arc<dyn ProfileCatalog>,
        },
    )
    .ok_or("관리 화면이 받아 줄 신원이 없어요")?;
    let stop = deps.stop.clone();
    Ok(Prepared {
        serving: Box::pin(bot_admin::serve(listener, app, async move {
            stop.cancelled_owned().await
        })),
        addr,
        token,
        terminal: delivery.terminal,
        token_file,
    })
}

impl Prepared {
    /// 비밀 파일을 발행한다(파일로 받는 경우). 활성 판본을 확정하기 **전에** 부른다 — 파일을 쓰지 못하면 Err라 판본을 바꾸기 전에
    /// 멈추고, 이후 확정이 실패해 이 값이 버려지면 발행을 되돌려 기존 파일을 복구한다.
    pub fn publish(&mut self) -> Result<(), String> {
        match self.token_file.as_mut() {
            Some(file) => file.publish(&self.token),
            None => Ok(()),
        }
    }

    /// 입구를 연다. 활성 판본·시드 확정까지 성공한 뒤에만 부르며 실패할 일이 없다. 발행한 파일을 확정하고(보관한 기존 파일을 버린다),
    /// 주소를 알리고, 운영자 비밀은 터미널에만 보여 준다(파일로 받은 경우에는 파일 경로만 알린다).
    pub fn open(self, tasks: &mut JoinSet<Result<(), String>>) {
        let Self {
            serving,
            addr,
            token,
            terminal,
            mut token_file,
        } = self;
        if let Some(file) = token_file.as_mut() {
            file.finalize();
        }
        println!("관리 화면: http://{addr}/ui");
        if terminal {
            println!("사용자 이름은 아무거나, 비밀번호는 {token}(이 터미널에만 보여요)");
        }
        if let Some(file) = &token_file {
            println!(
                "운영자 비밀은 {} 파일(0600)에 있어요",
                file.path().display()
            );
        }
        drop(token);
        tasks.spawn(async move {
            let served = serving
                .await
                .map_err(|e| format!("관리 화면이 멈췄어요: {e}"));
            // 종료하면 자기가 발행한 비밀 파일을 지우고 잠금을 푼다
            drop(token_file);
            served
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operator_secret_goes_only_to_a_terminal_or_a_designated_file() {
        let file = Path::new("/run/bot/admin-token");
        assert_eq!(
            delivery(true, None).unwrap(),
            Delivery {
                terminal: true,
                file: None
            }
        );
        assert_eq!(
            delivery(false, Some(file)).unwrap(),
            Delivery {
                terminal: false,
                file: Some(file.to_owned())
            }
        );
        assert!(delivery(true, Some(file)).unwrap().terminal);
        // 터미널도 파일도 없으면 시작하지 않는다(파이프 표준출력으로는 보내지 않는다)
        let error = delivery(false, None).unwrap_err();
        assert!(error.contains("BOT_ADMIN_TOKEN_FILE"), "{error}");
        assert!(delivery(false, Some(Path::new("relative/token"))).is_err());
    }
}
