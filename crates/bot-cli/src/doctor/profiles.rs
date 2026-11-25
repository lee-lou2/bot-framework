//! 프로필 검증, 입구 표, 필요한 자격 이름 점검. 자격은 **이름**만 보고하고 값은 출력하지 않는다.
//! 보관소가 sops이면 복호화가 필요해(외부 프로그램·키 서비스 접속 가능) 확인하지 않고 «미검사»로 둔다.
use super::{Item, State, capped, host::Checked};
use crate::{
    host::HostFile,
    load::{check_entrances, secret_store},
    serve::{Publication, seed},
    settings::{SecretsKind, Settings},
    validate::{
        missing_model_sandbox, missing_named, needed_secrets, weak_http_keys, weak_openai_keys,
    },
};
use bot_profile::{Limits, ProfileSet, Registry};
use std::sync::Arc;

const PROFILES: &str = "프로필";
const ENTRANCES: &str = "입구";
const SECRETS: &str = "자격";

fn skipped_rest(items: &mut Vec<Item>, why: &str) {
    items.push(Item::skipped(ENTRANCES, why));
    items.push(Item::skipped(SECRETS, why));
}

pub async fn check(settings: &Settings, host: Option<&Checked>, items: &mut Vec<Item>) {
    let Some(root) = settings.profiles.as_ref() else {
        items.push(
            Item::new(State::Fail, PROFILES, "프로필 루트 폴더가 없어요").with([
                "BOT_PROFILES나 `--profiles <폴더>`로 프로필 폴더들의 부모 폴더를 지정해 주세요"
                    .to_owned(),
                "새로 시작하려면 `bot init <이름>`으로 만들 수 있어요".to_owned(),
            ]),
        );
        skipped_rest(items, "프로필을 읽지 못해 건너뛰었어요");
        return;
    };
    let Some(Checked {
        file: host,
        revisions,
    }) = host
    else {
        items.push(Item::skipped(
            PROFILES,
            "호스트 설정이 올바르지 않아 건너뛰었어요",
        ));
        skipped_rest(items, "호스트 설정이 올바르지 않아 건너뛰었어요");
        return;
    };
    let registry = Registry::new();
    let set = match registry.reload(root, &host.catalog, &Limits::default()) {
        Ok(set) => set,
        Err(report) => {
            let lines = report
                .to_string()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            items.push(
                Item::new(State::Fail, PROFILES, "검증에 실패했어요").with(capped(lines, 30)),
            );
            skipped_rest(items, "프로필 검증이 실패해 건너뛰었어요");
            return;
        }
    };
    items.push(profiles(&set, host, revisions.as_ref()));
    items.push(match check_entrances(set.active().clone(), host) {
        Ok(_) => Item::new(State::Pass, ENTRANCES, "바인딩과 호스트 입구가 맞아요"),
        Err(error) => Item::new(State::Fail, ENTRANCES, error),
    });
    items.push(secrets(settings, host, &set).await);
}

/// 프로필 검증 뒤 `bot validate`·`bot serve`가 시작 전에 하는 나머지 판정(모델 VM 설정·시드·활성화 판본)을 같은 함수로 한다.
fn profiles(
    set: &Arc<ProfileSet>,
    host: &HostFile,
    revisions: Option<&std::collections::BTreeMap<String, bot_core::id::Revision>>,
) -> Item {
    let mut problems = Vec::new();
    for snapshot in set.active().values() {
        problems.extend(missing_model_sandbox(snapshot, host));
        problems.extend(seed::problems(snapshot));
    }
    // 서버 판본을 계산하지 못했으면(도구 서버 항목이 이미 알린다) 활성화 판본은 만들 수 없다
    if let Some(revisions) = revisions
        && let Err(error) = Publication::of(set, host, revisions)
    {
        problems.push(error);
    }
    let lines = set
        .render()
        .lines()
        .skip(1)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if problems.is_empty() {
        return Item::new(
            State::Pass,
            PROFILES,
            format!("{}개 검증을 통과했어요", set.active().len()),
        )
        .with(capped(lines, 30));
    }
    Item::new(
        State::Fail,
        PROFILES,
        format!("시작 전 점검에서 막히는 것 {}건", problems.len()),
    )
    .with(capped(problems, 30))
    .with(capped(lines, 30))
}

async fn secrets(settings: &Settings, host: &HostFile, set: &Arc<ProfileSet>) -> Item {
    if matches!(settings.secrets, SecretsKind::Sops(_)) {
        return Item::skipped(
            SECRETS,
            "sops 보관소는 복호화가 필요해 확인하지 않았어요 — `bot validate`가 확인해요",
        );
    }
    let store = match secret_store(settings) {
        Ok(store) => store,
        Err(error) => return Item::new(State::Fail, SECRETS, error),
    };
    let active = set.active();
    let mut problems = Vec::new();
    let mut total = 0;
    for snapshot in active.values() {
        let names = needed_secrets(snapshot, host);
        total += names.len();
        problems.extend(missing_named(&*store, &snapshot.id, names, &settings.secrets).await);
        problems.extend(weak_http_keys(&*store, snapshot, &host.gateway).await);
    }
    let snapshots: Vec<_> = active.values().cloned().collect();
    problems.extend(weak_openai_keys(&*store, &snapshots, &host.gateway).await);
    if problems.is_empty() {
        let summary = match total {
            0 => "필요한 자격이 없어요".to_owned(),
            n => format!("필요한 자격 {n}개가 모두 있어요(값은 확인만 하고 출력하지 않아요)"),
        };
        return Item::new(State::Pass, SECRETS, summary);
    }
    Item::new(
        State::Fail,
        SECRETS,
        format!(
            "없거나 문제 있는 자격 {}건(이름만 보여 줘요, 값은 출력하지 않아요)",
            problems.len()
        ),
    )
    .with(capped(problems, 30))
}
