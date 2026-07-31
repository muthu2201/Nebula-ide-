//! The Anthropic Messages API adapter.
//!
//! `POST /v1/messages` with `anthropic-version: 2023-06-01` and the user's own
//! `x-api-key`, sent from the user's machine straight to `api.anthropic.com`.
//!
//! There is deliberately no fill-in-the-middle path here. The Messages API is
//! chat-only — the legacy `/v1/complete` endpoint is deprecated and rejects
//! Claude 3 and later — so inline completion is served by the local model
//! instead. That is a fact about the API, not a design preference.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use crate::cache::CacheTtl;
use crate::keys::{KeyStore, Provider};
use crate::models::Model;
use crate::provider::{
    CompletionRequest, CompletionResponse, ContentBlock, Message, ModelProvider, Role, StopReason,
    StreamEvent, Usage,
};
use crate::{AiError, Result};

/// The API version header value. Fixed by Anthropic, not a Nebula choice.
pub const API_VERSION: &str = "2023-06-01";

/// The default API base.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The Anthropic provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    keys: KeyStore,
}

impl AnthropicProvider {
    /// A provider reading keys from `keys`.
    pub fn new(keys: KeyStore) -> Result<Self> {
        Self::with_base_url(keys, DEFAULT_BASE_URL)
    }

    /// A provider pointed at a custom base URL.
    ///
    /// Used by the tests, which run a server implementing the Messages API wire
    /// format so that the real HTTP and SSE code paths are exercised rather than
    /// bypassed.
    pub fn with_base_url(keys: KeyStore, base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            // Long, because a large agentic request with high effort genuinely
            // takes minutes. The caller cancels; the transport does not
            // second-guess.
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(30))
            .user_agent(concat!("nebula-ide/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AiError::Network {
                provider: "anthropic".to_string(),
                message: format!("building http client: {e}"),
            })?;

        Ok(Self { client, base_url: base_url.into().trim_end_matches('/').to_string(), keys })
    }

    /// Build the JSON body for a request.
    ///
    /// Public so the shape can be asserted in tests without a network round
    /// trip — the per-model quirks encoded here are exactly the kind of thing
    /// that silently regresses.
    pub fn build_body(&self, request: &CompletionRequest, stream: bool) -> Value {
        let info = request.model.info();
        let mut body = json!({
            "model": info.id,
            "max_tokens": request.max_tokens.min(info.max_output),
            "messages": request.messages.iter().map(encode_message).collect::<Vec<_>>(),
        });

        if stream {
            body["stream"] = json!(true);
        }

        if let Some(system) = &request.system {
            // The system prompt is sent as a content-block array rather than a
            // bare string, because only the array form can carry a
            // `cache_control` marker.
            let mut block = json!({ "type": "text", "text": system });
            if let Some(ttl) = request.cache_system {
                block["cache_control"] = ttl.to_json();
            }
            body["system"] = json!([block]);
        }

        if !request.tools.is_empty() {
            let mut tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema,
                    })
                })
                .collect();
            // A breakpoint on the last tool caches the whole definition block,
            // which is stable for the life of a session.
            if request.cache_system.is_some()
                && let Some(last) = tools.last_mut()
            {
                last["cache_control"] = CacheTtl::OneHour.to_json();
            }
            body["tools"] = json!(tools);
        }

        if !request.stop_sequences.is_empty() {
            body["stop_sequences"] = json!(request.stop_sequences);
        }

        if info.supports_effort
            && let Some(effort) = request.effort
        {
            body["effort"] = json!(effort.as_str());
        }

        // Opus 4.7 and later reject temperature/top_p/top_k with a 400 that
        // fails the entire request. Silently dropping the parameter is the only
        // behaviour that does not break the caller.
        if info.accepts_sampling_params
            && let Some(temperature) = request.temperature
        {
            body["temperature"] = json!(temperature);
        } else if request.temperature.is_some() {
            tracing::debug!(
                model = info.id,
                "dropping temperature: this model rejects sampling parameters"
            );
        }

        body
    }

    fn post(&self, path: &str, key: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base_url))
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
    }

    async fn key(&self) -> Result<crate::keys::ApiKey> {
        self.keys.get(Provider::Anthropic)
    }

    /// Turn a non-2xx response into a typed error.
    async fn error_from(&self, response: reqwest::Response) -> AiError {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();

        // Anthropic errors look like {"type":"error","error":{"type":..,"message":..}}
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
        let (message, error_type) = parsed
            .as_ref()
            .and_then(|v| v.get("error"))
            .map(|e| {
                (
                    e.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
                    e.get("type").and_then(|t| t.as_str()).map(str::to_string),
                )
            })
            .unwrap_or_else(|| (body.chars().take(400).collect(), None));

        AiError::Api {
            provider: "anthropic".to_string(),
            status,
            message: if message.is_empty() { "no message".to_string() } else { message },
            error_type,
        }
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let key = self.key().await?;
        let body = self.build_body(&request, false);

        let response = self
            .post("/v1/messages", key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Network {
                provider: "anthropic".to_string(),
                message: e.to_string(),
            })?;

        if !response.status().is_success() {
            return Err(self.error_from(response).await);
        }

        let value: Value = response.json().await.map_err(|e| AiError::Protocol {
            provider: "anthropic".to_string(),
            detail: format!("response was not JSON: {e}"),
        })?;

        parse_response(&value)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let key = self.key().await?;
        let body = self.build_body(&request, true);

        let response = self
            .post("/v1/messages", key.expose())
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Network {
                provider: "anthropic".to_string(),
                message: e.to_string(),
            })?;

        if !response.status().is_success() {
            return Err(self.error_from(response).await);
        }

        let mut bytes = response.bytes_stream();
        let stream = async_stream::stream(move |mut yielder| async move {
            let mut buffer = String::new();
            let mut usage = Usage::default();

            while let Some(chunk) = bytes.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(e) => {
                        yielder
                            .send(Err(AiError::Network {
                                provider: "anthropic".to_string(),
                                message: e.to_string(),
                            }))
                            .await;
                        return;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // SSE events are separated by a blank line. Anything after the
                // last separator is an incomplete event and stays buffered.
                while let Some(split) = buffer.find("\n\n") {
                    let event = buffer[..split].to_string();
                    buffer.drain(..split + 2);

                    for parsed in parse_sse_event(&event, &mut usage) {
                        yielder.send(Ok(parsed)).await;
                    }
                }
            }
        });

        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, request: &CompletionRequest) -> Result<usize> {
        let key = self.key().await?;
        let mut body = self.build_body(request, false);
        // The counting endpoint rejects generation-only fields.
        if let Some(object) = body.as_object_mut() {
            object.remove("max_tokens");
            object.remove("stream");
            object.remove("effort");
            object.remove("temperature");
            object.remove("stop_sequences");
        }

        let response = self
            .post("/v1/messages/count_tokens", key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Network {
                provider: "anthropic".to_string(),
                message: e.to_string(),
            })?;

        if !response.status().is_success() {
            return Err(self.error_from(response).await);
        }

        let value: Value = response.json().await.map_err(|e| AiError::Protocol {
            provider: "anthropic".to_string(),
            detail: format!("response was not JSON: {e}"),
        })?;

        value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .ok_or_else(|| AiError::Protocol {
                provider: "anthropic".to_string(),
                detail: "count_tokens response had no input_tokens".to_string(),
            })
    }

    async fn is_configured(&self) -> bool {
        self.keys.has(Provider::Anthropic)
    }
}

/// Encode one message for the wire.
fn encode_message(message: &Message) -> Value {
    let mut content: Vec<Value> =
        message.content.iter().map(|block| serde_json::to_value(block).unwrap_or(json!({}))).collect();

    // A cache breakpoint attaches to the *last* block of the message, marking
    // everything up to and including it as cacheable.
    if let Some(ttl) = message.cache
        && let Some(last) = content.last_mut()
        && let Some(object) = last.as_object_mut()
    {
        object.insert("cache_control".to_string(), ttl.to_json());
    }

    json!({
        "role": match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        "content": content,
    })
}

/// Parse a complete (non-streaming) response.
fn parse_response(value: &Value) -> Result<CompletionResponse> {
    let content: Vec<ContentBlock> = value
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| blocks.iter().filter_map(parse_content_block).collect())
        .unwrap_or_default();

    let stop_reason = value
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .and_then(parse_stop_reason);

    Ok(CompletionResponse {
        content,
        stop_reason,
        usage: parse_usage(value.get("usage")),
        model: value.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
}

fn parse_content_block(value: &Value) -> Option<ContentBlock> {
    match value.get("type")?.as_str()? {
        "text" => Some(ContentBlock::Text {
            text: value.get("text")?.as_str()?.to_string(),
        }),
        "tool_use" => Some(ContentBlock::ToolUse {
            id: value.get("id")?.as_str()?.to_string(),
            name: value.get("name")?.as_str()?.to_string(),
            input: value.get("input").cloned().unwrap_or_else(|| json!({})),
        }),
        "thinking" => Some(ContentBlock::Thinking {
            thinking: value.get("thinking")?.as_str()?.to_string(),
            signature: value.get("signature").and_then(|s| s.as_str()).map(str::to_string),
        }),
        // An unknown block type from a newer API revision is skipped rather than
        // failing the whole response.
        other => {
            tracing::debug!(block_type = other, "skipping unrecognised content block");
            None
        }
    }
}

fn parse_stop_reason(value: &str) -> Option<StopReason> {
    Some(match value {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        _ => return None,
    })
}

/// Read the `usage` object, including the cache counters.
fn parse_usage(value: Option<&Value>) -> Usage {
    let Some(usage) = value else {
        return Usage::default();
    };
    let read = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    // The creation counters are nested by TTL when both are used; the flat
    // `cache_creation_input_tokens` is the total. Prefer the breakdown, since
    // the two TTLs bill differently.
    let creation = usage.get("cache_creation");
    let (write_5m, write_1h) = match creation {
        Some(object) => (
            object.get("ephemeral_5m_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            object.get("ephemeral_1h_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        ),
        None => (read("cache_creation_input_tokens"), 0),
    };

    Usage {
        input_tokens: read("input_tokens"),
        output_tokens: read("output_tokens"),
        cache_read_tokens: read("cache_read_input_tokens"),
        cache_write_5m_tokens: write_5m,
        cache_write_1h_tokens: write_1h,
    }
}

/// Parse one SSE event block into zero or more stream events.
fn parse_sse_event(event: &str, usage: &mut Usage) -> Vec<StreamEvent> {
    let mut data = String::new();
    for line in event.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            data.push_str(payload.trim());
        }
    }
    if data.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return Vec::new();
    };

    let Some(event_type) = value.get("type").and_then(|t| t.as_str()) else {
        return Vec::new();
    };

    match event_type {
        "message_start" => {
            if let Some(message) = value.get("message") {
                *usage = parse_usage(message.get("usage"));
                return vec![StreamEvent::Start {
                    model: message
                        .get("model")
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string(),
                }];
            }
            Vec::new()
        }
        "content_block_start" => {
            let block = value.get("content_block");
            match block.and_then(|b| b.get("type")).and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let block = block.unwrap();
                    vec![StreamEvent::ToolUseStart {
                        id: block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        name: block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    }]
                }
                _ => Vec::new(),
            }
        }
        "content_block_delta" => {
            let Some(delta) = value.get("delta") else {
                return Vec::new();
            };
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => delta
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|text| vec![StreamEvent::TextDelta { text: text.to_string() }])
                    .unwrap_or_default(),
                Some("thinking_delta") => delta
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .map(|thinking| {
                        vec![StreamEvent::ThinkingDelta { thinking: thinking.to_string() }]
                    })
                    .unwrap_or_default(),
                Some("input_json_delta") => delta
                    .get("partial_json")
                    .and_then(|t| t.as_str())
                    .map(|partial_json| {
                        vec![StreamEvent::ToolInputDelta { partial_json: partial_json.to_string() }]
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            vec![StreamEvent::BlockEnd { index }]
        }
        "message_delta" => {
            // Output tokens are only final here, so the running total is
            // updated rather than replaced.
            if let Some(delta_usage) = value.get("usage")
                && let Some(output) = delta_usage.get("output_tokens").and_then(|v| v.as_u64())
            {
                usage.output_tokens = output as usize;
            }
            let stop_reason = value
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|s| s.as_str())
                .and_then(parse_stop_reason);
            if stop_reason.is_some() {
                return vec![StreamEvent::End { stop_reason, usage: *usage }];
            }
            Vec::new()
        }
        "message_stop" => vec![StreamEvent::End { stop_reason: None, usage: *usage }],
        // `ping` and `error` events carry no content the caller needs; an error
        // event is followed by the stream closing.
        _ => Vec::new(),
    }
}

/// A minimal async-stream helper.
///
/// `async-stream` would pull in a proc-macro dependency for one generator; this
/// is the same thing in twenty lines, using a channel.
mod async_stream {
    use futures::Stream;
    use tokio::sync::mpsc;

    /// A handle for sending items out of the generator.
    pub struct Yielder<T> {
        sender: mpsc::Sender<T>,
    }

    impl<T> Yielder<T> {
        /// Emit an item. Returns once the consumer has room.
        pub async fn send(&mut self, item: T) {
            // A closed receiver means the consumer dropped the stream, which is
            // a normal cancellation.
            let _ = self.sender.send(item).await;
        }
    }

    /// Build a stream from an async generator function.
    pub fn stream<T, F, Fut>(generator: F) -> impl Stream<Item = T>
    where
        T: Send + 'static,
        F: FnOnce(Yielder<T>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(async move {
            generator(Yielder { sender }).await;
        });
        tokio_stream_wrapper(receiver)
    }

    fn tokio_stream_wrapper<T>(mut receiver: mpsc::Receiver<T>) -> impl Stream<Item = T> {
        futures::stream::poll_fn(move |cx| receiver.poll_recv(cx))
    }
}

/// Given a model id string, resolve it to a catalogue entry.
pub fn resolve_model(id: &str) -> Result<Model> {
    Model::from_id(id).ok_or_else(|| AiError::UnknownModel(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::ApiKey;
    use crate::models::Effort;
    use crate::provider::ToolDefinition;

    fn provider_with(base_url: &str) -> AnthropicProvider {
        let keys = KeyStore::memory();
        keys.set(Provider::Anthropic, &ApiKey::new("sk-ant-api03-test-key-value")).unwrap();
        AnthropicProvider::with_base_url(keys, base_url).unwrap()
    }

    // --- Request construction ---

    #[test]
    fn the_body_carries_the_model_and_messages() {
        let provider = provider_with(DEFAULT_BASE_URL);
        let request = CompletionRequest::new(Model::Opus5, vec![Message::user("hello")]);
        let body = provider.build_body(&request, false);

        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn streaming_requests_set_the_flag() {
        let provider = provider_with(DEFAULT_BASE_URL);
        let request = CompletionRequest::new(Model::Opus5, vec![Message::user("hi")]);
        assert_eq!(provider.build_body(&request, true)["stream"], true);
    }

    #[test]
    fn temperature_is_dropped_for_models_that_reject_it() {
        // The whole request 400s if temperature reaches Opus 5, so it must be
        // dropped rather than passed through.
        let provider = provider_with(DEFAULT_BASE_URL);
        let request =
            CompletionRequest::new(Model::Opus5, vec![Message::user("hi")]).temperature(0.7);
        let body = provider.build_body(&request, false);
        assert!(
            body.get("temperature").is_none(),
            "temperature must not be sent to Opus 5: {body}"
        );

        // Haiku 4.5 predates the restriction and still accepts it.
        let request =
            CompletionRequest::new(Model::Haiku45, vec![Message::user("hi")]).temperature(0.7);
        let body = provider.build_body(&request, false);
        let sent = body["temperature"].as_f64().expect("temperature should be sent");
        // The field is an f32 widened to f64 for JSON, so compare with a
        // tolerance rather than against the f64 literal.
        assert!((sent - 0.7).abs() < 1e-6, "sent {sent}");
    }

    #[test]
    fn effort_is_sent_only_to_models_that_support_it() {
        let provider = provider_with(DEFAULT_BASE_URL);

        let request =
            CompletionRequest::new(Model::Opus5, vec![Message::user("hi")]).effort(Effort::Low);
        assert_eq!(provider.build_body(&request, false)["effort"], "low");

        let request =
            CompletionRequest::new(Model::Haiku45, vec![Message::user("hi")]).effort(Effort::Low);
        assert!(provider.build_body(&request, false).get("effort").is_none());
    }

    #[test]
    fn max_tokens_is_clamped_to_the_models_ceiling() {
        let provider = provider_with(DEFAULT_BASE_URL);
        let request = CompletionRequest::new(Model::Haiku45, vec![Message::user("hi")])
            .max_tokens(999_999);
        assert_eq!(
            provider.build_body(&request, false)["max_tokens"],
            Model::Haiku45.info().max_output
        );
    }

    #[test]
    fn the_system_prompt_is_sent_as_a_block_so_it_can_be_cached() {
        let provider = provider_with(DEFAULT_BASE_URL);
        let request = CompletionRequest::new(Model::Opus5, vec![Message::user("hi")])
            .system("You are a code assistant.");
        let body = provider.build_body(&request, false);

        assert!(body["system"].is_array(), "a bare string cannot carry cache_control");
        assert_eq!(body["system"][0]["text"], "You are a code assistant.");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "5m");
    }

    #[test]
    fn caching_can_be_turned_off() {
        let provider = provider_with(DEFAULT_BASE_URL);
        let mut request = CompletionRequest::new(Model::Opus5, vec![Message::user("hi")])
            .system("system prompt");
        request.cache_system = None;

        let body = provider.build_body(&request, false);
        assert!(body["system"][0].get("cache_control").is_none());
    }

    #[test]
    fn a_message_breakpoint_attaches_to_its_last_block() {
        let provider = provider_with(DEFAULT_BASE_URL);
        let messages = vec![
            Message::user("first").cached(CacheTtl::OneHour),
            Message::assistant("second"),
        ];
        let body =
            provider.build_body(&CompletionRequest::new(Model::Opus5, messages), false);

        assert_eq!(body["messages"][0]["content"][0]["cache_control"]["ttl"], "1h");
        assert!(body["messages"][1]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn tool_definitions_are_sent_and_the_block_is_cached() {
        let provider = provider_with(DEFAULT_BASE_URL);
        let request = CompletionRequest::new(Model::Opus5, vec![Message::user("hi")])
            .system("system")
            .tool(ToolDefinition {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: json!({ "type": "object" }),
            })
            .tool(ToolDefinition {
                name: "write_file".into(),
                description: "Write a file".into(),
                input_schema: json!({ "type": "object" }),
            });

        let body = provider.build_body(&request, false);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(
            body["tools"][0].get("cache_control").is_none(),
            "only the last tool carries the breakpoint"
        );
        assert_eq!(body["tools"][1]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn tool_results_are_encoded_as_user_content() {
        let provider = provider_with(DEFAULT_BASE_URL);
        let messages = vec![Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "file contents".into(),
                is_error: false,
            }],
        )];
        let body = provider.build_body(&CompletionRequest::new(Model::Opus5, messages), false);

        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][0]["content"][0]["tool_use_id"], "call_1");
    }

    // --- Response parsing ---

    #[test]
    fn a_text_response_parses() {
        let response = parse_response(&json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{ "type": "text", "text": "Hello there." }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 12, "output_tokens": 4 }
        }))
        .unwrap();

        assert_eq!(response.text(), "Hello there.");
        assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(response.usage.input_tokens, 12);
        assert_eq!(response.model, "claude-opus-5");
    }

    #[test]
    fn a_tool_use_response_parses() {
        let response = parse_response(&json!({
            "content": [
                { "type": "text", "text": "Let me read that." },
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "read_file",
                    "input": { "path": "src/main.rs" }
                }
            ],
            "stop_reason": "tool_use",
            "model": "claude-opus-5"
        }))
        .unwrap();

        assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
        let uses = response.tool_uses();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].0, "toolu_1");
        assert_eq!(uses[0].1, "read_file");
        assert_eq!(uses[0].2["path"], "src/main.rs");
    }

    #[test]
    fn thinking_blocks_parse_with_their_signature() {
        let response = parse_response(&json!({
            "content": [
                { "type": "thinking", "thinking": "Let me work through this.", "signature": "sig123" },
                { "type": "text", "text": "The answer is 4." }
            ],
            "stop_reason": "end_turn"
        }))
        .unwrap();

        assert_eq!(response.content.len(), 2);
        match &response.content[0] {
            ContentBlock::Thinking { thinking, signature } => {
                assert_eq!(thinking, "Let me work through this.");
                assert_eq!(signature.as_deref(), Some("sig123"));
            }
            other => panic!("expected a thinking block, got {other:?}"),
        }
        assert_eq!(response.text(), "The answer is 4.");
    }

    #[test]
    fn an_unknown_block_type_is_skipped_rather_than_failing_the_response() {
        // A newer API revision adding a block type must not break the client.
        let response = parse_response(&json!({
            "content": [
                { "type": "text", "text": "kept" },
                { "type": "some_future_block", "data": {} }
            ]
        }))
        .unwrap();
        assert_eq!(response.content.len(), 1);
        assert_eq!(response.text(), "kept");
    }

    #[test]
    fn cache_usage_is_read_from_the_ttl_breakdown() {
        let usage = parse_usage(Some(&json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 4000,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 200,
                "ephemeral_1h_input_tokens": 800
            }
        })));

        assert_eq!(usage.cache_read_tokens, 4000);
        assert_eq!(usage.cache_write_5m_tokens, 200);
        assert_eq!(usage.cache_write_1h_tokens, 800);
        assert_eq!(usage.total_input(), 100 + 4000 + 200 + 800);
        assert!(usage.cache_hit_rate() > 0.7);
    }

    #[test]
    fn a_flat_cache_creation_count_falls_back_to_the_five_minute_rate() {
        let usage = parse_usage(Some(&json!({
            "input_tokens": 10,
            "cache_creation_input_tokens": 500
        })));
        assert_eq!(usage.cache_write_5m_tokens, 500);
        assert_eq!(usage.cache_write_1h_tokens, 0);
    }

    #[test]
    fn a_missing_usage_object_yields_zeros_not_a_panic() {
        assert_eq!(parse_usage(None), Usage::default());
        assert_eq!(parse_usage(Some(&json!({}))), Usage::default());
    }

    #[test]
    fn every_stop_reason_maps() {
        assert_eq!(parse_stop_reason("end_turn"), Some(StopReason::EndTurn));
        assert_eq!(parse_stop_reason("max_tokens"), Some(StopReason::MaxTokens));
        assert_eq!(parse_stop_reason("stop_sequence"), Some(StopReason::StopSequence));
        assert_eq!(parse_stop_reason("tool_use"), Some(StopReason::ToolUse));
        assert_eq!(parse_stop_reason("refusal"), Some(StopReason::Refusal));
        assert_eq!(parse_stop_reason("something_new"), None);
    }

    // --- SSE parsing ---

    fn drain(events: &[&str]) -> (Vec<StreamEvent>, Usage) {
        let mut usage = Usage::default();
        let mut out = Vec::new();
        for event in events {
            out.extend(parse_sse_event(event, &mut usage));
        }
        (out, usage)
    }

    #[test]
    fn a_text_stream_parses_into_deltas() {
        let (events, _) = drain(&[
            r#"event: message_start
data: {"type":"message_start","message":{"model":"claude-opus-5","usage":{"input_tokens":10}}}"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
        ]);

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");

        match events.first() {
            Some(StreamEvent::Start { model }) => assert_eq!(model, "claude-opus-5"),
            other => panic!("expected a Start event, got {other:?}"),
        }
        match events.last() {
            Some(StreamEvent::End { stop_reason, usage }) => {
                assert_eq!(*stop_reason, Some(StopReason::EndTurn));
                assert_eq!(usage.input_tokens, 10, "input tokens come from message_start");
                assert_eq!(usage.output_tokens, 7, "output tokens are final in message_delta");
            }
            other => panic!("expected an End event, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_use_stream_parses_its_partial_json() {
        let (events, _) = drain(&[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"src/main.rs\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
        ]);

        match &events[0] {
            StreamEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolUseStart, got {other:?}"),
        }

        // The fragments are not independently parseable; concatenated they are.
        let json: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolInputDelta { partial_json } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["path"], "src/main.rs");

        assert!(matches!(events.last(), Some(StreamEvent::BlockEnd { index: 0 })));
    }

    #[test]
    fn thinking_deltas_are_surfaced_separately_from_text() {
        let (events, _) = drain(&[
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"considering"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
        ]);

        assert!(matches!(&events[0], StreamEvent::ThinkingDelta { thinking } if thinking == "considering"));
        assert!(matches!(&events[1], StreamEvent::TextDelta { text } if text == "answer"));
    }

    #[test]
    fn ping_events_are_ignored() {
        let (events, _) = drain(&["event: ping\ndata: {\"type\":\"ping\"}"]);
        assert!(events.is_empty());
    }

    #[test]
    fn malformed_sse_data_does_not_panic() {
        let mut usage = Usage::default();
        assert!(parse_sse_event("data: not json at all", &mut usage).is_empty());
        assert!(parse_sse_event("", &mut usage).is_empty());
        assert!(parse_sse_event("event: only\n", &mut usage).is_empty());
        assert!(parse_sse_event("data: {\"no_type\":true}", &mut usage).is_empty());
    }

    #[test]
    fn model_ids_resolve_and_unknown_ones_error() {
        assert_eq!(resolve_model("claude-opus-5").unwrap(), Model::Opus5);
        assert!(matches!(resolve_model("claude-2"), Err(AiError::UnknownModel(_))));
    }

    #[tokio::test]
    async fn a_provider_without_a_key_reports_it_before_making_a_request() {
        // Point the fallback at a provider whose variable is not set in any
        // realistic environment, rather than mutating the process environment
        // (which races other tests and is `unsafe` in this edition).
        let keys = KeyStore::memory();
        let provider = AnthropicProvider::with_base_url(keys, DEFAULT_BASE_URL).unwrap();

        if std::env::var(Provider::Anthropic.env_var()).is_ok() {
            // A developer machine with the key exported: the fallback is
            // working as designed, which is itself the thing worth asserting.
            assert!(provider.is_configured().await);
            return;
        }

        assert!(!provider.is_configured().await);
        let request = CompletionRequest::new(Model::Opus5, vec![Message::user("hi")]);
        let err = provider.complete(request).await.unwrap_err();
        assert!(matches!(err, AiError::NoApiKey(_)), "{err:?}");
        assert!(err.is_auth_failure());
    }
}
