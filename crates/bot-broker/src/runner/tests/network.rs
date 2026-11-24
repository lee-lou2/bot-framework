//! 도구 서버의 외부 접속(egress 프록시)과 단계 원장 콜백 채널 시험.

use super::*;

/// 도구가 프록시에 `CONNECT`를 요청하고 상태 줄을 돌려주는 가짜 서버.
fn probing(targets: &'static [&'static str], with_credentials: bool) -> Server {
    Server::new(&[("delete", schema())]).answering(move |ctx| {
        let url = ctx
            .env
            .iter()
            .find(|(k, _)| k == "HTTPS_PROXY")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        Box::pin(async move {
            let rest = url.strip_prefix("http://").unwrap_or_default();
            let (auth, addr) = rest.split_once('@').unwrap_or(("", rest));
            let mut lines = Vec::new();
            for target in targets {
                let Ok(mut stream) = TcpStream::connect(addr).await else {
                    lines.push("no proxy".to_owned());
                    continue;
                };
                let header = if with_credentials {
                    format!(
                        "Proxy-Authorization: Basic {}\r\n",
                        crate::proxy::base64_encode(auth.as_bytes())
                    )
                } else {
                    String::new()
                };
                stream
                    .write_all(format!("CONNECT {target} HTTP/1.1\r\n{header}\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut status = String::new();
                BufReader::new(stream).read_line(&mut status).await.unwrap();
                lines.push(status.trim().to_owned());
            }
            Answer::Result(json!({"content": [{"type": "text", "text": lines.join(" | ")}]}))
        })
    })
}

fn statuses(ran: &Ran) -> Vec<String> {
    let payload: Value = serde_json::from_str(done_text(ran)).unwrap();
    payload["content"][0]["text"]
        .as_str()
        .unwrap()
        .split(" | ")
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn the_tool_can_only_leave_through_declared_hosts_within_the_operator_cap() {
    const TARGETS: &[&str] = &[
        "api.example:443",
        "files.example:443",
        "evil.example:443",
        "api.example:80",
        "rebind.example:443",
    ];
    let r = rig(
        probing(TARGETS, true),
        |_| {},
        |h| {
            h.egress_caps.insert(
                ProfileId("personal".into()),
                vec![
                    HostPort {
                        host: "api.example".into(),
                        port: 443,
                    },
                    HostPort {
                        host: "rebind.example".into(),
                        port: 443,
                    },
                ],
            );
        },
    );
    let tool = decl(
        &[],
        &["api.example:443", "files.example:443", "rebind.example:443"],
        false,
    );
    let ran = run(&r, job(tool, r#"{"event":"a"}"#)).await;
    let got = statuses(&ran);
    assert!(got[0].contains("200"), "선언 ∩ 상한: {got:?}");
    assert!(got[1].contains("403"), "선언했지만 운영자 상한 밖: {got:?}");
    assert!(got[2].contains("403"), "선언하지 않은 호스트: {got:?}");
    assert!(got[3].contains("403"), "선언하지 않은 포트: {got:?}");
    assert!(
        got[4].contains("403"),
        "이름이 사설 주소로 풀리면 거절: {got:?}"
    );
    assert_eq!(
        r.connector.addrs(),
        vec!["93.184.216.34:443".parse().unwrap()],
        "접속은 허용한 한 곳뿐"
    );
    // 샌드박스에는 프록시 하나뿐이고, 호출이 끝나면 닫힌다
    let spec = &r.sandbox.specs()[0];
    let addr = spec.egress.as_ref().expect("프록시").addr;
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == name && v.contains(&addr.to_string())),
            "{name}"
        );
    }
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn a_tool_cannot_use_the_proxy_without_the_per_call_credentials() {
    let r = rig(probing(&["api.example:443"], false), |_| {}, |_| {});
    let ran = run(
        &r,
        job(decl(&[], &["api.example:443"], false), r#"{"event":"a"}"#),
    )
    .await;
    assert!(statuses(&ran)[0].contains("407"));
    assert!(r.connector.addrs().is_empty());
}

#[tokio::test]
async fn no_declared_host_means_no_network_at_all() {
    let r = plain();
    run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
    let spec = &r.sandbox.specs()[0];
    assert!(spec.egress.is_none() && spec.env.is_empty());
    // 운영자 상한과 겹치는 곳이 없어도 마찬가지다
    let capped = rig(
        plain_server(),
        |_| {},
        |h| {
            h.egress_caps.insert(
                ProfileId("personal".into()),
                vec![HostPort {
                    host: "other.example".into(),
                    port: 443,
                }],
            );
        },
    );
    run(
        &capped,
        job(decl(&[], &["api.example:443"], false), r#"{"event":"a"}"#),
    )
    .await;
    assert!(capped.sandbox.specs()[0].egress.is_none());
}

fn plain_server() -> Server {
    Server::new(&[("delete", schema())])
}

#[tokio::test]
async fn multi_step_tools_get_a_callback_channel_that_closes_with_the_call() {
    let server = plain_server().answering(|ctx| {
        Box::pin(async move {
            let mut pipe = ctx.effects.expect("콜백 채널");
            let frame = r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary"}}"#;
            pipe.write.write_all(format!("{frame}\n").as_bytes()).await.unwrap();
            let mut reader = BufReader::new(pipe.read);
            let mut reply = String::new();
            reader.read_line(&mut reply).await.unwrap();
            Answer::Result(json!({"content": [{"type": "text", "text": reply.trim()}]}))
        })
    });
    let r = rig(server, |_| {}, |_| {});
    let ran = run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await;
    let payload: Value = serde_json::from_str(done_text(&ran)).unwrap();
    assert_eq!(
        payload["content"][0]["text"],
        r#"{"v":1,"id":"1","state":"started","effect":"e-1","send":true}"#
    );
    assert_eq!(r.journal.states().len(), 1);
    assert!(r.sandbox.specs()[0].effects);
    // 채널을 주지 않는 어댑터면 실행하지 않는다
    let r = rig(plain_server(), |_| {}, |_| {});
    r.sandbox.withhold_effects_pipe(true);
    assert!(matches!(
        run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await,
        Ran::NotSent
    ));
    assert!(!r.sandbox.methods().contains(&"tools/call".to_string()));
}

const UPLOAD_FRAME: &str =
    r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary"}}"#;
const LOOKUP_FRAME: &str =
    r#"{"v":1,"id":"2","method":"effect.status","step":"upload","args":{"file":"summary"}}"#;

#[tokio::test]
async fn a_step_lookup_through_the_pipe_channel_is_checked_against_the_calls_own_tool() {
    // 호출을 연 도구(`agenda.delete`)로 묶인 원장이면 조회가 읽고, 다른 도구로 묶였으면 원장이 거절한다(채널이 도구를 정한다)
    for (bound, expected) in [
        ("agenda.delete", r#""state":"started""#),
        ("agenda.other", r#""error":"denied""#),
    ] {
        let server = plain_server().answering(|ctx| {
            Box::pin(async move {
                let mut pipe = ctx.effects.expect("콜백 채널");
                let mut reader = BufReader::new(pipe.read);
                let mut seen = Vec::new();
                for frame in [UPLOAD_FRAME, LOOKUP_FRAME] {
                    pipe.write
                        .write_all(format!("{frame}\n").as_bytes())
                        .await
                        .unwrap();
                    let mut reply = String::new();
                    reader.read_line(&mut reply).await.unwrap();
                    seen.push(reply.trim().to_owned());
                }
                Answer::Result(json!({"content": [{"type": "text", "text": seen.join(" | ")}]}))
            })
        });
        let r = rig(server, |_| {}, |_| {});
        r.journal.bind_call("r1", "c1", bound);
        let ran = run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await;
        let seen = reply_text(&ran);
        let parts: Vec<&str> = seen.split(" | ").collect();
        assert!(parts[0].contains(r#""send":true"#), "{seen}");
        assert!(parts[1].contains(expected), "{bound}: {seen}");
        assert!(parts[1].contains(r#""id":"2""#), "{seen}");
        // 조회는 아무것도 바꾸지 않았다: 단계 하나가 시작 상태 그대로다
        assert_eq!(
            r.journal.states(),
            vec![(
                "upload".to_string(),
                bot_core::state::EffectState::Started,
                None
            )],
            "{bound}"
        );
    }
}

#[tokio::test]
async fn a_step_lookup_through_the_proxys_local_address_is_checked_against_the_calls_own_tool() {
    let frames: &'static [&'static str] = &[UPLOAD_FRAME, LOOKUP_FRAME];
    for (bound, expected) in [
        ("agenda.delete", r#""state":"started""#),
        ("agenda.other", "denied"),
    ] {
        let r = rig(effect_client(frames, true, "/effect"), |_| {}, |_| {});
        r.sandbox.set_effects_via_proxy(true);
        r.journal.bind_call("r1", "c1", bound);
        let ran = run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await;
        let seen = reply_text(&ran);
        let parts: Vec<&str> = seen.split(" | ").collect();
        assert!(parts[1].contains(expected), "{bound}: {seen}");
        assert!(!parts[1].contains(r#""send":true"#), "{seen}");
        assert_eq!(r.journal.step_count(), 1, "{bound}");
    }
}

/// 도구가 프록시의 전용 주소로 단계 원장 프레임을 보내고 응답 한 줄을 돌려받는 가짜 서버.
fn effect_client(frames: &'static [&'static str], auth_ok: bool, path: &'static str) -> Server {
    Server::new(&[("delete", schema())]).answering(move |ctx| {
        Box::pin(async move {
            let url = ctx.env.iter().find(|(k, _)| k == "HTTPS_PROXY").map(|(_, v)| v.clone()).unwrap_or_default();
            let target = ctx.env.iter().find(|(k, _)| k == "BOT_EFFECT_URL").map(|(_, v)| v.clone()).unwrap_or_default();
            let rest = url.strip_prefix("http://").unwrap_or_default();
            let (auth, addr) = rest.split_once('@').unwrap_or(("", rest));
            let mut seen = Vec::new();
            for frame in frames {
                let Ok(mut stream) = TcpStream::connect(addr).await else { seen.push("no proxy".to_owned()); continue };
                let header = if auth_ok { format!("Proxy-Authorization: Basic {}\r\n", crate::proxy::base64_encode(auth.as_bytes())) } else { String::new() };
                let target = target.replace("/effect", path);
                let request = format!("POST {target} HTTP/1.1\r\nHost: bot-effects.invalid\r\n{header}Content-Length: {}\r\n\r\n{frame}", frame.len());
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut all = String::new();
                let _ = stream.read_to_string(&mut all).await;
                let status = all.lines().next().unwrap_or("").to_owned();
                let body = all.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
                seen.push(format!("{status} {body}"));
            }
            Answer::Result(json!({"content": [{"type": "text", "text": seen.join(" | ")}]}))
        })
    })
}

fn reply_text(ran: &Ran) -> String {
    let payload: Value = serde_json::from_str(done_text(ran)).unwrap();
    payload["content"][0]["text"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_sandbox_without_fd_passing_reports_steps_through_the_proxys_local_address() {
    let frames: &'static [&'static str] = &[
        r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary"}}"#,
        r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"file-17"}"#,
        r#"{"v":1,"id":"3","method":"effect.request","step":"upload","args":{"file":"summary"}}"#,
    ];
    let r = rig(effect_client(frames, true, "/effect"), |_| {}, |_| {});
    r.sandbox.set_effects_via_proxy(true);
    let ran = run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await;
    let seen = reply_text(&ran);
    let parts: Vec<&str> = seen.split(" | ").collect();
    assert!(
        parts[0].contains("200")
            && parts[0].contains(r#""state":"started""#)
            && parts[0].contains(r#""send":true"#),
        "{seen}"
    );
    assert!(
        parts[1].contains(r#""state":"confirmed""#) && parts[1].contains("file-17"),
        "{seen}"
    );
    assert!(
        parts[2].contains(r#""state":"confirmed""#) && parts[2].contains(r#""send":false"#),
        "{seen}"
    );
    let spec = &r.sandbox.specs()[0];
    assert!(!spec.effects, "파이프 채널은 주지 않는다");
    assert!(spec.host_env.contains(&(
        "BOT_EFFECT_URL".into(),
        "http://bot-effects.invalid/effect".into()
    )));
    assert!(
        spec.egress_hosts
            .contains(&"bot-effects.invalid:80".to_owned()),
        "{:?}",
        spec.egress_hosts
    );
    assert_eq!(
        r.journal.states(),
        vec![(
            "upload".to_string(),
            bot_core::state::EffectState::Confirmed,
            Some("file-17".to_string())
        )]
    );
    assert!(
        r.connector.addrs().is_empty(),
        "전용 주소는 바깥으로 나가지 않는다"
    );
}

#[tokio::test]
async fn the_local_address_needs_the_per_call_credentials_and_only_serves_the_effect_path() {
    let frame: &'static [&'static str] =
        &[r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"x"}}"#];
    for (auth_ok, path, expect) in [
        (false, "/effect", "407"),
        (true, "/other", "404"),
        (true, "/effect/../x", "404"),
    ] {
        let r = rig(effect_client(frame, auth_ok, path), |_| {}, |_| {});
        r.sandbox.set_effects_via_proxy(true);
        let ran = run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await;
        assert!(
            reply_text(&ran).contains(expect),
            "{path}: {}",
            reply_text(&ran)
        );
        assert!(
            r.journal.states().is_empty(),
            "거절된 요청은 원장에 닿지 않는다"
        );
    }
    // 단계 원장을 쓰지 않는 도구에는 전용 주소도 환경 변수도 없다
    let r = rig(effect_client(frame, true, "/effect"), |_| {}, |_| {});
    r.sandbox.set_effects_via_proxy(true);
    run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
    assert!(r.sandbox.specs()[0].host_env.is_empty() && r.sandbox.specs()[0].egress.is_none());
}

#[tokio::test]
async fn a_stale_generation_cannot_report_through_the_local_address() {
    let frames: &'static [&'static str] =
        &[r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"x"}}"#];
    let r = rig(effect_client(frames, true, "/effect"), |_| {}, |_| {});
    r.sandbox.set_effects_via_proxy(true);
    r.journal.set_generation("r1", 2); // 워커가 이미 인계됐다
    let ran = run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await;
    assert!(
        reply_text(&ran).contains(r#""error":"stale""#),
        "{}",
        reply_text(&ran)
    );
    assert!(r.journal.states().is_empty());
}
