use super::*;

fn identity() -> Identity {
    Identity::new(
        "0b9c1e2a-aaaa-bbbb-cccc-000000000001",
        1_700_000_000,
        "assistant",
    )
}

fn json_of(event: &str) -> Value {
    let line = event.strip_prefix("data: ").expect("data line");
    assert!(line.ends_with("\n\n"), "{event:?}");
    assert_eq!(
        line.matches('\n').count(),
        2,
        "single-line JSON plus blank line: {event:?}"
    );
    serde_json::from_str(line.trim_end()).unwrap()
}

#[test]
fn a_completion_has_the_official_shape_and_no_usage_unless_measured() {
    let body = completion(&identity(), "hello", Finish::Stop, None);
    assert_eq!(body["id"], "chatcmpl-0b9c1e2a-aaaa-bbbb-cccc-000000000001");
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["created"], 1_700_000_000);
    assert_eq!(body["model"], "assistant");
    let choice = &body["choices"][0];
    assert_eq!(choice["index"], 0);
    assert_eq!(choice["message"]["role"], "assistant");
    assert_eq!(choice["message"]["content"], "hello");
    assert!(choice["message"].get("refusal").is_some_and(Value::is_null));
    assert!(choice.get("logprobs").is_some_and(Value::is_null));
    assert_eq!(choice["finish_reason"], "stop");
    assert!(body.get("usage").is_none(), "{body}");
}

#[test]
fn measured_usage_sums_and_a_clipped_answer_says_length() {
    let usage = TokenUsage {
        prompt: 11,
        completion: 31,
    };
    let body = completion(&identity(), "x", Finish::Length, Some(usage));
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    assert_eq!(body["usage"]["prompt_tokens"], 11);
    assert_eq!(body["usage"]["completion_tokens"], 31);
    assert_eq!(body["usage"]["total_tokens"], 42);
}

#[test]
fn the_first_chunk_announces_the_role_with_an_empty_content() {
    let chunk = json_of(&first_chunk(&identity(), false));
    assert_eq!(chunk["object"], "chat.completion.chunk");
    assert_eq!(chunk["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunk["choices"][0]["delta"]["content"], "");
    assert!(chunk["choices"][0]["finish_reason"].is_null());
    assert!(chunk.get("usage").is_none(), "{chunk}");
}

#[test]
fn usage_appears_only_when_asked_for_and_only_the_last_chunk_carries_numbers() {
    let id = identity();
    let usage = TokenUsage {
        prompt: 1,
        completion: 2,
    };
    let plain: Vec<String> = [first_chunk(&id, false)]
        .into_iter()
        .chain(delta_chunks(&id, "abc", false))
        .chain([finish_chunk(&id, Finish::Stop, false)])
        .collect();
    assert!(plain.iter().all(|c| !c.contains("usage")), "{plain:?}");
    for c in [
        first_chunk(&id, true),
        delta_chunks(&id, "abc", true).remove(0),
        finish_chunk(&id, Finish::Stop, true),
    ] {
        let value = json_of(&c);
        assert!(
            value["usage"].is_null() && value.get("usage").is_some(),
            "{c}"
        );
    }
    let last = json_of(&usage_chunk(&id, usage));
    assert_eq!(last["choices"], json!([]));
    assert_eq!(last["usage"]["total_tokens"], 3);
}

#[test]
fn every_chunk_of_a_run_shares_id_created_and_model() {
    let id = identity();
    for c in [
        first_chunk(&id, false),
        delta_chunks(&id, "x", false).remove(0),
        finish_chunk(&id, Finish::Length, false),
        usage_chunk(
            &id,
            TokenUsage {
                prompt: 0,
                completion: 0,
            },
        ),
    ] {
        let v = json_of(&c);
        assert_eq!(v["id"], "chatcmpl-0b9c1e2a-aaaa-bbbb-cccc-000000000001");
        assert_eq!(v["created"], 1_700_000_000);
        assert_eq!(v["model"], "assistant");
    }
    let finish = json_of(&finish_chunk(&id, Finish::Length, false));
    assert_eq!(finish["choices"][0]["delta"], json!({}));
    assert_eq!(finish["choices"][0]["finish_reason"], "length");
}

#[test]
fn deltas_never_split_a_character_and_rebuild_the_text() {
    let text = "€★€★€★€★€★€★€★🙂🙂🙂🙂 e\u{301}x\n\"quote\"\\";
    for max in [4, 5, 7, 512] {
        let parts = pieces(text, max);
        assert!(parts.iter().all(|p| p.len() <= max.max(4) && !p.is_empty()));
        assert_eq!(parts.concat(), text);
    }
    let id = identity();
    let long = "★".repeat(3000);
    let chunks = delta_chunks(&id, &long, false);
    assert!(chunks.len() > 1);
    let rebuilt: String = chunks
        .iter()
        .map(|c| {
            json_of(c)["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(rebuilt, long);
}

#[test]
fn an_empty_text_has_no_pieces_and_a_tiny_limit_still_ends() {
    assert!(pieces("", 512).is_empty());
    assert_eq!(pieces("🙂🙂", 1).concat(), "🙂🙂");
}

#[test]
fn models_have_the_official_shape() {
    let list = model_list(vec![model("assistant", 1_700_000_000)]);
    assert_eq!(list["object"], "list");
    assert_eq!(
        list["data"][0],
        json!({"id": "assistant", "object": "model", "created": 1_700_000_000u64, "owned_by": "bot-framework"})
    );
    assert_eq!(
        model_list(Vec::new()),
        json!({"object": "list", "data": []})
    );
}

#[test]
fn fixed_notices_never_claim_a_result_and_name_the_recovery() {
    let pending = approval_pending();
    let running = still_running();
    assert!(pending.contains("Idempotency-Key"), "{pending}");
    assert!(running.contains("Idempotency-Key"), "{running}");
    assert_ne!(pending, running);
    for reason in ApprovalBlock::ALL {
        let text = blocked(reason);
        assert!(!text.trim().is_empty(), "{reason:?}");
        assert!(!text.contains(reason.code()), "{text}");
    }
    let all: std::collections::BTreeSet<String> =
        ApprovalBlock::ALL.into_iter().map(blocked).collect();
    assert_eq!(
        all.len(),
        ApprovalBlock::ALL.len(),
        "distinct notice per reason"
    );
}
