//! 사람 승인 카드가 **올라간 Slack 설치**(team)와 지금 호스트 등록부의 같은 설치를 대조한다.
//!
//! 프로필 판본에는 워크스페이스 **별칭**만 있다(`slack:home/U123`, 전달 바인딩). 같은 별칭의 실제 `team_id`만 바뀐 재배포는 프로필·사람·바인딩이 그대로라서
//! 고정·현재 판본의 경로 비교(`approver::openai`)로는 알 수 없다. 카드가 실제로 올라가 눌린 설치는 확정한 게시 영수증(`CardReceipt::team`)이 말해 주므로,
//! 승인을 소비하기 전(과 클릭할 때) 그 team이 지금 등록부(`ScheduleRoutes`: 같은 프로필의 운영자 등록부 하나, 외부 조회 없음)가 전달 바인딩의 채널 별칭을 풀어 낸
//! 정규 경로 `팀ID/채널ID`의 팀과 같은지 본다. 소비 커밋은 호스트 활성 집합을 고정한 핸들이 DB 잠금 안에서 다시 대조한다.

use super::{BrokerService, route::Route};
use crate::approver::openai::Origin;
use bot_core::{
    id::{BindingId, ProfileId},
    ledger::StoredApproval,
    port::Error,
    profile::{Adapter, ApprovalRoute, Snapshot},
    schedule::DeliveryTarget,
    state::{Conversation, ConversationKind},
};

impl BrokerService {
    /// 전달 바인딩 `binding`의 설치가 지금 등록부에서도 team `posted`인가. 같으면 `Same`, 바인딩이 더는 Slack 채널 바인딩이 아니거나 등록부가 거절하거나 다른
    /// 팀이면 `Gone`(확정), 등록부가 없거나 일시 오류면 `Unsure`(아무것도 확정하지 않고 다시 시도한다).
    pub(super) fn site_state(
        &self,
        profile: &ProfileId,
        snapshot: &Snapshot,
        binding: &BindingId,
        posted: &str,
    ) -> Route {
        let Some(Adapter::SlackChannel { channel, .. }) =
            snapshot.bindings.get(binding).map(|b| &b.source)
        else {
            return Route::Gone;
        };
        let target = DeliveryTarget {
            binding: binding.clone(),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: channel.clone(),
                thread: None,
            },
        };
        match self.route(profile, &target) {
            Ok(now) => match now.conversation.id.split_once('/') {
                Some((team, _)) if team == posted => Route::Same,
                _ => Route::Gone,
            },
            Err(Error::Denied | Error::Invalid) => Route::Gone,
            Err(_) => Route::Unsure,
        }
    }

    /// 이 승인 카드가 올라간 설치가 지금도 같은 설치인가. 본인 인증된 관리 화면(로컬 경로)은 설치가 없어 늘 `Same`이다. Slack 경로인데 확정한 영수증이 없으면
    /// 카드가 어느 설치에 올라갔는지 모르므로 `Gone`이다(영수증이 없는 카드는 눌릴 수도 없다).
    pub(super) fn card_site(
        &self,
        stored: &StoredApproval,
        snapshot: &Snapshot,
        found: &Origin,
    ) -> Route {
        let ApprovalRoute::Slack { binding, .. } = &found.route else {
            return Route::Same;
        };
        match &stored.delivery {
            Some(receipt) => self.site_state(
                &stored.approval.attempt.profile,
                snapshot,
                binding,
                &receipt.team,
            ),
            None => Route::Gone,
        }
    }
}
