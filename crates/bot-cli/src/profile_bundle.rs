//! `bot profile-bundle put <폴더>` — 옛 판본의 원본 프로필 폴더를 저장소에 보관한다.
//! 이 기능이 생기기 전의 버전이 보관하지 않은 판본을 진행 중 작업이 아직 쓰고 있어 `bot serve`가 복원하지 못할 때, 같은 판본의 원본 폴더로 채운다.
//! 폴더를 검증해 만든 묶음은 자기 해시를 판본으로 보관하므로, 프로필 ID와 해시가 맞는 원본일 때만 복원에 쓰인다(다른 폴더는 다른 키가 된다). 호스트 등록부는 보지 않는다.

use crate::settings::Settings;
use bot_core::{id::Revision, port::Error, profile::ProfileBundles};
use bot_profile::{Limits, read_bundle, read_legacy_bundle};
use bot_store::{Config, Store};
use std::{collections::BTreeMap, path::Path};

pub async fn put(settings: &Settings, dir: &Path) -> Result<(), String> {
    let limits = Limits::default();
    let current = read_bundle(dir, &limits);
    let legacy = read_legacy_bundle(dir, &limits);
    let profile_id = match (&current, &legacy) {
        (Ok((id, _, _)), _) | (_, Ok((id, _, _))) => id.clone(),
        (Err(report), Err(_)) => {
            return Err(format!("프로필 폴더를 검증하지 못했어요:\n{report}"));
        }
    };
    let url = settings
        .database_url
        .as_deref()
        .ok_or("BOT_DATABASE_URL이 필요해요(PostgreSQL 접속 주소)")?;
    let store = Store::connect(url, Config::default())
        .await
        .map_err(|_| "저장소에 접속하지 못했어요")?;
    store
        .migrate()
        .await
        .map_err(|_| "저장소 마이그레이션에 실패했어요")?;
    // 구판(`BOTPROFILE1`) 판본이 저장소에 고정돼 있으면 같은 원본 폴더로 그 옛 SHA-256을 재생성해 보충한다
    let target = BTreeMap::from([(profile_id, Revision(String::new()))]);
    let pinned = store
        .pinned_revisions(&target)
        .await
        .map_err(|_| "저장소에서 고정 판본을 읽지 못했어요")?;
    let (profile, revision, bundle) = match (legacy, current) {
        (Ok((id, rev, bytes)), _) if pinned.contains(&(id.clone(), rev.clone())) => {
            (id, rev, bytes)
        }
        (_, Ok(built)) => built,
        (_, Err(report)) => {
            return Err(format!("프로필 폴더를 검증하지 못했어요:\n{report}"));
        }
    };
    store
        .put_bundle(profile.clone(), revision.clone(), bundle)
        .await
        .map_err(|error| match error {
            Error::Conflict => {
                "저장소에 같은 판본 번호로 다른 묶음이 이미 있어요 — 저장소가 손상됐을 수 있어요"
            }
            Error::Unavailable => "저장소에 닿지 못했어요",
            _ => "저장소가 묶음을 받지 않았어요",
        })?;
    println!(
        "프로필 {} 판본 {}의 묶음을 보관했어요(이미 있었다면 그대로예요)",
        profile.0, revision.0
    );
    Ok(())
}
