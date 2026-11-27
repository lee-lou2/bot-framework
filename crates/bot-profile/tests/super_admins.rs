//! Integration tests for profile `super_admins` parsing, validation, host ceiling, policy revision, and bundle restore.
mod support;

use bot_core::id::{PrincipalId, ProfileId};
use bot_profile::{Limits, from_bundle, load};
use std::collections::BTreeSet;
use support::{Temp, base, codes, edit, host, ok, snapshot, write_profile};

#[test]
fn valid_super_admins_parse_and_round_trip_through_bundle() {
    let files = edit(
        base(),
        "profile.yaml",
        "  - {identity: \"service:morning\", roles: [reader]}",
        "  - {identity: \"service:morning\", roles: [reader]}\n  - {identity: \"slack:home/U1\", roles: [owner]}\n  - {identity: \"http:reminders/alice\", roles: [owner]}\nsuper_admins: [\"local:owner\", \"slack:home/U1\", \"http:reminders/alice\"]",
    );
    let set = ok(&files);
    let id = ProfileId("personal".into());
    let snap = set.profile(&id).unwrap();
    assert!(snap.is_super_admin(&PrincipalId("local:owner".into())));
    assert!(snap.is_super_admin(&PrincipalId("slack:home/U1".into())));
    assert!(snap.is_super_admin(&PrincipalId("http:reminders/alice".into())));
    assert!(!snap.is_super_admin(&PrincipalId("service:morning".into())));
    assert!(!snap.is_super_admin(&PrincipalId("local:other".into())));

    let bundle = set.bundle(&id).unwrap();
    let restored = from_bundle(bundle, &id, &snap.revision, &Limits::default()).unwrap();
    assert_eq!(restored.super_admins, snap.super_admins);
}

#[test]
fn rejects_service_non_member_and_invalid_super_admins() {
    let service = edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "super_admins: [\"service:morning\"]\nsecrets: [calendar-reader]",
    );
    assert!(
        codes(&service).contains(&"super-admin-service"),
        "{:?}",
        codes(&service)
    );

    let non_member = edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "super_admins: [\"local:other\"]\nsecrets: [calendar-reader]",
    );
    assert!(
        codes(&non_member).contains(&"super-admin-not-member"),
        "{:?}",
        codes(&non_member)
    );

    let bad_id = edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "super_admins: [\"not-a-principal\"]\nsecrets: [calendar-reader]",
    );
    assert!(
        codes(&bad_id).contains(&"bad-identity"),
        "{:?}",
        codes(&bad_id)
    );
}

#[test]
fn host_super_admins_ceiling_restricts_profile_super_admins() {
    let files = edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "super_admins: [\"local:owner\"]\nsecrets: [calendar-reader]",
    );
    let root = Temp::new("super-admin-host");
    write_profile(root.path(), "personal", &files);

    // When host `super_admins` is empty, profile declarations are unrestricted.
    let open_host = host();
    assert!(load(root.path(), &open_host, &Limits::default()).is_ok());

    let mut blocked = host();
    blocked.super_admins.insert(
        "personal".into(),
        BTreeSet::from(["local:other".to_owned()]),
    );
    let report = load(root.path(), &blocked, &Limits::default()).unwrap_err();
    assert!(
        report.codes().contains(&"super-admin-not-allowed"),
        "{report}"
    );

    let mut allowed = host();
    allowed.super_admins.insert(
        "personal".into(),
        BTreeSet::from(["local:owner".to_owned()]),
    );
    assert!(load(root.path(), &allowed, &Limits::default()).is_ok());
}

#[test]
fn policy_revision_is_unchanged_without_super_admins_and_changes_when_set() {
    let baseline = snapshot(&base());
    let empty_list = snapshot(&edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "super_admins: []\nsecrets: [calendar-reader]",
    ));
    let with_admin = snapshot(&edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "super_admins: [\"local:owner\"]\nsecrets: [calendar-reader]",
    ));

    assert_eq!(
        baseline.policy_revision, empty_list.policy_revision,
        "empty super_admins must keep the existing policy revision hash"
    );
    assert_ne!(
        baseline.policy_revision, with_admin.policy_revision,
        "non-empty super_admins must change the policy revision hash"
    );
}
