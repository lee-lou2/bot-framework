//! 데이터베이스 점검. 기본은 설정(주소가 있고 형식이 맞는지)만 보고, `--check-db`를 줬을 때만 짧게 접속해 연결을 확인한다.
//! 접속은 마이그레이션·쓰기 없이 연결만 하고 곧 닫는다. 주소(비밀번호가 있을 수 있다)와 접속 오류 글은 출력하지 않는다.
use super::{Item, State};
use crate::settings::Settings;
use bot_store::{Config, Store};
use std::time::Duration;

const SETTING: &str = "데이터베이스";
const CONNECTION: &str = "데이터베이스 연결";
const CONNECT_LIMIT: Duration = Duration::from_secs(5);

/// 주소의 형식을 보고 접속 대상이 이 기계(로컬)인지 원격인지 알려 준다. 호스트 이름은 돌려주지 않는다.
fn target(url: &str) -> Result<&'static str, &'static str> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or("postgres:// 또는 postgresql:// 로 시작하는 주소여야 해요")?;
    let (head, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = head.split('/').next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(inner) => inner.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    let local = |name: &str| {
        name.is_empty()
            || name.starts_with('/')
            || matches!(name, "localhost" | "127.0.0.1" | "::1")
    };
    // 쿼리의 host=·hostaddr= 가 접속 대상을 바꿀 수 있다
    let redirected = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .any(|(key, value)| matches!(key, "host" | "hostaddr") && !local(value));
    Ok(if local(host) && !redirected {
        "로컬"
    } else {
        "원격"
    })
}

pub async fn check(settings: &Settings, check_db: bool, items: &mut Vec<Item>) {
    let Some(url) = settings.database_url.as_deref() else {
        items.push(Item::new(State::Fail, SETTING, "BOT_DATABASE_URL이 없어요").with([
            "serve·chat·접수가 저장소를 쓰려면 PostgreSQL 접속 주소(postgres://사용자@호스트/DB)가 필요해요".to_owned(),
        ]));
        items.push(Item::skipped(CONNECTION, "주소가 없어 건너뛰었어요"));
        return;
    };
    // 접속하지 않고 `connect`와 같은 구문 분석으로 형식을 본다. 실패 이유에는 주소의 조각이 담길 수 있어 고정 문구만 보인다.
    let kind = match target(url) {
        Ok(kind) if Store::check_url(url) => kind,
        _ => {
            items.push(
                Item::new(State::Fail, SETTING, "BOT_DATABASE_URL 형식이 맞지 않아요").with([
                    "postgres://사용자@호스트:포트/DB 형식이어야 해요(주소는 출력하지 않아요)"
                        .to_owned(),
                ]),
            );
            items.push(Item::skipped(
                CONNECTION,
                "주소 형식이 맞지 않아 건너뛰었어요",
            ));
            return;
        }
    };
    items.push(Item::new(
        State::Pass,
        SETTING,
        format!("주소가 설정돼 있어요(형식 정상, 대상: {kind}, 주소는 출력하지 않아요)"),
    ));
    if !check_db {
        items.push(Item::skipped(
            CONNECTION,
            "접속하지 않았어요 — `--check-db`를 주면 짧게 접속해 연결만 확인해요(쓰기·마이그레이션 없음)",
        ));
        return;
    }
    // 접속 오류 글에는 주소가 담길 수 있어 내용은 버리고 분류만 알린다. 접속이 거부되는 동안은 연결 라이브러리가 기다리며 다시 시도하므로
    // 제한 시간이 곧 «연결되지 않음»이다.
    let attempt = tokio::time::timeout(CONNECT_LIMIT, Store::connect(url, Config::default())).await;
    let hint = "주소·계정·네트워크를 확인해 주세요(오류 글에 주소가 담길 수 있어 출력하지 않아요)"
        .to_owned();
    items.push(match attempt {
        Ok(Ok(store)) => {
            store.pool().close().await;
            Item::new(
                State::Pass,
                CONNECTION,
                "연결됐어요(접속만 확인하고 바로 닫았어요 — 쓰기·마이그레이션 없음)",
            )
        }
        Ok(Err(_)) => Item::new(State::Fail, CONNECTION, "연결하지 못했어요").with([hint]),
        Err(_) => Item::new(
            State::Fail,
            CONNECTION,
            format!("{}초 안에 연결되지 않았어요", CONNECT_LIMIT.as_secs()),
        )
        .with([hint]),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_target_without_returning_a_host() {
        for local in [
            "postgres://u:pw@localhost:5432/db",
            "postgresql://127.0.0.1/db",
            "postgres://u@[::1]:5432/db",
            "postgres:///db?host=/var/run/postgresql",
        ] {
            assert_eq!(target(local), Ok("로컬"), "{local}");
        }
        for remote in [
            "postgres://u:pw@db.internal.example:5432/db",
            "postgres:///db?host=db.internal.example",
            "postgres://u@localhost/db?hostaddr=10.0.0.5",
        ] {
            assert_eq!(target(remote), Ok("원격"), "{remote}");
        }
        for bad in ["mysql://u@h/db", "localhost", ""] {
            assert!(target(bad).is_err(), "{bad}");
        }
    }
}
