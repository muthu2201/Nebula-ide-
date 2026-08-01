//! The provider port.
//!
//! Everything above this module — the agent loop, the chat panel, the commit
//! message generator — depends only on [`ModelProvider`]. That is what keeps
//! Nebula cloud-agnostic: adding a provider is one implementation of this trait,
//! and removing one (if its terms stop permitting direct BYOK calls) is one
//! deletion.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::cache::CacheTtl;
use crate::models::{Effort, Model};

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The user.
    User,
    /// The model.
    Assistant,
}

/// A piece of a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// The model asking to call a tool.
    ToolUse {
        /// Correlates the call with its result.
        id: String,
        /// Which tool.
        name: String,
        /// The arguments.
        input: serde_json::Value,
    },
    /// The result of a tool call, sent back as a user-role message.
    ToolResult {
        /// The `id` of the `ToolUse` this answers.
        tool_use_id: String,
        /// What the tool produced.
        content: String,
        /// Whether the tool failed.
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    /// The model's reasoning, when thinking is enabled.
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// The signature that lets the model verify its own prior reasoning on
        /// a subsequent turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ContentBlock {
    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    /// The text of this block, if it has any.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// One turn in a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who said it.
    pub role: Role,
    /// What they said.
    pub content: Vec<ContentBlock>,
    /// Where to place a cache breakpoint after this message, if anywhere.
    ///
    /// Not serialised directly: the adapter translates it into the provider's
    /// own `cache_control` representation.
    #[serde(skip)]
    pub cache: Option<CacheTtl>,
}

impl Message {
    /// A user message containing only text.
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: vec![ContentBlock::text(text)], cache: None }
    }

    /// An assistant message containing only text.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: vec![ContentBlock::text(text)], cache: None }
    }

    /// A message with arbitrary content blocks.
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content, cache: None }
    }

    /// Place a cache breakpoint after this message.
    pub fn cached(mut self, ttl: CacheTtl) -> Self {
        self.cache = Some(ttl);
        self
    }

    /// All text in this message, concatenated.
    pub fn text(&self) -> String {
        self.content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("")
    }

    /// The tool calls this message contains.
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect()
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// JSON Schema for its input.
    pub input_schema: serde_json::Value,
}

/// A request for a completion.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionRequest {
    /// Which model.
    pub model: Model,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// The system prompt.
    pub system: Option<String>,
    /// Whether to cache the system prompt.
    ///
    /// Almost always worth it: the system prompt is identical across every turn
    /// of a session, and a cache hit costs a tenth of a fresh read.
    pub cache_system: Option<CacheTtl>,
    /// Tools the model may call.
    pub tools: Vec<ToolDefinition>,
    /// Maximum tokens to generate.
    pub max_tokens: usize,
    /// Reasoning effort.
    pub effort: Option<Effort>,
    /// Sampling temperature.
    ///
    /// Ignored for models whose [`crate::models::ModelInfo::accepts_sampling_params`]
    /// is false — sending it there fails the request outright.
    pub temperature: Option<f32>,
    /// Stop sequences.
    pub stop_sequences: Vec<String>,
}

impl CompletionRequest {
    /// A request with sensible defaults.
    pub fn new(model: Model, messages: Vec<Message>) -> Self {
        Self {
            model,
            messages,
            system: None,
            cache_system: Some(CacheTtl::FiveMinutes),
            tools: Vec::new(),
            // Deliberately not the model's maximum: a runaway generation is the
            // user's money, and a caller that needs more says so.
            max_tokens: 8192,
            effort: None,
            temperature: None,
            stop_sequences: Vec::new(),
        }
    }

    /// Set the system prompt.
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Set the output ceiling.
    pub fn max_tokens(mut self, tokens: usize) -> Self {
        self.max_tokens = tokens;
        self
    }

    /// Set the reasoning effort.
    pub fn effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Add a tool.
    pub fn tool(mut self, tool: ToolDefinition) -> Self {
        self.tools.push(tool);
        self
    }

    /// Set the sampling temperature.
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The output hit `max_tokens`.
    MaxTokens,
    /// A stop sequence matched.
    StopSequence,
    /// The model is calling a tool and is waiting for the result.
    ToolUse,
    /// The model refused.
    Refusal,
}

/// Token accounting for one request.
///
/// The cache fields matter: a misplaced breakpoint silently degrades to a
/// full-price call, and reading these is the only way to notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Fresh input tokens.
    pub input_tokens: usize,
    /// Generated tokens.
    pub output_tokens: usize,
    /// Tokens served from cache, billed at 0.1×.
    pub cache_read_tokens: usize,
    /// Tokens written to the five-minute cache, billed at 1.25×.
    pub cache_write_5m_tokens: usize,
    /// Tokens written to the one-hour cache, billed at 2×.
    pub cache_write_1h_tokens: usize,
}

impl Usage {
    /// Total input tokens, cached or not.
    pub fn total_input(&self) -> usize {
        self.input_tokens
            + self.cache_read_tokens
            + self.cache_write_5m_tokens
            + self.cache_write_1h_tokens
    }

    /// The fraction of input tokens served from cache.
    ///
    /// Worth surfacing: a session whose hit rate has collapsed usually means a
    /// breakpoint is sitting after something that changes every turn.
    pub fn cache_hit_rate(&self) -> f64 {
        let total = self.total_input();
        if total == 0 {
            return 0.0;
        }
        self.cache_read_tokens as f64 / total as f64
    }

    /// Add another request's usage to this one.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_5m_tokens += other.cache_write_5m_tokens;
        self.cache_write_1h_tokens += other.cache_write_1h_tokens;
    }
}

/// A completed response.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResponse {
    /// The content the model produced.
    pub content: Vec<ContentBlock>,
    /// Why it stopped.
    pub stop_reason: Option<StopReason>,
    /// Token accounting.
    pub usage: Usage,
    /// The model that answered, as the provider reported it.
    pub model: String,
}

impl CompletionResponse {
    /// All text content concatenated.
    pub fn text(&self) -> String {
        self.content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("")
    }

    /// The tool calls the model made.
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect()
    }

    /// Turn this response into an assistant message, for the next turn.
    pub fn as_message(&self) -> Message {
        Message::new(Role::Assistant, self.content.clone())
    }
}

/// An incremental event while streaming.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Generation started.
    Start {
        /// The model that is answering.
        model: String,
    },
    /// A fragment of text.
    TextDelta {
        /// The fragment.
        text: String,
    },
    /// A fragment of reasoning.
    ThinkingDelta {
        /// The fragment.
        thinking: String,
    },
    /// The model began a tool call.
    ToolUseStart {
        /// The call's identifier.
        id: String,
        /// Which tool.
        name: String,
    },
    /// A fragment of a tool call's JSON arguments.
    ToolInputDelta {
        /// The fragment, which is *not* independently parseable JSON.
        partial_json: String,
    },
    /// A content block finished.
    BlockEnd {
        /// Its index in the message.
        index: usize,
    },
    /// Generation finished.
    End {
        /// Why.
        stop_reason: Option<StopReason>,
        /// Token accounting.
        usage: Usage,
    },
}

/// A source of completions.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// A short identifier, e.g. `anthropic`.
    fn name(&self) -> &str;

    /// Send a request and wait for the whole response.
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;

    /// Send a request and receive incremental events.
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>>;

    /// Ask the provider to count the request's tokens exactly.
    ///
    /// Providers ship their own tokeniser and it changes between model
    /// generations — the 4.7+ tokeniser produces roughly 30% more tokens for the
    /// same text — so an exact count comes from the provider, never from a
    /// local approximation.
    async fn count_tokens(&self, request: &CompletionRequest) -> Result<usize>;

    /// Whether this provider currently has usable credentials.
    async fn is_configured(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_expose_their_text() {
        let message = Message::user("hello");
        assert_eq!(message.role, Role::User);
        assert_eq!(message.text(), "hello");
    }

    #[test]
    fn multi_block_messages_concatenate_only_their_text() {
        let message = Message::new(
            Role::Assistant,
            vec![
                ContentBlock::text("Let me check. "),
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({ "path": "src/main.rs" }),
                },
                ContentBlock::text("Done."),
            ],
        );
        assert_eq!(message.text(), "Let me check. Done.");

        let uses = message.tool_uses();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].1, "read_file");
    }

    #[test]
    fn content_blocks_serialise_with_the_expected_tags() {
        let text = serde_json::to_value(ContentBlock::text("hi")).unwrap();
        assert_eq!(text["type"], "text");
        assert_eq!(text["text"], "hi");

        let tool_use = serde_json::to_value(ContentBlock::ToolUse {
            id: "id".into(),
            name: "n".into(),
            input: serde_json::json!({}),
        })
        .unwrap();
        assert_eq!(tool_use["type"], "tool_use");

        let result = serde_json::to_value(ContentBlock::ToolResult {
            tool_use_id: "id".into(),
            content: "output".into(),
            is_error: false,
        })
        .unwrap();
        assert_eq!(result["type"], "tool_result");
        assert!(
            result.get("is_error").is_none(),
            "a false is_error should be omitted rather than sent"
        );
    }

    #[test]
    fn a_failed_tool_result_serialises_its_error_flag() {
        let result = serde_json::to_value(ContentBlock::ToolResult {
            tool_use_id: "id".into(),
            content: "no such file".into(),
            is_error: true,
        })
        .unwrap();
        assert_eq!(result["is_error"], true);
    }

    #[test]
    fn requests_default_to_a_bounded_output() {
        let request = CompletionRequest::new(Model::Opus5, vec![Message::user("hi")]);
        assert_eq!(request.max_tokens, 8192);
        assert!(
            request.max_tokens < Model::Opus5.info().max_output,
            "the default must not be the model's maximum; a runaway generation is the user's money"
        );
        assert_eq!(request.cache_system, Some(CacheTtl::FiveMinutes));
    }

    #[test]
    fn request_builders_compose() {
        let request = CompletionRequest::new(Model::Sonnet5, vec![Message::user("hi")])
            .system("You are helpful.")
            .max_tokens(1000)
            .effort(Effort::Low)
            .temperature(0.5)
            .tool(ToolDefinition {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            });

        assert_eq!(request.system.as_deref(), Some("You are helpful."));
        assert_eq!(request.max_tokens, 1000);
        assert_eq!(request.effort, Some(Effort::Low));
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.tools.len(), 1);
    }

    #[test]
    fn usage_totals_count_every_input_category() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 900,
            cache_write_5m_tokens: 200,
            cache_write_1h_tokens: 0,
        };
        assert_eq!(usage.total_input(), 1200);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn the_cache_hit_rate_reveals_a_misplaced_breakpoint() {
        let healthy = Usage { input_tokens: 100, cache_read_tokens: 900, ..Usage::default() };
        assert!((healthy.cache_hit_rate() - 0.9).abs() < 1e-9);

        // Every turn paying full price: the symptom of a breakpoint placed
        // after something that changes.
        let broken = Usage { input_tokens: 1000, ..Usage::default() };
        assert_eq!(broken.cache_hit_rate(), 0.0);

        assert_eq!(Usage::default().cache_hit_rate(), 0.0, "no division by zero");
    }

    #[test]
    fn usage_accumulates_across_turns() {
        let mut total = Usage::default();
        total.add(&Usage { input_tokens: 10, output_tokens: 5, ..Usage::default() });
        total.add(&Usage { input_tokens: 20, cache_read_tokens: 100, ..Usage::default() });

        assert_eq!(total.input_tokens, 30);
        assert_eq!(total.output_tokens, 5);
        assert_eq!(total.cache_read_tokens, 100);
    }

    #[test]
    fn a_response_converts_into_the_next_turns_message() {
        let response = CompletionResponse {
            content: vec![ContentBlock::text("answer")],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
            model: "claude-opus-5".into(),
        };
        let message = response.as_message();
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.text(), "answer");
    }

    #[test]
    fn stop_reasons_use_the_wire_spelling() {
        assert_eq!(serde_json::to_value(StopReason::EndTurn).unwrap(), "end_turn");
        assert_eq!(serde_json::to_value(StopReason::MaxTokens).unwrap(), "max_tokens");
        assert_eq!(serde_json::to_value(StopReason::ToolUse).unwrap(), "tool_use");
    }
}
