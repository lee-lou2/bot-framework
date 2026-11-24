//! 브로커 내부 포트. 저장소·프로필·감사·효과 포트는 bot-core(`ledger`·`profile`·`port`)를 쓴다.

use crate::resolve::Resolved;
use bot_core::{
    command::JsonPayload,
    id::*,
    port::{Error, Reply},
    profile::Tool,
    state::{Conversation, RunOrigin},
};

/// 인자를 검증하고 정규화한 결과. `canonical`의 바이트가 곧 해시한 바이트이며 도구가 받는 바이트다.
#[derive(Clone, Debug)]
pub struct Sealed {
    pub canonical: String,
    pub sha256: [u8; 32],
    /// 명시적 목적지가 가리키는 (수신자, 내용). 결정할 수 없으면 `seal`이 거절한다.
    pub destination: Option<(String, String)>,
}

pub trait Sealer: Send + Sync {
    /// 입력 스키마 검증 → 목적지 확정 → RFC 8785 정규화 → SHA-256. 실패는 `Invalid`.
    fn seal(&self, tool: &Tool, args: &JsonPayload) -> Result<Sealed, Error>;
}

/// 승인까지 끝난 호출 한 건.
#[derive(Clone, Debug)]
pub struct ToolJob {
    pub attempt: Attempt,
    /// 접수 때 인증한 주체. 위임·승인·재개에도 같은 주체로 데이터 범위를 정한다.
    pub principal: PrincipalId,
    /// 루트 접수 원장이 정한 진행 출처(`_meta["bot/trigger"]`). 위임·재개에도 보존되며 모델 인자·요청 ID·본문으로 추정하지 않는다.
    pub origin: RunOrigin,
    pub call: CallId,
    pub tool: Resolved,
    /// 정규화한 인자(해시한 바이트 그대로).
    pub args: String,
    /// 도구가 고른(`Tool.env`) 비밀 아닌 환경 변수의 이름·값. 호출하는 에이전트의 환경에서 고정 판본과 현재 판본이 같은 값을 줄 때만
    /// 채워진다(`resolve::selected_env`). 호스트 환경(`process.env`)으로는 채우지 않는다.
    pub env: Vec<(String, String)>,
    /// `Destination::Reply`일 때 고정된 원래 대화.
    pub reply_to: Option<Conversation>,
    /// 원장에 기록한 샌드박스 소유 표지.
    pub owner: String,
}

#[derive(Clone, Debug)]
pub enum Ran {
    Done(JsonPayload),
    /// `tools/call`을 보내기 전에 실패(스폰·핸드셰이크·선언 불일치). 미실행이 입증된다.
    NotSent,
    /// 서버가 호출을 실행하기 전에 확정적으로 거절했다(JSON-RPC 잘못된 요청·없는 메서드·잘못된 인자). 외부 효과는 없다.
    Rejected,
    /// 보냈으나 결과를 모른다(시간 초과·프로세스 종료·연결 끊김·서버 내부 오류).
    Unknown(&'static str),
    /// 실행은 끝났지만 결과를 막았다(자격 노출·형식·크기).
    Blocked(&'static str),
}

pub trait ToolRunner: Send + Sync {
    /// 호스트가 이 서버 별칭으로 설치한 실행 파일·이미지의 고정 판본. 설치되지 않았으면 `None`.
    /// 승인에 묶이므로 승인 뒤 설치가 바뀌면 종전 승인은 무효다.
    fn installed(&self, server: &str) -> Option<String>;
    /// 이 호스트의 샌드박스가 설치한 서버를 실행할 수 있는가. 호출을 기록하거나 자격을 풀기 전에 본다. 호출 간 상태 볼륨을 요구하는
    /// 서버(`state: required`)를 지원하지 않으면 `Denied`다. 기본은 모두 받는다.
    fn admit(&self, _server: &str) -> Result<(), Error> {
        Ok(())
    }
    /// 도구가 받을 자격(`Tool.secrets`의 이름들)이 지금 이 프로필의 보관소에 있는가. 새 호출을 기록하거나 승인 카드를 만들기 **전에** 본다
    /// (실행할 수 없는 호출로 사람에게 승인을 받지 않게). 보관소에 그 이름이 없으면 `Missing`이고, 읽지 못하면 다른 오류다(지금만일 수 있어
    /// «없음»이 아니다). 값은 있는지만 보고 쓰거나 남기지 않는다. 기본은 확인하지 않는다.
    fn admit_secrets(&self, _profile: &ProfileId, _names: &[SecretName]) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn run(&self, job: ToolJob) -> Reply<'_, Ran>;
    /// 이전 시도가 남긴 프로세스(자식 포함)가 모두 끝났음을 확인한다. 확인하지 못하면 `Unavailable`.
    fn reap(&self, owner: String) -> Reply<'_, ()>;
}
