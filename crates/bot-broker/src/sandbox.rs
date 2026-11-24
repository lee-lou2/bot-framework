//! 도구 서버를 가두는 샌드박스 포트. 제품(컨테이너·OS 샌드박스)은 호스트가 고르고 어댑터가 이 포트를 구현한다.
//!
//! 어댑터가 지켜야 할 것:
//! - 환경은 `spec.env`가 전부다. 호스트 환경을 상속하지 않고 홈·도구 파일·호스트 소켓·DB를 마운트하지 않는다.
//! - 네트워크는 `spec.egress` 하나뿐이다(없으면 0). 직접 인터넷·DNS·로컬 네트워크 우회 경로를 열지 않는다.
//! - `Handle::terminate`는 프로세스 그룹 전체(자식 포함)가 끝난 것을 확인한 뒤에야 성공한다.
//! - `reap(owner)`는 이전 시도가 남긴 같은 소유 표지의 프로세스(자식 포함)를 끝내고 확인한다.
//!   호스트 단절 등으로 확인하지 못하면 `Unavailable`이다. 이 경우 브로커는 아무것도 다시 실행하지 않는다.
//! - 샌드박스를 설정하지 못하면 `spawn`은 실패한다(격리 없이 실행하지 않는다).

use bot_core::port::{Error, Reply};
use std::{net::SocketAddr, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite};

pub type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

pub struct Pipe {
    pub read: BoxRead,
    pub write: BoxWrite,
}

/// 도구가 닿을 수 있는 유일한 외부 경로(브로커가 호출마다 여는 `CONNECT` 프록시).
/// `url`은 호출 전용 자격을 포함한 주소(`http://user:pw@127.0.0.1:포트`)다. `env`의 프록시 변수와 같은 값이며,
/// 도구에 환경변수를 주지 않고 어댑터가 상위 프록시로 직접 쓰는 샌드박스(예: srt)를 위해 따로 싣는다.
#[derive(Clone, Eq, PartialEq)]
pub struct ProxyEndpoint {
    pub addr: SocketAddr,
    pub url: String,
}

/// 진단 출력에 호출 전용 자격(`url`)을 싣지 않는다.
impl std::fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyEndpoint")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// 샌드박스 전체 수명 상한. 지나면 어댑터가 강제로 끝낸다.
    pub wall: Duration,
}

#[derive(Clone)]
pub struct SandboxSpec {
    /// 원장에 기록한 소유 표지. 종료 확인과 감사에 쓴다.
    pub owner: String,
    /// 운영자가 설치한 서버 별칭과 그 고정 판본(승인에 묶인 값).
    pub server: String,
    pub image: String,
    /// 환경변수(선언한 비밀, 도구가 고른 비밀 아닌 설정, 프록시 변수).
    pub env: Vec<(String, String)>,
    /// 호스트가 정한 비밀 아닌 변수(`BOT_EFFECT_URL`). 어댑터는 허용한 이름만 받는다.
    pub host_env: Vec<(String, String)>,
    pub egress: Option<ProxyEndpoint>,
    /// 허용한 `호스트:포트` 목록(선언 ∩ 운영자 상한). 어댑터의 자체 허용 목록에도 같은 값을 쓴다(이중 집행).
    pub egress_hosts: Vec<String>,
    /// 호출 전용 양방향 채널(단계 원장 콜백)이 필요한가.
    pub effects: bool,
    pub limits: Limits,
}

/// 진단 출력에 환경 값(선언한 비밀)을 싣지 않는다. 키 이름만 보인다.
impl std::fmt::Debug for SandboxSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("SandboxSpec")
            .field("owner", &self.owner)
            .field("server", &self.server)
            .field("image", &self.image)
            .field("env", &names)
            .field(
                "host_env",
                &self
                    .host_env
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("egress", &self.egress)
            .field("egress_hosts", &self.egress_hosts)
            .field("effects", &self.effects)
            .field("limits", &self.limits)
            .finish()
    }
}

pub struct Process {
    /// MCP stdio. 서버의 stdin으로 쓰고 stdout에서 읽는다. stderr는 어댑터가 제한·가림 처리해 진단에만 쓴다.
    pub stdio: Pipe,
    /// `spec.effects`일 때만 있다.
    pub effects: Option<Pipe>,
    pub handle: Box<dyn Handle>,
}

pub trait Handle: Send {
    fn terminate(self: Box<Self>) -> Reply<'static, ()>;
}

pub trait Sandbox: Send + Sync {
    fn spawn(&self, spec: SandboxSpec) -> Reply<'_, Process>;
    fn reap(&self, owner: &str) -> Reply<'_, ()>;
    /// 표준 입출력 밖의 통로(FD)를 줄 수 없는 샌드박스는 단계 원장 콜백을 브로커 프록시의 전용 주소로 받는다.
    fn effects_via_proxy(&self) -> bool {
        false
    }
    /// 이 샌드박스가 설치한 서버(`server`, 승인에 묶인 판본 `image`)를 실행할 수 있는가. 자격을 풀기 전에 미리 묻는다.
    /// 호출 간 상태 볼륨을 요구하는 서버(`state: required`)를 지원하지 않으면 `Denied`다. 기본은 모두 받는다.
    fn admit(&self, _server: &str, _image: &str) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_carries_environment_values_or_proxy_credentials() {
        let spec = SandboxSpec {
            owner: "r1.1.c1".into(),
            server: "demo".into(),
            image: "img1".into(),
            env: vec![("TOKEN".into(), "sekret-value-1".into())],
            host_env: vec![],
            egress: Some(ProxyEndpoint {
                addr: "127.0.0.1:9".parse().unwrap(),
                url: "http://bot:pw-123@127.0.0.1:9".into(),
            }),
            egress_hosts: vec![],
            effects: false,
            limits: Limits {
                wall: Duration::from_secs(1),
            },
        };
        let shown = format!("{spec:?}");
        assert!(
            shown.contains("TOKEN")
                && !shown.contains("sekret-value-1")
                && !shown.contains("pw-123"),
            "{shown}"
        );
    }
}
