//! 종료 정리 확인(`bot_runtime::Quiesce`): 종료 유예가 지난 뒤 남은 run을 대기열로 돌려주기 전에 이 시도(claim)가 띄운 것이 모두 끝났음을 확인한다.
//! 순서(docs/reliability.md): 모델 VM이 끝났음을 확인(`reap`) → 이 시도가 시작해 둔 도구 호출마다 프로세스가 끝났음을 확인(`ToolRunner::reap`) → 세션 토큰을 거둔다 →
//! 거둔 뒤에 막 시작된 호출이 없는지 다시 훑어 끝낸다. (모델 프로세스는 워커가 반납 신호로 실행기를 멈출 때 이미 끝났고, 이 단계가 그것을 «확인»한다.)
//! 하나라도 확인하지 못하면 오류다 — 워커는 반납하지 않고 리스 인계 규칙에 맡긴다(다음 점유의 브로커가 시작해 둔 호출을 같은 `reap`으로 복구한다).

use bot_broker::{front::SessionTable, ports::ToolRunner};
use bot_core::{
    id::Attempt,
    port::{Error, Reply},
    sandbox::ModelSandbox,
};
use bot_runtime::{Quiesce, model::container::ContainerSandbox};
use bot_store::Store;
use std::{collections::BTreeSet, sync::Arc};

/// 세션을 거둔 뒤 도구 호출을 다시 훑는 최대 횟수(새 소유 표지가 없을 때까지).
const PASSES: usize = 3;

/// 이 시도(세대)가 시작해 아직 끝나지 않은 도구 호출의 소유 표지를 알려 주는 입구. 저장소가 구현한다.
pub trait StartedCalls: Send + Sync {
    fn owners(&self, attempt: Attempt) -> Reply<'_, Vec<String>>;
}

impl StartedCalls for Store {
    fn owners(&self, attempt: Attempt) -> Reply<'_, Vec<String>> {
        Box::pin(async move { self.started_owners(&attempt).await })
    }
}

pub struct Quiescer {
    started: Arc<dyn StartedCalls>,
    tools: Arc<dyn ToolRunner>,
    /// 모델 CLI를 쓰는 호스트일 때만: 세션 표와 모델 샌드박스
    model: Option<(Arc<SessionTable>, Arc<ModelSandbox>)>,
}

impl Quiescer {
    pub fn new(
        started: Arc<dyn StartedCalls>,
        tools: Arc<dyn ToolRunner>,
        model: Option<(Arc<SessionTable>, Arc<ModelSandbox>)>,
    ) -> Self {
        Self {
            started,
            tools,
            model,
        }
    }
}

impl Quiescer {
    /// 이 시도가 시작해 둔 호출 가운데 아직 확인하지 않은 것을 `reap`한다. 새로 확인한 개수를 돌려준다.
    async fn reap_started(
        &self,
        attempt: &Attempt,
        done: &mut BTreeSet<String>,
    ) -> Result<usize, Error> {
        let owners = self.started.owners(attempt.clone()).await?;
        let fresh: Vec<String> = owners
            .into_iter()
            .filter(|owner| !done.contains(owner))
            .collect();
        for owner in &fresh {
            self.tools.reap(owner.clone()).await?;
            done.insert(owner.clone());
        }
        Ok(fresh.len())
    }
}

impl Quiesce for Quiescer {
    fn quiesce(&self, attempt: Attempt) -> Reply<'_, ()> {
        Box::pin(async move {
            if let Some((_, sandbox)) = &self.model {
                sandbox.reap(&ContainerSandbox::owner_of(&attempt)).await?;
            }
            let mut done = BTreeSet::new();
            self.reap_started(&attempt, &mut done).await?;
            if let Some((sessions, _)) = &self.model {
                sessions.revoke_attempt(&attempt);
            }
            // 세션을 거둔 뒤에도 이미 들어와 있던 요청이 호출을 막 시작했을 수 있다: 새 호출이 없을 때까지 다시 본다
            for _ in 0..PASSES {
                if self.reap_started(&attempt, &mut done).await? == 0 {
                    return Ok(());
                }
            }
            // 여러 번 훑고도 새 호출이 계속 생겼다: 확인하지 못했다
            Err(Error::Unavailable)
        })
    }
}

#[cfg(test)]
mod tests;
