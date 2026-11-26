//! API 직접 호출 모델 실행기(`RunnerKind::Step`): 파일·셸이 필요 없는 에이전트를 VM 없이 어느 OS에서나 상류 모델 API(Anthropic Messages · OpenAI 호환 Chat
//! Completions — OpenAI·Gemini 호환·Ollama·vLLM)로 실행한다. 모델은 호스트 파일·셸에 닿지 못한다: 이 실행기는 대화를 상류에 보내고 답(최종 글 또는 도구 호출)만 받으며,
//! 도구는 지금처럼 워커가 `Broker::call`(인가·승인·샌드박스·원장·감사)로 부른다. 기존 `claude-cli`(Loop)와 Apple VM 어댑터는 그대로다.
//!
//! - **목적지**: 호스트 설정의 `providers.<별칭>.endpoint`만(`ApiEndpoint`). `https`만 쓰고, `http`는 호스트가 loopback IP 리터럴(127.0.0.0/8·`::1`)일 때만 — 로컬 Ollama·시험.
//!   `localhost` 이름·사설망 `http`·자격(`user@`)·쿼리·조각이 있는 주소는 거절한다. 서비스 키(`secret`)는 loopback일 때만 생략한다.
//! - **전송**(`ApiClient`): 리다이렉트·숨은 재시도·연결 재사용을 끄고, 연결·응답 시간 한도와 본문 크기 상한을 둔다. 키는 보내는 순간 읽어 머리에만 싣고 `sensitive`로 표시한다.
//!   판본 확인(`SendGate`)은 키를 읽기 전과 읽은 뒤 보내기 직전, 두 번 한다: 구판은 철회된 목적지로 대화도 키도 보내지 않고, 키를 읽는 사이에 판본이 바뀌어도 키를 버리고 보내지 않는다.
//! - **재시도 없음**: 실행기는 스스로 다시 보내지 않는다. 429·5xx·연결 실패·시간 초과는 일시 오류, 400·401·403·404·응답 모양 불일치와 쿼터 소진(429 본문이 `insufficient_quota`)은 영구 실패로
//!   분류하고 다시 할지는 워커 정책이 정한다.
//! - **도구 이름**: 상류 규칙(`^[a-zA-Z0-9_-]{1,64}$`)에 맞게 요청마다 되돌릴 수 있는 별칭표를 만든다. 표에 없는 이름·깨진 인자는 도구를 부르지 않고 고정 코드로 끝낸다.
//! - **신뢰 경계**: 맥락 조립(`context::assemble`)이 준 시스템 글만 시스템 자리에 싣는다. 사용자 줄·도구 결과·회상 기억(비신뢰)은 사용자 역할(`user`·`tool_result`·`tool`)에만 간다.
//! - **키가 응답에 있으면 닫는다**: 해당 요청의 실제 서비스 키가 상류 응답(원문과, 풀어 읽은 모든 글 — 최종 글·호출과 함께 온 글·인자·공급자 상태 포함)에 있으면 체크포인트·브로커에
//!   아무것도 넘기지 않고 고정 코드로 끝낸다.
//! - **사용량**: 응답이 실제로 보고한 토큰만 낸다(없으면 없음, 0으로 채우지 않는다). 합계 규칙은 워커가 정한다(`worker/agent.rs`).
//! - **공급자 상태**: 도구 호출과 함께 공급자가 준 서명·원래 호출 ID는 워커의 멱등 호출 ID와 따로 불투명 상태(`ModelStep::Calls::state`)로 체크포인트에 남았다가 같은 공급자에 되돌려진다.
//! - 로그·오류는 고정 어휘 코드만 남긴다. 요청·응답 본문과 키는 어디에도 남기지 않는다.

mod endpoint;
mod names;
mod runner;
mod scan;
mod transport;
mod wire;

pub use endpoint::ApiEndpoint;
pub use runner::{ApiRunner, ApiSettings, Effort, LimitField, SendGate};
pub use transport::ApiClient;
pub use wire::ApiKind;

#[cfg(test)]
mod tests;
