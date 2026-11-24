//! 모델 실행과 MCP 도구 실행이 공유하는 OS 샌드박스 경계.
use crate::{id::Attempt, port::Reply};
use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

/// 신뢰 구역에 맞는 어댑터가 구현한다. OS 격리를 설정할 수 없으면 spawn은 실패한다.
pub trait Sandbox<Spec>: Send + Sync {
    type Process: Send;

    /// 프로세스를 띄운다. 이 future를 버리면(호출자가 취소했다) 시작을 취소한 것이고 아무것도 남기지 않아야 한다: 이미 만든 것이 있다면 반환하기 전에
    /// 그 자리에서 거두거나, 만든 뒤에는 기다리지 않고 곧바로 `Process`로 돌려준다(그러면 호출자가 `kill`한다).
    fn spawn(&self, spec: Spec) -> Reply<'_, Self::Process>;
    /// 지정한 소유자의 자손 프로세스까지 종료를 확인해야 성공한다.
    fn reap(&self, owner: &str) -> Reply<'_, ()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mount {
    pub host: PathBuf,
    pub guest: String,
}

/// 호스트가 조립한 모델 CLI 요청. 어댑터는 현재 claim의 전용 브로커 IPC만 연결한다.
/// 입력은 읽기 전용이고 scratch만 쓰기 가능하다. 경로 매핑을 보장할 수 없으면 실행을 거절한다.
#[derive(Clone, Eq, PartialEq)]
pub struct ModelSpec {
    pub attempt: Attempt,
    pub program: String,
    pub args: Vec<String>,
    /// 비밀 없는 CLI 설정. 호스트 환경은 상속하지 않는다.
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub readonly_mounts: Vec<Mount>,
    pub scratch: Option<Mount>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    pub idle_timeout: Duration,
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct Exit {
    pub code: Option<i32>,
    /// 호스트 내부 오류 분류에만 사용하는 길이 제한 진단이다.
    pub stderr_tail: String,
}

pub type LineFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
pub type UnitFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
pub type ExitFuture<'a> = Pin<Box<dyn Future<Output = Exit> + Send + 'a>>;

pub trait ModelProcess: Send {
    fn next_line(&mut self) -> LineFuture<'_>;
    fn kill(&mut self) -> UnitFuture<'_>;
    fn wait(&mut self) -> ExitFuture<'_>;
}

/// 워커에 주입하는 모델용 포트. 도구 명세나 외부 접속 설정을 받지 않는다.
pub type ModelSandbox = dyn Sandbox<ModelSpec, Process = Box<dyn ModelProcess>>;
