//! 소유자 DM 승인 카드 전달 계약(docs/reliability.md «Slack 소유자에게 승인 카드를 전달할 때»).
//!
//! 저장소가 승인과 같은 커밋에 **전달 작업**(`CardJob`)을 쌓는다. 영수증이 확정되기 전의 카드는 열리지 않은 카드라 누르거나 결정할 수 없다.
//! 게이트웨이가 작업을 점유(`claim`)하고, 현재 활성 판본으로 원래 바인딩이 아직 이 설치의 것인지·소유자가 아직 멤버인지 확인한 뒤 `begin`이 Ok일 때만
//! (**최초 전송자만**) 소유자 DM에 카드를 한 번 올리고 결과를 `settle`한다. 올라갔는지 불명이면 `Held`로 정산하고 자동으로 다시 올리지 않는다.
//! 저장소는 `Posted`의 영수증을 활성 집합·세대·유효기간과 다시 대조해 한 커밋에서 카드를 열고 원래 채널에 `DM_SENT_NOTICE` 한 줄만 등록한다.
//! 열 수 없거나 올릴 수 없거나 불명이면 `APPROVAL_DM_UNAVAILABLE`로 닫는다(승인 소비 0·목적지 발송 0·성공 안내 0, 반복 예약은 그대로). 닫는 같은 커밋에서 원래 채널에
//! 닫혔다는 한 줄(`DM_UNAVAILABLE_NOTICE`·`DM_UNCONFIRMED_NOTICE`)만 남는다.
//! 확정한 영수증은 `StoredApproval::delivery`로 읽는다(클릭 대조용).

use crate::{
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId, Revision},
    intake::NotSentReason,
    port::Reply,
};
use std::time::Duration;

/// 원래 채널·스레드에 가는 공개 안내 한 줄(초안·수신자·해시·버튼·승인 식별자는 없다).
pub const DM_SENT_NOTICE: &str = "DM으로 승인 요청을 보냈어요";

/// 소유자 DM을 열 수 없거나 카드를 올릴 수 없다고 확정해 요청을 닫았을 때 원래 채널·스레드에 남기는 한 줄(초안·수신자·해시·승인 식별자는 없다).
pub const DM_UNAVAILABLE_NOTICE: &str = "승인 요청을 DM으로 보낼 수 없어 이번 요청을 닫았어요.";

/// 카드가 올라갔는지 알 수 없어 요청을 닫았을 때 원래 채널·스레드에 남기는 한 줄. 불명한 사실은 단정하지 않는다.
pub const DM_UNCONFIRMED_NOTICE: &str =
    "승인 요청이 DM으로 전달됐는지 확인하지 못해 이번 요청을 닫았어요.";

/// 승인 카드를 소유자 DM으로 전달하지 못해 닫았다는 고정 낱말: 감사 결과이자 미발송 영수증 `{"status":"approval_dm_unavailable"}`의 값이다.
pub const APPROVAL_DM_UNAVAILABLE: &str = "approval_dm_unavailable";

/// 소유자 DM으로 가야 하는 승인 카드 한 건.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardJob {
    pub nonce: ApprovalId,
    pub profile: ProfileId,
    /// 원래 입력 바인딩(Slack 채널 바인딩): 카드를 올릴 설치를 정한다
    pub binding: BindingId,
    /// 저장된 요청자(= 지정 승인자)의 사람 신원 `slack:<별칭>/<U…>`
    pub owner: PrincipalId,
    /// 승인 문서와 같은 바이트의 카드 `{nonce,title,lines}`
    pub card: JsonPayload,
    pub generation: Generation,
}

/// 전달 결과 정산.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CardOutcome {
    /// 응답이 확인한 설치 팀·DM 채널·메시지 `ts`
    Posted {
        team: String,
        dm: String,
        ts: String,
    },
    /// 아무것도 올라가지 않았다(열기 일시 실패·속도 제한·연결 전 실패·자격을 못 읽음): 백오프 뒤 다시 점유한다
    NotSent {
        reason: NotSentReason,
        retry_after: Option<Duration>,
    },
    /// DM 경로가 없거나 카드를 올릴 수 없다: `APPROVAL_DM_UNAVAILABLE`로 닫는다
    Unavailable,
    /// 올라갔는지 알 수 없다: 카드는 닫힌 채로 `APPROVAL_DM_UNAVAILABLE`로 닫는다(자동 재게시 0)
    Held,
}

/// 영수증이 확정된 카드의 전달 기록(클릭 대조용). 소유자 DM으로 전달한 카드에만, 게시를 확인한 뒤에만 있다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardReceipt {
    pub team: String,
    pub dm: String,
    pub ts: String,
    pub owner: PrincipalId,
    pub profile: ProfileId,
    pub binding: BindingId,
}

pub trait CardOutbox: Send + Sync {
    /// 전달 작업을 리스로 점유한다. `sending` 불명 작업은 자동으로 다시 잡지 않는다(오래되면 저장소가 닫는다).
    fn claim(&self, limit: u32) -> Reply<'_, Vec<CardJob>>;
    /// 현재 활성 집합·카드 유효·세대·프로필 판본을 확인하고 `sending`을 기록한다. 최초 전송자만 Ok를 받는다(`Conflict`는 이미 시작됨, `Stale`은 낡음).
    fn begin(&self, nonce: ApprovalId, checked_revision: Revision) -> Reply<'_, ()>;
    /// 결과를 기록한다(`Posted`면 카드를 열고 공개 안내 한 줄, 아니면 닫는다). 같은 정산의 재전달은 같은 결과다.
    fn settle(&self, nonce: ApprovalId, outcome: CardOutcome) -> Reply<'_, ()>;
}
