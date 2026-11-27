//! Effect-journal callbacks (pipe and proxy endpoints) check receipts and hold reasons against declared and proxy credentials.

use super::*;
use bot_core::state::EffectState;

const REQUEST: &str =
    r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary"}}"#;
const SECOND: &str =
    r#"{"v":1,"id":"3","method":"effect.request","step":"notify","args":{"to":"inbox"}}"#;
/// Embeds the store `password` secret (`pw1`) inside a reported receipt.
const LEAKY_RECEIPT: &str = r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"uploaded;pw1;done"}"#;
/// Short credentials like `pw1` match the hold-reason charset (`[a-z0-9_]`) and must still be blocked.
const LEAKY_REASON: &str =
    r#"{"v":1,"id":"4","method":"effect.status","effect":"e-2","outcome":"held","reason":"pw1"}"#;

fn needs_password() -> Tool {
    decl(&[("PASS", "password")], &[], true)
}

fn nothing_secret_is_stored(r: &Rig) {
    for (step, _, text) in r.journal.states() {
        assert!(!step.contains("pw1"), "{step}");
        assert!(!text.unwrap_or_default().contains("pw1"));
    }
}

fn states(r: &Rig) -> Vec<(String, EffectState, Option<String>)> {
    let mut states = r.journal.states();
    states.sort_by(|a, b| a.0.cmp(&b.0));
    states
}

fn expected_states() -> Vec<(String, EffectState, Option<String>)> {
    vec![
        (
            "notify".to_string(),
            EffectState::Held,
            Some("reason_credential".to_string()),
        ),
        (
            "upload".to_string(),
            EffectState::Held,
            Some("receipt_credential".to_string()),
        ),
    ]
}

#[tokio::test]
async fn the_pipe_channel_holds_a_receipt_and_a_reason_that_carry_a_declared_credential() {
    let server = Server::new(&[("delete", schema())]).answering(|ctx| {
        Box::pin(async move {
            let mut pipe = ctx.effects.expect("effects pipe");
            let mut reader = BufReader::new(pipe.read);
            let mut seen = Vec::new();
            for frame in [REQUEST, LEAKY_RECEIPT, SECOND, LEAKY_REASON] {
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
    let ran = run(&r, job(needs_password(), r#"{"event":"a"}"#)).await;
    let payload: Value = serde_json::from_str(done_text(&ran)).unwrap();
    let seen = payload["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(seen.contains(r#""receipt":"receipt_credential""#), "{seen}");
    assert!(seen.contains(r#""receipt":"reason_credential""#), "{seen}");
    assert!(
        !seen.contains("pw1"),
        "secret must not echo back to the tool: {seen}"
    );
    assert_eq!(states(&r), expected_states());
    nothing_secret_is_stored(&r);
}

fn proxy_client(frames: fn(&str) -> Vec<String>) -> Server {
    Server::new(&[("delete", schema())]).answering(move |ctx| {
        Box::pin(async move {
            let url = ctx.env.iter().find(|(k, _)| k == "HTTPS_PROXY").map(|(_, v)| v.clone()).unwrap_or_default();
            let target = ctx.env.iter().find(|(k, _)| k == "BOT_EFFECT_URL").map(|(_, v)| v.clone()).unwrap_or_default();
            let rest = url.strip_prefix("http://").unwrap_or_default();
            let (auth, addr) = rest.split_once('@').unwrap_or(("", rest));
            let password = auth.split_once(':').map_or("", |(_, pw)| pw).to_owned();
            let mut seen = Vec::new();
            for frame in frames(&password) {
                let Ok(mut stream) = TcpStream::connect(addr).await else { seen.push("no proxy".to_owned()); continue };
                let header = format!("Proxy-Authorization: Basic {}\r\n", crate::proxy::base64_encode(auth.as_bytes()));
                let request = format!("POST {target} HTTP/1.1\r\nHost: bot-effects.invalid\r\n{header}Content-Length: {}\r\n\r\n{frame}", frame.len());
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut all = String::new();
                let _ = stream.read_to_string(&mut all).await;
                seen.push(all.split("\r\n\r\n").nth(1).unwrap_or("").to_owned());
            }
            Answer::Result(json!({"content": [{"type": "text", "text": seen.join(" | ")}]}))
        })
    })
}

fn declared_credential_frames(_: &str) -> Vec<String> {
    [REQUEST, LEAKY_RECEIPT, SECOND, LEAKY_REASON]
        .map(str::to_owned)
        .to_vec()
}

fn proxy_password_frames(password: &str) -> Vec<String> {
    vec![
        REQUEST.to_owned(),
        format!(
            r#"{{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"proxy={password}"}}"#
        ),
    ]
}

#[tokio::test]
async fn the_proxy_channel_holds_a_receipt_and_a_reason_that_carry_a_declared_credential() {
    let r = rig(proxy_client(declared_credential_frames), |_| {}, |_| {});
    r.sandbox.set_effects_via_proxy(true);
    let ran = run(&r, job(needs_password(), r#"{"event":"a"}"#)).await;
    let payload: Value = serde_json::from_str(done_text(&ran)).unwrap();
    let seen = payload["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(seen.contains(r#""receipt":"receipt_credential""#), "{seen}");
    assert!(seen.contains(r#""receipt":"reason_credential""#), "{seen}");
    assert!(!seen.contains("pw1"), "{seen}");
    assert_eq!(states(&r), expected_states());
    nothing_secret_is_stored(&r);
}

#[tokio::test]
async fn the_proxy_password_is_one_of_the_credentials_a_report_may_not_carry() {
    let r = rig(proxy_client(proxy_password_frames), |_| {}, |_| {});
    r.sandbox.set_effects_via_proxy(true);
    let ran = run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await;
    let payload: Value = serde_json::from_str(done_text(&ran)).unwrap();
    let seen = payload["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(seen.contains(r#""receipt":"receipt_credential""#), "{seen}");
    assert_eq!(
        r.journal.states(),
        vec![(
            "upload".to_string(),
            EffectState::Held,
            Some("receipt_credential".to_string())
        )]
    );
    // Clean receipts through the same proxy path are stored normally.
    let r = rig(
        proxy_client(|_| {
            vec![
                REQUEST.to_owned(),
                r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"file-17"}"#.to_owned(),
            ]
        }),
        |_| {},
        |_| {},
    );
    r.sandbox.set_effects_via_proxy(true);
    run(&r, job(decl(&[], &[], true), r#"{"event":"a"}"#)).await;
    assert_eq!(
        r.journal.states(),
        vec![(
            "upload".to_string(),
            EffectState::Confirmed,
            Some("file-17".to_string())
        )]
    );
}
