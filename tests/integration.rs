//! Integration tests for the `req` module.
//!
//! Two tiers:
//! - **Pure tests** (no network) exercise request serialization, response
//!   deserialization, and local validation. They run on every `cargo test`.
//! - **Live tests** hit the real DeepSeek API and are marked `#[ignore]`, so
//!   they only run when explicitly requested (e.g. `just test-live`). They need
//!   `DEEPSEEK_API_KEY` in the environment and exercise both client backends.

use ai_write::req::{
    ChatRequest, ChatResponse, Effort, Error, FinishReason, Model, ResponseFormat, Thinking,
};

// ---------------------------------------------------------------------------
// Pure tests (no network)
// ---------------------------------------------------------------------------

#[test]
fn request_serializes_expected_shape() {
    let req = ChatRequest::builder(Model::V4Flash)
        .system("sys")
        .user("hi")
        .thinking(Thinking::Disabled)
        .max_tokens(100)
        .response_format(ResponseFormat::JsonObject)
        .build()
        .unwrap();
    let v = serde_json::to_value(&req).unwrap();

    assert_eq!(v["model"], "deepseek-v4-flash");
    assert_eq!(v["thinking"]["type"], "disabled");
    assert_eq!(v["response_format"]["type"], "json_object");
    assert_eq!(v["messages"][0]["role"], "system");
    assert_eq!(v["messages"][1]["role"], "user");
    assert_eq!(v["messages"][1]["content"], "hi");
    // Unset optional fields must not appear on the wire.
    assert!(v.get("stream").is_none());
    assert!(v.get("temperature").is_none());
    assert!(v["messages"][0].get("name").is_none());
}

#[test]
fn thinking_enabled_serializes_effort() {
    let req = ChatRequest::builder(Model::V4Pro)
        .user("x")
        .thinking(Thinking::Enabled {
            effort: Effort::Max,
        })
        .build()
        .unwrap();
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(v["model"], "deepseek-v4-pro");
    assert_eq!(v["thinking"]["type"], "enabled");
    assert_eq!(v["thinking"]["reasoning_effort"], "max");
}

#[test]
fn parses_chat_response() {
    let json = r#"{
        "id": "abc",
        "object": "chat.completion",
        "created": 1781679415,
        "model": "deepseek-v4-flash",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "你好"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 9, "completion_tokens": 1, "total_tokens": 10,
            "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": 9
        }
    }"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.content(), Some("你好"));
    assert_eq!(resp.finish_reason(), Some(&FinishReason::Stop));
    assert_eq!(resp.usage.unwrap().total_tokens, 10);
}

#[test]
fn unknown_finish_reason_is_preserved() {
    let json = r#"{
        "id": "x", "object": "chat.completion", "created": 1, "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": null},
                     "finish_reason": "some_new_reason"}]
    }"#;
    let resp: ChatResponse = serde_json::from_str(json).unwrap();
    match resp.finish_reason() {
        Some(FinishReason::Unknown(s)) => assert_eq!(s, "some_new_reason"),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn validate_rejects_logprobs_in_thinking_mode() {
    let err = ChatRequest::builder(Model::V4Flash)
        .user("x")
        .thinking(Thinking::Enabled {
            effort: Effort::High,
        })
        .logprobs(true)
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)), "got {err:?}");
}

#[test]
fn validate_rejects_empty_messages() {
    let err = ChatRequest::builder(Model::V4Flash).build().unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Live tests (network + DEEPSEEK_API_KEY; run with `--ignored`)
// ---------------------------------------------------------------------------

#[cfg(feature = "blocking")]
#[test]
#[ignore = "hits the live DeepSeek API; needs DEEPSEEK_API_KEY"]
fn live_blocking() {
    use ai_write::req::blocking::Client;

    let client = Client::from_env().expect("DEEPSEEK_API_KEY must be set");

    let models = client.list_models().unwrap();
    assert!(models.iter().any(|m| m.id == "deepseek-v4-flash"));
    assert!(client.balance().unwrap().is_available);

    let req = ChatRequest::builder(Model::V4Flash)
        .user("用三个字打个招呼")
        .thinking(Thinking::Disabled)
        .max_tokens(32)
        .build()
        .unwrap();
    let resp = client.chat(&req).unwrap();
    assert!(resp.content().is_some());

    let req = ChatRequest::builder(Model::V4Flash)
        .user("从 1 数到 5,用空格分隔")
        .thinking(Thinking::Disabled)
        .max_tokens(64)
        .build()
        .unwrap();
    let mut text = String::new();
    let mut usage = None;
    for chunk in client.chat_stream(&req).unwrap() {
        let chunk = chunk.unwrap();
        if let Some(piece) = chunk.delta_content() {
            text.push_str(piece);
        }
        if chunk.usage.is_some() {
            usage = chunk.usage.clone();
        }
    }
    assert!(!text.is_empty(), "stream produced no content");
    assert!(usage.is_some(), "stream produced no final usage");
}

#[cfg(feature = "async")]
#[tokio::test]
#[ignore = "hits the live DeepSeek API; needs DEEPSEEK_API_KEY"]
async fn live_async() {
    use ai_write::req::Client;
    use futures_util::StreamExt;

    let client = Client::from_env().expect("DEEPSEEK_API_KEY must be set");

    let models = client.list_models().await.unwrap();
    assert!(models.iter().any(|m| m.id == "deepseek-v4-flash"));
    assert!(client.balance().await.unwrap().is_available);

    let req = ChatRequest::builder(Model::V4Flash)
        .user("用三个字打个招呼")
        .thinking(Thinking::Disabled)
        .max_tokens(32)
        .build()
        .unwrap();
    assert!(client.chat(&req).await.unwrap().content().is_some());

    let req = ChatRequest::builder(Model::V4Flash)
        .user("从 1 数到 5,用空格分隔")
        .thinking(Thinking::Disabled)
        .max_tokens(64)
        .build()
        .unwrap();
    let mut stream = client.chat_stream(&req).await.unwrap();
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        if let Some(piece) = chunk.unwrap().delta_content() {
            text.push_str(piece);
        }
    }
    assert!(!text.is_empty(), "stream produced no content");
}
