//! Tests for `ToolRunner::admit_secrets` existence checks within the profile namespace.

use super::*;

fn names(list: &[&str]) -> Vec<SecretName> {
    list.iter().map(|n| SecretName::parse(n).unwrap()).collect()
}

#[tokio::test]
async fn it_checks_only_the_profiles_own_namespace_and_only_for_existence() {
    let r = rig(Server::new(&[("delete", schema())]), |_| {}, |_| {});
    let personal = ProfileId("personal".into());
    r.runner
        .admit_secrets(&personal, &names(&["reader", "password"]))
        .await
        .unwrap();
    assert!(matches!(
        r.runner
            .admit_secrets(&personal, &names(&["reader", "absent"]))
            .await,
        Err(bot_core::port::Error::Missing)
    ));
    assert!(matches!(
        r.runner
            .admit_secrets(&ProfileId("other".into()), &names(&["password"]))
            .await,
        Err(bot_core::port::Error::Missing)
    ));
    let asked = r.secrets.resolved().len();
    r.runner.admit_secrets(&personal, &[]).await.unwrap();
    assert_eq!(r.secrets.resolved().len(), asked);
}
