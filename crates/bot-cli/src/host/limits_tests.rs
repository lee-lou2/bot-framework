use super::*;

fn host(limits: &str) -> HostFile {
    parse(&format!(
        r#"{{"version": 1,
            "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": 501}}}}}}}},
            "models": {{"personal": ["assistant"]}}{limits}}}"#
    ))
    .unwrap()
}

fn fingerprint(host: &HostFile) -> [u8; 32] {
    host.authority_fingerprint(&BTreeMap::new()).unwrap()
}

fn ceiling(secs: u32) -> String {
    format!(r#", "limits": {{"tool_call_max_secs": {secs}}}"#)
}

#[test]
fn the_ceiling_defaults_to_sixty_and_reaches_the_catalog_that_validates_profiles() {
    let plain = host("");
    assert_eq!(plain.limits.tool_call_max_secs, 60);
    assert_eq!(plain.catalog.tool_call_cap(), 60);
    let raised = host(&ceiling(900));
    assert_eq!(raised.limits.tool_call_max_secs, 900);
    assert_eq!(raised.catalog.tool_call_cap(), 900);
    assert_eq!(demo(1000).catalog.tool_call_cap(), 60);
    assert_eq!(host(r#", "limits": {}"#).limits, plain.limits);
}

#[test]
fn a_bad_limits_section_stops_the_host_file_before_anything_starts() {
    for (case, section, why) in [
        ("0 secs", ceiling(0), "limits.tool_call_max_secs"),
        ("over 900", ceiling(901), "limits.tool_call_max_secs"),
        (
            "not an integer",
            r#", "limits": {"tool_call_max_secs": "300"}"#.to_owned(),
            "limits.tool_call_max_secs",
        ),
        (
            "unknown key",
            r#", "limits": {"tool_call_max": 300}"#.to_owned(),
            "limits.tool_call_max",
        ),
        ("not an object", r#", "limits": 300"#.to_owned(), "limits"),
    ] {
        let text = format!(
            r#"{{"version": 1, "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": 501}}}}}}}}{section}}}"#
        );
        let error = match parse(&text) {
            Ok(_) => panic!("{case}: expected rejection"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(why), "{case}: {error}");
    }
}

#[test]
fn only_a_changed_ceiling_changes_the_activation_fingerprint() {
    let base = fingerprint(&host(""));
    assert_eq!(
        fingerprint(&host(&ceiling(60))),
        base,
        "explicit default must keep the existing fingerprint"
    );
    assert_eq!(fingerprint(&host(r#", "limits": {}"#)), base);
    let raised = fingerprint(&host(&ceiling(300)));
    assert_ne!(raised, base, "raising ceiling must force re-activation");
    assert_ne!(fingerprint(&host(&ceiling(900))), raised);
    assert_ne!(
        fingerprint(&host(&ceiling(30))),
        base,
        "lowering ceiling must force re-activation"
    );
    assert_eq!(
        fingerprint(&host(&ceiling(300))),
        raised,
        "same value must produce the same fingerprint"
    );
}
