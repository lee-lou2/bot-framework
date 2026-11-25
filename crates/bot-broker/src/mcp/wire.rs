//! 원격 MCP 서버로 나가는 전송 포트. HTTP 한 건을 보내고 응답 머리와 본문 조각을 돌려준다. TLS·이름 풀이·연결은 구현(조립하는 곳)이 맡고 아래를 **반드시** 지킨다.
//!
//! - **연결마다** 이름 풀이 결과 전체를 검사하고(`egress::vet_resolved`) 검사한 주소로만 연결한다. 한 연결을 다음 요청에 다시 쓰지 않는다.
//!   IP 리터럴 호스트도 같은 검사를 받는다. 예외는 `tls == false`인 **루프백 리터럴** 하나뿐이다(`allow_private`로 넓히지 않는다).
//! - 리다이렉트를 따라가지 않는다(3xx는 상태 그대로 돌려준다). 재시도·재전송·프록시 사용·압축 해제를 하지 않는다.
//! - 비밀인 머리 값(`Authorization`)은 어떤 로그·오류에도 싣지 않는다.
//! - 연결을 맺기 전(이름 풀이·주소 검사·TCP·TLS)에 실패하면 `Unreached`다. 요청을 쓰기 시작한 뒤의 실패는 모두 `Lost`다(서버가 받았는지 모른다).
use crate::model::ChunkSource;
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireMethod {
    Post,
    Delete,
}

pub struct WireRequest {
    pub method: WireMethod,
    /// `https`이면 true. false는 루프백 IP 리터럴로 가는 `http`뿐이다.
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// `/`로 시작하는 경로와 질의
    pub path: String,
    /// (이름, 값, 비밀 여부). 비밀인 값은 진단 출력에 싣지 않는다.
    pub headers: Vec<(String, String, bool)>,
    pub body: Vec<u8>,
    /// 운영자가 사설망을 명시적으로 열었을 때(`RunnerConfig::allow_private_egress`)만 true. 루프백 `http` 예외와는 무관하다.
    pub allow_private: bool,
}

/// 진단 출력에는 머리 이름만 싣는다(값에 자격이 들 수 있다).
impl std::fmt::Debug for WireRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(n, _, _)| n.as_str()).collect();
        f.debug_struct("WireRequest")
            .field("method", &self.method)
            .field("tls", &self.tls)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

pub struct WireResponse {
    pub status: u16,
    /// 응답 머리(이름은 받은 그대로, 값은 글자 그대로)
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn ChunkSource>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireFail {
    /// 연결을 맺기 전·맺는 중에 실패했다: 요청이 서버에 닿지 않았다(미실행이 입증된다).
    Unreached,
    /// 요청을 쓰기 시작한 뒤 응답 머리를 받지 못했다(서버가 받았는지 모른다).
    Lost,
    /// 보내기 직전 확인에서 이 프로세스의 판본이 더는 활성이 아니다(또는 확인하지 못했다): 아무것도 보내지 않았다.
    Stale,
}

pub type WireReply<'a> = Pin<Box<dyn Future<Output = Result<WireResponse, WireFail>> + Send + 'a>>;

pub trait RemoteWire: Send + Sync {
    fn send(&self, request: WireRequest) -> WireReply<'_>;
}

/// 응답 머리 값을 이름(대소문자 무시)으로 찾는다.
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}
