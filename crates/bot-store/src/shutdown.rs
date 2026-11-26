//! 종료 반납: 정상 종료 유예가 지난 뒤 호스트가 이 claim의 모델·도구 프로세스 종료와 세션 토큰 회수를 확인하고 `release_for_shutdown`을 부른다.
//! 지금도 유효한 소유자만 run을 대기열로 돌릴 수 있고, 시도 횟수는 쓰지 않는다(docs/reliability.md «워커 종료와 리스 인계»).

use crate::{
    approvals,
    audit::{self, Entry},
    db::{R, Store},
    finish::lock_leased,
};
use bot_core::{id::Attempt, port::Error};
use sqlx::Row;

impl Store {
    /// 현재 소유자(리스가 끝나지 않았고 세대·claim_epoch가 같은 시도)가 run을 시도 횟수를 쓰지 않고 `ready`로 돌려준다.
    /// 같은 커밋에서 세대를 한 번 올리고 옛 승인을 무효화하며, 바로 다음 점유는 그 세대를 그대로 쓴다(`keep_generation`) —
    /// 다음 점유는 새 claim_epoch만 받는다. 이미 끝난 run·다른 소유자·옛 claim은 `Stale`이라 아무것도 바꾸지 않는다.
    /// 취소 요청이 선 run과 민감 접수 기록은 `Denied`다(취소는 일반 경로가 끝낸다). 호스트의 활성 집합과 무관하다 — 구판 핸들도 부를 수 있다.
    pub(crate) async fn release_for_shutdown_run(&self, at: &Attempt) -> R<()> {
        let mut tx = self.pool.begin().await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private || run.cancel_requested {
            return Err(Error::Denied.into());
        }
        sqlx::query(
            "update bot_runs set state = 'ready', lease_until_ms = null, lease_owner = null,
                    generation = generation + 1, keep_generation = true, updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .execute(&mut *tx)
        .await?;
        // 옛 세대가 연 승인은 무효다(옛 버튼 거절). 시작한 호출은 그대로 두고 다음 점유의 브로커가 복구를 판정한다
        approvals::supersede_generation(&mut tx, &run.profile, &run.id, at.generation.0 as i64)
            .await?;
        audit::append(
            &mut tx,
            &run.profile,
            &Entry::new("system", "run.release", &run.id, "shutdown"),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 이 시도(세대)가 시작해 아직 끝나지 않은 도구 호출의 소유 표지들. 호스트의 종료 정리가 그 프로세스를 `reap`으로 끝내고 확인한다.
    /// 호출이 없으면 빈 목록이다. 현재 소유자 여부는 보지 않는다(읽기뿐이다).
    pub async fn started_owners(&self, at: &Attempt) -> Result<Vec<String>, Error> {
        sqlx::query(
            "select owner from bot_calls where profile = $1 and run = $2 and state = 'started' and started_generation = $3
             and owner is not null order by call",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(at.generation.0 as i64)
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(|row| row.get("owner")).collect())
        .map_err(|e| crate::db::Fail::from(e).into_core())
    }
}
