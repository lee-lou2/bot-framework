//! Mock-server unit tests for `MemoryJudgeClient` (Liquid `d1.2-ultra`, TypeSafe Jev `422` fallback, secret filtering, and stale gates).
use super::*;
use bot_core::port::{ProfileSecrets, SecretValue};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct StaticSecrets(String);
impl ProfileSecrets for StaticSecrets {
    fn resolve(&self, _: SecretName) -> Reply<'_, SecretValue> {
        let v = self.0.as_bytes().to_vec();
        Box::pin(async move { Ok(SecretValue(v)) })
    }
}
impl SecretStore for StaticSecrets {
    fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let s = self.0.clone();
        Box::pin(async move { Ok(Box::new(StaticSecrets(s)) as Box<dyn ProfileSecrets>) })
    }
}

struct Gate(bool);
impl ActiveCheck for Gate {
    fn verify(&self) -> Reply<'_, ()> {
        let ok = self.0;
        Box::pin(async move { if ok { Ok(()) } else { Err(Error::Stale) } })
    }
}

async fn mock_sequence(
    responses: Vec<(u16, String)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        for (status, body) in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap();
            seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        }
        seen
    });
    (
        format!("http://127.0.0.1:{}/decisions", addr.port()),
        handle,
    )
}

fn client_for(base: &str, active: bool) -> MemoryJudgeClient {
    MemoryJudgeClient::new(
        MemoryJudgeSettings {
            endpoint: ApiEndpoint::parse(base).unwrap(),
            model: "d1.2-ultra".into(),
            secret: Some(SecretName::parse("liquid-api-key").unwrap()),
        },
        Arc::new(StaticSecrets("liquid_test_token_123456".into())),
        Arc::new(Gate(active)),
    )
    .unwrap()
}

#[tokio::test]
async fn judges_save_with_sentence_expansion_and_supersede() {
    let ok_body = json!({
        "answers": {
            "action": { "type": "choice", "choice": "save", "probabilities": { "save": 0.98, "none": 0.02 }, "confidence": 0.96 },
            "contains_secret": { "type": "noul", "noul": 0.002 },
            "kind": { "type": "choice", "choice": "fact", "confidence": 0.95 },
            "target_memory": { "type": "choice", "choice": "m0", "probabilities": { "m0": 0.99, "none": 0.01 }, "confidence": 0.98 },
            "memorable_spans": { "type": "spans", "spans": [{ "text": "다음 주부터 판교", "found": 0.99 }] }
        }
    })
    .to_string();
    let (base, server) = mock_sequence(vec![(200, ok_body)]).await;
    let client = client_for(&base, true);
    let decision = client
        .judge(
            &ProfileId("personal".into()),
            "아 맞다 나 다음 주부터 판교로 이사해! 근데 오늘 날씨 어때?",
            &[MemoryCandidate {
                id: MemoryId("mem_1".into()),
                text: "강남역 근처에 산다.".into(),
            }],
        )
        .await
        .unwrap();
    let reqs = server.await.unwrap();
    assert!(reqs[0].contains("POST /decisions/v1/systemone HTTP/1.1"));
    assert!(reqs[0].contains("authorization: Bearer liquid_test_token_123456"));
    assert_eq!(
        decision,
        MemoryDecision::Save {
            items: vec![MemoryDecisionItem {
                text: "아 맞다 나 다음 주부터 판교로 이사해!".into(),
                kind: MemoryKind::Fact,
            }],
            supersedes: Some(MemoryId("mem_1".into())),
        }
    );
}

#[tokio::test]
async fn retries_without_spans_on_422_for_typesafe_jev_compatibility_and_handles_forget() {
    let fallback_ok = json!({
        "answers": {
            "action": { "type": "choice", "choice": "forget", "probabilities": { "forget": 0.97 }, "confidence": 0.97 },
            "contains_secret": { "type": "noul", "noul": 0.001 },
            "kind": { "type": "choice", "choice": "fact" },
            "target_memory": { "type": "choice", "choice": "m0", "probabilities": { "m0": 0.96 }, "confidence": 0.96 }
        }
    })
    .to_string();
    let (base, server) = mock_sequence(vec![
        (422, r#"{"error":"spans unsupported"}"#.into()),
        (200, fallback_ok),
    ])
    .await;
    let client = client_for(&format!("{base}/v1/systemone"), true);
    let decision = client
        .judge(
            &ProfileId("personal".into()),
            "클라이밍 일정은 잊어줘",
            &[MemoryCandidate {
                id: MemoryId("mem_3".into()),
                text: "수요일 클라이밍".into(),
            }],
        )
        .await
        .unwrap();
    let reqs = server.await.unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].contains("memorable_spans"));
    assert!(!reqs[1].contains("memorable_spans"));
    assert_eq!(decision, MemoryDecision::Forget(MemoryId("mem_3".into())));
}

#[tokio::test]
async fn blocks_credentials_locally_and_rejects_secret_leak_or_stale_gate() {
    let client = client_for("http://127.0.0.1:1/decisions", true);
    let skipped = client
        .judge(
            &ProfileId("personal".into()),
            "My key is sk-proj-1234567890abcdefghij",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(skipped, MemoryDecision::None);

    let leaked = json!({"answers": {"note": "liquid_test_token_123456"}}).to_string();
    let (base, _) = mock_sequence(vec![(200, leaked)]).await;
    assert!(matches!(
        client_for(&base, true)
            .judge(&ProfileId("personal".into()), "I prefer oat milk", &[])
            .await,
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        client_for(&base, false)
            .judge(&ProfileId("personal".into()), "I prefer oat milk", &[])
            .await,
        Err(Error::Stale)
    ));
}
