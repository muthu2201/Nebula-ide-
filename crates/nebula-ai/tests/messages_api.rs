//! End-to-end tests against a server that speaks the Messages API wire format.
//!
//! The unit tests in `anthropic.rs` cover request construction and response
//! parsing in isolation. These drive the whole path — reqwest, TLS-less HTTP,
//! chunked SSE delivery, error mapping — against a real socket, because the
//! parts most likely to break in production are the ones a unit test cannot
//! reach: a stream arriving in awkwardly-split chunks, a 429 with a provider
//! error body, an authorization header that never made it onto the request.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::StreamExt;
use nebula_ai::keys::ApiKey;
use nebula_ai::{
    AiError, AnthropicProvider, CompletionRequest, KeyStore, Message, Model, ModelProvider,
    Provider, StopReason, StreamEvent,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

/// What the fake API should do with the next request.
#[derive(Clone, Copy, PartialEq)]
enum Behaviour {
    /// Answer with a normal message.
    Text,
    /// Answer with a tool call.
    ToolUse,
    /// Answer with an SSE stream, delivered in small chunks.
    Stream,
    /// Answer with a rate-limit error.
    RateLimited,
    /// Answer with an authentication error.
    Unauthorized,
    /// Answer the token count endpoint.
    CountTokens,
}

#[derive(Clone)]
struct ServerState {
    behaviour: Arc<Mutex<Behaviour>>,
    /// Every request body the server received, for assertions.
    received: Arc<Mutex<Vec<(Value, HeaderMap)>>>,
}

async fn messages(State(state): State<ServerState>, headers: HeaderMap, body: String) -> Response {
    let parsed: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    state.received.lock().push((parsed.clone(), headers.clone()));

    // Authentication is checked exactly as the real API does, so a client that
    // forgets the header fails here rather than in production.
    let key = headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if key.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            json!({
                "type": "error",
                "error": { "type": "authentication_error", "message": "missing x-api-key" }
            })
            .to_string(),
        )
            .into_response();
    }

    let behaviour = *state.behaviour.lock();
    match behaviour {
        Behaviour::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            json!({
                "type": "error",
                "error": { "type": "authentication_error", "message": "invalid x-api-key" }
            })
            .to_string(),
        )
            .into_response(),

        Behaviour::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "type": "error",
                "error": { "type": "rate_limit_error", "message": "slow down" }
            })
            .to_string(),
        )
            .into_response(),

        Behaviour::Text => axum::Json(json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": parsed.get("model").cloned().unwrap_or(json!("claude-opus-5")),
            "content": [{ "type": "text", "text": "Hello from the API." }],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7,
                "cache_read_input_tokens": 1000,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 0,
                    "ephemeral_1h_input_tokens": 500
                }
            }
        }))
        .into_response(),

        Behaviour::ToolUse => axum::Json(json!({
            "id": "msg_02",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [
                { "type": "text", "text": "Reading the file." },
                {
                    "type": "tool_use",
                    "id": "toolu_01",
                    "name": "read_file",
                    "input": { "path": "src/main.rs" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        }))
        .into_response(),

        Behaviour::CountTokens => axum::Json(json!({ "input_tokens": 1234 })).into_response(),

        Behaviour::Stream => {
            // Deliver the stream in deliberately awkward chunks: an event split
            // mid-JSON, two events in one chunk. A decoder that assumes one
            // chunk is one event fails here.
            let pieces: Vec<&'static str> = vec![
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":15}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_de",
                "lta\",\"text\":\"Streaming \"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"works.\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ];

            let stream =
                futures::stream::iter(pieces.into_iter().map(|piece| {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(piece))
                }));

            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }
    }
}

/// Start the fake API and return its address plus the shared state.
async fn start_server() -> (SocketAddr, ServerState) {
    let state = ServerState {
        behaviour: Arc::new(Mutex::new(Behaviour::Text)),
        received: Arc::new(Mutex::new(Vec::new())),
    };

    let app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(messages))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, state)
}

fn provider_for(addr: SocketAddr) -> AnthropicProvider {
    let keys = KeyStore::memory();
    keys.set(Provider::Anthropic, &ApiKey::new("sk-ant-api03-integration-test-key")).unwrap();
    AnthropicProvider::with_base_url(keys, format!("http://{addr}")).unwrap()
}

#[tokio::test]
async fn a_complete_request_round_trips_over_http() {
    let (addr, state) = start_server().await;
    let provider = provider_for(addr);

    let request = CompletionRequest::new(Model::Opus5, vec![Message::user("Say hello")])
        .system("You are terse.");
    let response = provider.complete(request).await.unwrap();

    assert_eq!(response.text(), "Hello from the API.");
    assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(response.model, "claude-opus-5");

    // The cache accounting must survive the round trip, since it is what tells
    // a user whether their breakpoints are working.
    assert_eq!(response.usage.input_tokens, 42);
    assert_eq!(response.usage.cache_read_tokens, 1000);
    assert_eq!(response.usage.cache_write_1h_tokens, 500);
    assert!(response.usage.cache_hit_rate() > 0.6);

    let received = state.received.lock();
    let (body, headers) = received.last().unwrap();
    assert_eq!(body["model"], "claude-opus-5");
    assert_eq!(body["system"][0]["text"], "You are terse.");
    assert_eq!(
        headers.get("anthropic-version").unwrap(),
        "2023-06-01",
        "the version header is mandatory"
    );
    assert!(
        headers.get("x-api-key").is_some(),
        "the user's key must be on the request; it is what makes this BYOK"
    );
}

#[tokio::test]
async fn the_cost_of_a_response_can_be_estimated() {
    let (addr, _state) = start_server().await;
    let provider = provider_for(addr);

    let response = provider
        .complete(CompletionRequest::new(Model::Opus5, vec![Message::user("hi")]))
        .await
        .unwrap();

    let cost = Model::Opus5.estimate_cost(&response.usage);
    assert!(cost > 0.0);
    // Sanity: a handful of tokens must not cost dollars.
    assert!(cost < 0.01, "estimated {cost} for a tiny request");
}

#[tokio::test]
async fn a_tool_call_round_trips() {
    let (addr, state) = start_server().await;
    *state.behaviour.lock() = Behaviour::ToolUse;
    let provider = provider_for(addr);

    let response = provider
        .complete(CompletionRequest::new(Model::Opus5, vec![Message::user("read main.rs")]))
        .await
        .unwrap();

    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    let uses = response.tool_uses();
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].1, "read_file");
    assert_eq!(uses[0].2["path"], "src/main.rs");
}

#[tokio::test]
async fn a_stream_reassembles_correctly_from_awkward_chunks() {
    let (addr, state) = start_server().await;
    *state.behaviour.lock() = Behaviour::Stream;
    let provider = provider_for(addr);

    let mut stream = provider
        .stream(CompletionRequest::new(Model::Opus5, vec![Message::user("stream please")]))
        .await
        .unwrap();

    let mut text = String::new();
    let mut saw_start = false;
    let mut final_usage = None;

    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::Start { model } => {
                assert_eq!(model, "claude-opus-5");
                saw_start = true;
            }
            StreamEvent::TextDelta { text: delta } => text.push_str(&delta),
            StreamEvent::End { stop_reason, usage } => {
                if stop_reason.is_some() {
                    assert_eq!(stop_reason, Some(StopReason::EndTurn));
                    final_usage = Some(usage);
                }
            }
            _ => {}
        }
    }

    assert!(saw_start);
    assert_eq!(text, "Streaming works.", "an event split across two chunks must still reassemble");

    let usage = final_usage.expect("the stream should report final usage");
    assert_eq!(usage.input_tokens, 15, "input tokens come from message_start");
    assert_eq!(usage.output_tokens, 9, "output tokens are only final in message_delta");
}

#[tokio::test]
async fn a_rate_limit_is_typed_and_marked_retryable() {
    let (addr, state) = start_server().await;
    *state.behaviour.lock() = Behaviour::RateLimited;
    let provider = provider_for(addr);

    let err = provider
        .complete(CompletionRequest::new(Model::Opus5, vec![Message::user("hi")]))
        .await
        .unwrap_err();

    match &err {
        AiError::Api { status, message, error_type, .. } => {
            assert_eq!(*status, 429);
            assert_eq!(message, "slow down");
            assert_eq!(error_type.as_deref(), Some("rate_limit_error"));
        }
        other => panic!("expected an API error, got {other:?}"),
    }
    assert!(err.is_retryable(), "a rate limit is worth retrying");
    assert!(!err.is_auth_failure());
}

#[tokio::test]
async fn an_authentication_failure_is_not_retried() {
    let (addr, state) = start_server().await;
    *state.behaviour.lock() = Behaviour::Unauthorized;
    let provider = provider_for(addr);

    let err = provider
        .complete(CompletionRequest::new(Model::Opus5, vec![Message::user("hi")]))
        .await
        .unwrap_err();

    assert!(err.is_auth_failure());
    assert!(!err.is_retryable(), "retrying a bad key just burns time");
}

#[tokio::test]
async fn token_counting_uses_the_providers_own_tokeniser() {
    let (addr, state) = start_server().await;
    *state.behaviour.lock() = Behaviour::CountTokens;
    let provider = provider_for(addr);

    let request = CompletionRequest::new(Model::Opus5, vec![Message::user("count me")]);
    let count = provider.count_tokens(&request).await.unwrap();
    assert_eq!(count, 1234);

    // The counting endpoint rejects generation-only fields, so they must be
    // stripped before the request goes out.
    let received = state.received.lock();
    let (body, _) = received.last().unwrap();
    assert!(body.get("max_tokens").is_none(), "max_tokens must be stripped: {body}");
    assert!(body.get("stream").is_none());
    assert!(body.get("effort").is_none());
}

#[tokio::test]
async fn a_multi_turn_conversation_sends_the_whole_history() {
    let (addr, state) = start_server().await;
    let provider = provider_for(addr);

    let messages = vec![
        Message::user("first question"),
        Message::assistant("first answer"),
        Message::user("second question"),
    ];
    provider.complete(CompletionRequest::new(Model::Opus5, messages)).await.unwrap();

    let received = state.received.lock();
    let (body, _) = received.last().unwrap();
    let sent = body["messages"].as_array().unwrap();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0]["role"], "user");
    assert_eq!(sent[1]["role"], "assistant");
    assert_eq!(sent[2]["content"][0]["text"], "second question");
}

#[tokio::test]
async fn an_unreachable_endpoint_produces_a_retryable_network_error() {
    let keys = KeyStore::memory();
    keys.set(Provider::Anthropic, &ApiKey::new("sk-ant-api03-key-value-here")).unwrap();
    // Port 1 is reserved and nothing listens there.
    let provider = AnthropicProvider::with_base_url(keys, "http://127.0.0.1:1").unwrap();

    let err = tokio::time::timeout(
        Duration::from_secs(30),
        provider.complete(CompletionRequest::new(Model::Opus5, vec![Message::user("hi")])),
    )
    .await
    .expect("the request should fail quickly, not hang")
    .unwrap_err();

    assert!(matches!(err, AiError::Network { .. }), "{err:?}");
    assert!(err.is_retryable());
}
