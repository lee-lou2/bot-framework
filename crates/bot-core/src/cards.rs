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
    api::ApprovalBlock,
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

/// 전달 작업을 점유할 때마다 시도가 오른다. 사람의 버튼은 기존 nonce를 계속 사용한다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardKey {
    pub nonce: ApprovalId,
    pub attempt: u32,
}

/// 소유자 DM으로 가야 하는 승인 카드 한 건.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardJob {
    pub key: CardKey,
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

/// HTTP(OpenAI 호환) 출처 실행의 카드가 «원래 요청»에서 이어받은 고정 증거. 채널 출처의 카드에는 없다.
/// 전달 바인딩(`CardJob::binding`)·지정된 사람(`CardJob::owner`)은 승인 경로가 정하고, 이 값들은 그 경로를 접수 때 고정한 것과 다시 맞춰 보는 데 쓴다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CardOrigin {
    /// 원래 API 모델 바인딩(위임 자식이어도 루트의 것).
    pub binding: BindingId,
    /// 요청한 HTTP 주체. 승인자와 다르다.
    pub requester: PrincipalId,
    /// 접수 때 고정한 프로필 판본.
    pub revision: Revision,
    /// 접수 때 호스트가 확인한 전달 설치(Slack `team_id`). 첫 카드 발송 전 현재 설치와 같아야 한다.
    pub team: Option<String>,
}

pub trait CardOutbox: Send + Sync {
    /// 전달 작업을 리스로 점유한다. `sending` 불명 작업은 자동으로 다시 잡지 않는다(오래되면 저장소가 닫는다).
    fn claim(&self, limit: u32) -> Reply<'_, Vec<CardJob>>;
    /// 현재 발송 시도와 리스·활성 집합·카드 유효·세대·프로필 판본을 확인하고 `sending`을 기록한다. 최초 전송자만 Ok를 받는다.
    fn begin(&self, key: CardKey, checked_revision: Revision) -> Reply<'_, ()>;
    /// 결과를 기록한다(`Posted`면 카드를 열고 공개 안내 한 줄, 아니면 닫는다). 같은 정산의 재전달은 같은 결과다.
    fn settle(&self, key: CardKey, outcome: CardOutcome) -> Reply<'_, ()>;
    /// 이 카드가 이어받은 원래 요청의 고정 증거. HTTP(OpenAI 호환) 출처 카드에만 `Some`이다(기본 구현은 없음).
    fn origin(&self, _nonce: ApprovalId) -> Reply<'_, Option<CardOrigin>> {
        Box::pin(async { Ok(None) })
    }
    /// 올리기 전에 승인 경로가 어긋났음을 알았다(승인 지정·사람 멤버 철회, 전달 경로·설치·바인딩 재지정): 카드를 올리지 않고 작업과 승인을 닫으며
    /// 저장된 판정(`ApprovalBlock`)을 남긴다. 조회가 이 낱말로 «도구가 실행되지 않았다»를 알린다. 이 구분을 모르는 구현은 전달 실패로 정산한다(기본 구현).
    fn block(&self, key: CardKey, _why: ApprovalBlock) -> Reply<'_, ()> {
        self.settle(key, CardOutcome::Unavailable)
    }
}
