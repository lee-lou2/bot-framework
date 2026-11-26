//! `container run` 인자 조립: 보안 플래그·진입·릴레이 고정·거절·이름 모양·이미지 고정(순수 함수).

use super::*;

#[test]
fn every_variable_that_carries_the_session_token_is_treated_as_a_secret() {
    // 세션 토큰은 `-e 이름`(값 없이)으로만 가고 값은 클라이언트 프로세스 환경으로 간다: 모델 요청이 싣는 이름 둘 모두 비밀로 본다
    for name in ["ANTHROPIC_API_KEY", "RELAY_TOKEN"] {
        assert!(crate::model::container::args::secret_like(name), "{name}");
    }
    assert!(!crate::model::container::args::secret_like("HOME"));
}

#[test]
fn the_command_line_is_locked_down_and_the_secret_stays_out_of_it() {
    let fake = Fake::new("args");
    let spec = fake.spec();
    let command = build(&fake.config(), &spec, &plan(Some(relay()))).unwrap();
    let a = &command.args;
    let at = |flag: &str| {
        a.iter()
            .position(|x| x == flag)
            .unwrap_or_else(|| panic!("{flag}: {a:?}"))
    };
    // 고정 보안 플래그와 호스트 전용 망·자원 한도·소유 라벨·이름·작업 폴더
    assert_eq!(a[0], "run");
    for flag in ["--rm", "--read-only", "--init", "--no-dns"] {
        at(flag);
    }
    assert_eq!(
        (
            a[at("--tmpfs") + 1].as_str(),
            a[at("--cap-drop") + 1].as_str()
        ),
        ("/tmp", "ALL")
    );
    assert!(
        !a.contains(&"--cap-add".to_owned()),
        "방화벽 진입이 없으면 더하는 권한도 없다"
    );
    assert_eq!(a[at("--network") + 1], "botvm-net");
    assert_eq!(
        (a[at("-c") + 1].as_str(), a[at("-m") + 1].as_str()),
        ("2", "2048M")
    );
    assert_eq!(a[at("-w") + 1], "/bot");
    let owner = args::owner_of(&spec);
    assert_eq!(owner, "personal.run-1.g2.c3");
    for label in [
        format!("{OWNER_LABEL}={}", label_value(&owner)),
        format!("{MANAGED_LABEL}=1"),
        format!("{INSTANCE_LABEL}=botvm"),
        format!("{INCARNATION_LABEL}=inc1"),
    ] {
        assert!(a.contains(&label), "{label}: {a:?}");
    }
    assert_eq!(command.name, a[at("--name") + 1]);
    assert!(is_ours("botvm", &command.name) && command.name.ends_with("-7"));
    assert_eq!(command.name, name_of("botvm", &owner, "inc1", 7));
    at("-i"); // 표준입력으로 요청을 준다
    // 마운트: 입력은 읽기 전용, scratch만 쓰기
    let mounts: Vec<&String> = a
        .iter()
        .zip(a.iter().skip(1))
        .filter(|(f, _)| *f == "--mount")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(mounts.len(), 2);
    assert!(mounts[0].ends_with("target=/bot,readonly"), "{mounts:?}");
    assert!(
        mounts[1].ends_with("target=/work") && !mounts[1].contains("readonly"),
        "{mounts:?}"
    );
    // 비밀은 이름만 명령줄에 있고 값은 클라이언트 환경으로만 간다. 릴레이는 호스트가 연 주소로 고정이다
    assert!(a.windows(2).any(|w| w == ["-e", "ANTHROPIC_AUTH_TOKEN"]));
    assert!(a.windows(2).any(|w| w == ["-e", "HOME=/work/home"]));
    for pair in [
        "BOT_RELAY_IP=192.168.99.1",
        "BOT_RELAY_PORT=8787",
        "ANTHROPIC_BASE_URL=http://192.168.99.1:8787",
    ] {
        assert_eq!(
            a.windows(2).filter(|w| w == &["-e", pair]).count(),
            1,
            "{pair}: {a:?}"
        );
    }
    assert!(
        !a.iter().any(|x| x.contains("sess-token-secret-1")),
        "{a:?}"
    );
    assert_eq!(
        command.secret_env,
        [(
            "ANTHROPIC_AUTH_TOKEN".to_owned(),
            "sess-token-secret-1".to_owned()
        )]
    );
    // 고정한 이미지(다이제스트) 뒤에 프로그램과 인자들이 끝까지 온다
    let tail: Vec<&str> = a[a.len() - 5..].iter().map(String::as_str).collect();
    assert_eq!(tail, [PINNED, "claude", "-p", "--model", "m"]);
    assert_eq!(ContainerSandbox::owner_of(&spec.attempt), owner);
}

#[test]
fn with_the_trusted_entry_the_model_gets_only_the_minimum_caps_and_the_anchor_a_fixed_non_root_wait()
 {
    let fake = Fake::new("entry");
    let config = fake.firewalled();
    let model = build(&config, &fake.spec(), &plan(Some(relay())))
        .unwrap()
        .args;
    let at = |flag: &str| model.iter().position(|x| x == flag).unwrap();
    // 모든 capability를 버린 뒤 init이 시작할 때 필요한 것만 더한다. 이미지의 고정 ENTRYPOINT를 쓰므로 진입을 덮지 않는다
    assert!(at("--cap-drop") < at("--cap-add"));
    let caps: Vec<&str> = model
        .windows(2)
        .filter(|w| w[0] == "--cap-add")
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(caps, ["NET_ADMIN", "SETPCAP"]);
    assert!(!model.contains(&"--entrypoint".to_owned()) && !model.contains(&"--user".to_owned()));
    // 이미지의 init은 `claude <인자…>`를 받는다(고정 Claude를 exec한다): 프로그램은 `claude`뿐이고 다른 이름은 시작하지 않는다
    let tail: Vec<&str> = model[model.len() - 5..]
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(tail, [PINNED, "claude", "-p", "--model", "m"]);
    for other in ["bash", "/usr/local/bin/claude", "sh"] {
        let mut spec = fake.spec();
        spec.program = other.into();
        assert_eq!(
            build(&config, &spec, &plan(Some(relay()))).unwrap_err(),
            Refused("program"),
            "{other}"
        );
    }
    // 닻: 별도 대기 진입·비root·capability 0·URL·마운트·입력 없음
    let anchor = build_anchor(&config, &plan(None)).unwrap().args;
    let at = |flag: &str| anchor.iter().position(|x| x == flag).unwrap();
    assert_eq!(anchor[at("--entrypoint") + 1], "/usr/local/bin/bot-wait");
    assert_eq!(anchor[at("--user") + 1], "10002:10002");
    assert_eq!(anchor[at("--cap-drop") + 1], "ALL");
    for flag in ["--cap-add", "-e", "--mount", "-i"] {
        assert!(!anchor.contains(&flag.to_owned()), "{flag}: {anchor:?}");
    }
    assert_eq!(anchor.last().unwrap(), PINNED);
    let label = format!("{OWNER_LABEL}={}", label_value(&anchor_owner(&config)));
    assert!(anchor.contains(&label), "{anchor:?}");
    // 방화벽 진입 없는(시험) 구성의 닻은 이미지의 `sleep`이다
    let plain = build_anchor(&fake.config(), &plan(None)).unwrap().args;
    assert_eq!(plain[plain.len() - 3..], [PINNED, "sleep", "2147483647"]);
}

#[test]
fn the_relay_address_is_fixed_by_the_host_and_the_spec_cannot_change_it() {
    let fake = Fake::new("relay");
    let with = |env: Vec<(&str, &str)>| {
        let mut spec = fake.spec();
        spec.env = env
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        build(&fake.config(), &spec, &plan(Some(relay())))
    };
    // 같은 주소(끝의 `/`는 같게 본다)는 한 번만 들어가고, 없으면 호스트가 넣는다
    for env in [
        vec![("ANTHROPIC_BASE_URL", "http://192.168.99.1:8787")],
        vec![("ANTHROPIC_BASE_URL", "http://192.168.99.1:8787/")],
        vec![],
    ] {
        let args = with(env).unwrap().args;
        let urls: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("ANTHROPIC_BASE_URL="))
            .collect();
        assert_eq!(urls, ["ANTHROPIC_BASE_URL=http://192.168.99.1:8787"]);
    }
    // 다른 주소·호스트 전용 이름은 거절한다
    for env in [
        vec![("ANTHROPIC_BASE_URL", "http://192.168.99.1:9999")],
        vec![("ANTHROPIC_BASE_URL", "http://10.0.0.1:8787")],
        vec![("ANTHROPIC_BASE_URL", "https://api.anthropic.com")],
        vec![("BOT_RELAY_IP", "10.0.0.1")],
        vec![("BOT_RELAY_PORT", "22")],
    ] {
        assert_eq!(with(env.clone()).unwrap_err(), Refused("relay"), "{env:?}");
    }
    // 릴레이가 없거나(열기 전) 포트가 0이거나 IPv6면 띄우지 않는다
    for bad in [
        None,
        Some("192.168.99.1:0".parse().unwrap()),
        Some("[::1]:8787".parse().unwrap()),
    ] {
        assert_eq!(
            build(&fake.config(), &fake.spec(), &plan(bad)).unwrap_err(),
            Refused("relay"),
            "{bad:?}"
        );
    }
}

#[test]
fn unsafe_specs_and_configs_are_refused_before_any_command_runs() {
    let fake = Fake::new("refuse");
    let check = |edit: &dyn Fn(&mut ModelSpec, &mut ContainerConfig), want: &'static str| {
        let (mut spec, mut config) = (fake.spec(), fake.config());
        edit(&mut spec, &mut config);
        assert_eq!(
            build(&config, &spec, &plan(Some(relay()))).unwrap_err(),
            Refused(want),
            "{want}"
        );
    };
    let outside =
        std::env::temp_dir().join(format!("bot-container-outside-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    check(
        &|s, _| s.readonly_mounts[0].host = outside.clone(),
        "mount_outside_root",
    );
    check(
        &|s, _| s.readonly_mounts[0].host = fake.root.join("missing"),
        "mount_missing",
    );
    let comma = fake.root.join("a,b");
    std::fs::create_dir_all(&comma).unwrap();
    check(
        &|s, _| s.readonly_mounts[0].host = comma.clone(),
        "mount_path",
    );
    // 링크로 루트 밖을 가리켜도 풀어서 거절한다
    let link = fake.root.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    check(
        &|s, _| s.readonly_mounts[0].host = link.clone(),
        "mount_outside_root",
    );
    check(&|s, _| s.cwd = "bot".into(), "cwd");
    check(&|s, _| s.cwd = "/bot/../etc".into(), "cwd");
    check(
        &|s, _| s.readonly_mounts[0].guest = "/work".into(),
        "mount_target",
    );
    check(
        &|s, _| s.readonly_mounts[0].guest = "/".into(),
        "mount_target",
    );
    check(&|s, _| s.env.push(("BAD NAME".into(), "x".into())), "env");
    check(&|s, _| s.env.push(("A=B".into(), "x".into())), "env");
    check(&|s, _| s.env.push(("HOME".into(), "dup".into())), "env");
    check(&|s, _| s.env.push(("OK".into(), "a\0b".into())), "env");
    check(&|s, _| s.program = "-rm".into(), "program");
    check(&|s, _| s.args.push("a\0b".into()), "args");
    check(&|_, c| c.image = "--privileged".into(), "image");
    check(&|_, c| c.image = "alpine 3".into(), "image");
    check(&|_, c| c.network = "net;rm".into(), "config");
    check(&|_, c| c.instance = "a/b".into(), "config");
    // 진입 설정: 닻은 비root, 권한 이름은 대문자 낱말, `ALL`은 안 된다
    for entry in [
        Entry {
            anchor: "bot-wait".into(),
            anchor_user: (10002, 10002),
            model_caps: vec![],
        },
        Entry {
            anchor: "/bin/wait".into(),
            anchor_user: (0, 10002),
            model_caps: vec![],
        },
        Entry {
            anchor: "/bin/wait".into(),
            anchor_user: (10002, 0),
            model_caps: vec![],
        },
        Entry {
            anchor: "/bin/wait".into(),
            anchor_user: (1, 1),
            model_caps: vec!["ALL".into()],
        },
        Entry {
            anchor: "/bin/wait".into(),
            anchor_user: (1, 1),
            model_caps: vec!["net_admin".into()],
        },
    ] {
        check(&|_, c| c.entry = Some(entry.clone()), "entry");
    }
    // 플래그처럼 보이는 값도 인자 자리에서는 값일 뿐이다(`-e` 뒤에 한 덩이로 간다)
    let mut spec = fake.spec();
    spec.env.push(("FLAGLIKE".into(), "--privileged".into()));
    let ok = build(&fake.config(), &spec, &plan(Some(relay()))).unwrap();
    assert!(
        ok.args
            .windows(2)
            .any(|w| w == ["-e", "FLAGLIKE=--privileged"])
    );
}

#[test]
fn only_exact_names_are_ours_and_images_are_pinned_by_digest() {
    // 접두사끼리 포함 관계여도 다른 어댑터의 이름은 걸리지 않는다
    for (prefix, id, ours) in [
        ("botvm", "botvm-0123456789ab-3", true),
        ("botvm", "botvm-secondary-0123456789ab-3", false),
        ("botvm", "botvm-0123456789ab", false),
        ("botvm", "botvm-0123456789ab-x", false),
        ("botvm", "botvm-0123456789ab-3-extra", false),
        ("botvm", "botvm-0123456789ab-3-4", false),
        ("botvm", "botvm-0123456789AB-3", false),
        ("botvm", "botvm-0123456789a-3", false),
        ("botvm", "botvm2-0123456789ab-3", false),
        ("botvm-secondary", "botvm-secondary-0123456789ab-3", true),
        ("botvm-secondary", "botvm-0123456789ab-3", false),
        ("botvm", "xbotvm-0123456789ab-3", false),
    ] {
        assert_eq!(is_ours(prefix, id), ours, "{prefix} / {id}");
    }
    let d = DIGEST;
    for (image, want) in [
        ("alpine:3.20", Some(format!("alpine@{d}"))),
        ("alpine", Some(format!("alpine@{d}"))),
        (
            "127.0.0.1:5088/bot-model-vm:latest",
            Some(format!("127.0.0.1:5088/bot-model-vm@{d}")),
        ),
        (
            "127.0.0.1:5088/bot-model-vm",
            Some(format!("127.0.0.1:5088/bot-model-vm@{d}")),
        ),
        (&format!("alpine@{d}"), Some(format!("alpine@{d}"))),
        (
            "alpine@sha256:0000000000000000000000000000000000000000000000000000000000000000",
            None,
        ),
    ] {
        assert_eq!(pin(image, d), want, "{image}");
    }
    for bad in ["sha256:abc", "md5:00", "sha256:ZZ", ""] {
        assert_eq!(pin("alpine:3.20", bad), None, "{bad}");
    }
}

#[test]
fn the_image_entry_preset_matches_the_published_contract_and_builds_both_vms() {
    let entry = Entry::model_image();
    assert_eq!(entry.anchor, "/usr/local/bin/bot-anchor");
    assert_eq!(entry.anchor_user, (10001, 10001));
    assert_eq!(
        entry.model_caps,
        ["NET_ADMIN", "SETGID", "SETPCAP", "SETUID"]
    );
    let fake = Fake::new("preset");
    let mut config = fake.config();
    config.unfirewalled = false;
    config.entry = Some(entry);
    assert!(build(&config, &fake.spec(), &plan(Some(relay()))).is_ok());
    assert!(build_anchor(&config, &plan(None)).is_ok());
}
