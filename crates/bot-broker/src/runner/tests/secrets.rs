//! `ToolRunner::admit_secrets`: 선언한 자격 이름이 이 프로필의 보관소에 있는지만 본다(값은 쓰거나 남기지 않는다).

use super::*;

fn names(list: &[&str]) -> Vec<SecretName> {
    list.iter().map(|n| SecretName::parse(n).unwrap()).collect()
}

#[tokio::test]
async fn it_checks_only_the_profiles_own_namespace_and_only_for_existence() {
    let r = rig(Server::new(&[("delete", schema())]), |_| {}, |_| {});
    let personal = ProfileId("personal".into());
    // 있는 이름만 선언했다
    r.runner
        .admit_secrets(&personal, &names(&["reader", "password"]))
        .await
        .unwrap();
    // 하나라도 없으면 `Missing`이고 다른 오류와 구분된다
    assert!(matches!(
        r.runner
            .admit_secrets(&personal, &names(&["reader", "absent"]))
            .await,
        Err(bot_core::port::Error::Missing)
    ));
    // 다른 프로필에만 있는 이름은 이 프로필에서 없는 것이다(이름공간이 나뉜다)
    assert!(matches!(
        r.runner
            .admit_secrets(&ProfileId("other".into()), &names(&["password"]))
            .await,
        Err(bot_core::port::Error::Missing)
    ));
    // 선언이 없으면 보관소를 묻지 않는다
    let asked = r.secrets.resolved().len();
    r.runner.admit_secrets(&personal, &[]).await.unwrap();
    assert_eq!(r.secrets.resolved().len(), asked);
}
