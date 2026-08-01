//! Wire types.

use serde::{Deserialize, Serialize};

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request identifier. Absent for notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    /// Method name.
    pub method: String,
    /// Parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    /// A request with an identifier.
    pub fn new(id: impl Into<serde_json::Value>, method: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id.into()),
            method: method.into(),
            params: None,
        }
    }

    /// A notification: no identifier, no response expected.
    pub fn notification(method: impl Into<String>) -> Self {
        Self { jsonrpc: "2.0".to_string(), id: None, method: method.into(), params: None }
    }

    /// Attach parameters.
    pub fn with_params(mut self, params: serde_json::Value) -> Self {
        self.params = Some(params);
        self
    }

    /// Merge `meta` into the request's `params._meta`.
    ///
    /// This is how the stateless core carries protocol version, client identity
    /// and capabilities: every request is self-describing, so any request can be
    /// answered by any server instance.
    pub fn with_meta(mut self, meta: serde_json::Value) -> Self {
        let params = self.params.get_or_insert_with(|| serde_json::json!({}));
        if let Some(object) = params.as_object_mut() {
            let existing = object.entry("_meta").or_insert_with(|| serde_json::json!({}));
            if let (Some(existing), Some(incoming)) = (existing.as_object_mut(), meta.as_object()) {
                for (key, value) in incoming {
                    existing.insert(key.clone(), value.clone());
                }
            } else {
                *existing = meta;
            }
        }
        self
    }

    /// Whether this is a notification.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Structured detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// The identifier of the request being answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    /// The result, when the call succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// The error, when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// A success response.
    pub fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self { jsonrpc: "2.0".to_string(), id: Some(id), result: Some(result), error: None }
    }

    /// An error response.
    pub fn failure(id: Option<serde_json::Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message: message.into(), data: None }),
        }
    }
}

/// A tool a server exposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name, unique within the server.
    pub name: String,
    /// What the tool does. This text goes to the model, so it is part of the
    /// prompt and is treated as untrusted input.
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the arguments.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
    /// Optional annotations: read-only hints, destructive hints, and similar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,
}

impl Tool {
    /// Whether the server annotated this tool as read-only.
    ///
    /// A hint from the server, not a guarantee. Nebula uses it to decide what
    /// can run without a permission prompt, and the sandbox — not this flag —
    /// is what actually prevents a lying server from doing damage.
    pub fn is_read_only(&self) -> bool {
        self.annotations
            .as_ref()
            .and_then(|a| a.get("readOnlyHint"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Whether the server annotated this tool as destructive.
    pub fn is_destructive(&self) -> bool {
        self.annotations
            .as_ref()
            .and_then(|a| a.get("destructiveHint"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

/// A prompt template a server exposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    /// Prompt name.
    pub name: String,
    /// What it is for.
    #[serde(default)]
    pub description: String,
    /// Declared arguments.
    #[serde(default)]
    pub arguments: Vec<serde_json::Value>,
}

/// A resource a server exposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    /// Resource URI.
    pub uri: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// What it contains.
    #[serde(default)]
    pub description: String,
    /// MIME type, if known.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// A piece of content in a tool result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// Base64-encoded image data.
    Image {
        /// Base64 payload.
        data: String,
        /// MIME type.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Embedded resource contents.
    Resource {
        /// The resource payload as the server sent it.
        resource: serde_json::Value,
    },
}

impl Content {
    /// The text of this content, if it has any.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// The result of calling a tool.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolResult {
    /// Returned content.
    #[serde(default)]
    pub content: Vec<Content>,
    /// Whether the tool itself reported an error.
    ///
    /// Distinct from a JSON-RPC error: the call succeeded, and the tool is
    /// telling the model that what it was asked to do did not work.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    /// Structured content, when the tool declares an output schema.
    #[serde(rename = "structuredContent", default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<serde_json::Value>,
}

impl ToolResult {
    /// All text content concatenated, which is what goes into the transcript.
    pub fn text(&self) -> String {
        self.content.iter().filter_map(Content::as_text).collect::<Vec<_>>().join("\n")
    }
}

/// A request from the server for input the client must supply, from the
/// Multi Round-Trip Request flow.
///
/// In 2026-07-28 a server that needs elicitation returns
/// `resultType: "input_required"` rather than holding a stream open; the client
/// answers by retrying the original call with `inputResponses` filled in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRequest {
    /// Correlates the answer with the question.
    pub id: String,
    /// What kind of input is wanted (`elicitation`, `sampling`, `roots`).
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The request payload, whose shape depends on `kind`.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Standard JSON-RPC error codes, plus the MCP-specific ones.
pub mod error_codes {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON is not a valid request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameters.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal server error.
    pub const INTERNAL_ERROR: i64 = -32603;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_serialise_without_null_fields() {
        let request = JsonRpcRequest::new(1, "tools/list");
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "tools/list");
        assert!(json.get("params").is_none(), "an absent params must not serialise as null");
    }

    #[test]
    fn notifications_carry_no_id() {
        let notification = JsonRpcRequest::notification("notifications/initialized");
        assert!(notification.is_notification());
        let json = serde_json::to_value(&notification).unwrap();
        assert!(json.get("id").is_none());
    }

    #[test]
    fn meta_is_merged_into_params_not_replacing_them() {
        let request = JsonRpcRequest::new(1, "tools/call")
            .with_params(serde_json::json!({ "name": "search" }))
            .with_meta(serde_json::json!({ "protocolVersion": "2026-07-28" }));

        let params = request.params.unwrap();
        assert_eq!(params["name"], "search", "existing params must survive");
        assert_eq!(params["_meta"]["protocolVersion"], "2026-07-28");
    }

    #[test]
    fn repeated_meta_merges_rather_than_overwriting() {
        let request = JsonRpcRequest::new(1, "tools/call")
            .with_meta(serde_json::json!({ "protocolVersion": "2026-07-28" }))
            .with_meta(serde_json::json!({ "clientId": "nebula" }));

        let meta = &request.params.unwrap()["_meta"];
        assert_eq!(meta["protocolVersion"], "2026-07-28");
        assert_eq!(meta["clientId"], "nebula");
    }

    #[test]
    fn meta_can_be_attached_to_a_request_with_no_params() {
        let request = JsonRpcRequest::new(1, "tools/list").with_meta(serde_json::json!({ "a": 1 }));
        assert_eq!(request.params.unwrap()["_meta"]["a"], 1);
    }

    #[test]
    fn responses_round_trip() {
        let response =
            JsonRpcResponse::success(serde_json::json!(7), serde_json::json!({ "tools": [] }));
        let text = serde_json::to_string(&response).unwrap();
        let parsed: JsonRpcResponse = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, response);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn tools_parse_with_only_the_required_fields() {
        let tool: Tool = serde_json::from_value(serde_json::json!({
            "name": "minimal"
        }))
        .unwrap();
        assert_eq!(tool.name, "minimal");
        assert_eq!(tool.description, "");
        assert!(!tool.is_read_only());
        assert!(!tool.is_destructive());
    }

    #[test]
    fn tool_annotations_are_read_when_present() {
        let tool: Tool = serde_json::from_value(serde_json::json!({
            "name": "reader",
            "inputSchema": { "type": "object" },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        }))
        .unwrap();
        assert!(tool.is_read_only());
        assert!(!tool.is_destructive());
    }

    #[test]
    fn tool_results_concatenate_their_text() {
        let result: ToolResult = serde_json::from_value(serde_json::json!({
            "content": [
                { "type": "text", "text": "first" },
                { "type": "image", "data": "AAAA", "mimeType": "image/png" },
                { "type": "text", "text": "second" }
            ]
        }))
        .unwrap();
        assert_eq!(result.text(), "first\nsecond");
        assert!(!result.is_error);
    }

    #[test]
    fn a_tool_reported_error_is_not_a_transport_error() {
        let result: ToolResult = serde_json::from_value(serde_json::json!({
            "content": [{ "type": "text", "text": "file not found" }],
            "isError": true
        }))
        .unwrap();
        assert!(result.is_error, "the call succeeded; the tool is reporting failure");
        assert_eq!(result.text(), "file not found");
    }

    #[test]
    fn structured_content_is_preserved() {
        let result: ToolResult = serde_json::from_value(serde_json::json!({
            "content": [],
            "structuredContent": { "rows": [1, 2, 3] }
        }))
        .unwrap();
        assert_eq!(result.structured_content.unwrap()["rows"][2], 3);
    }

    #[test]
    fn input_requests_parse_from_the_mrtr_shape() {
        let request: InputRequest = serde_json::from_value(serde_json::json!({
            "id": "elicit-1",
            "type": "elicitation",
            "params": { "message": "Which branch?" }
        }))
        .unwrap();
        assert_eq!(request.id, "elicit-1");
        assert_eq!(request.kind, "elicitation");
        assert_eq!(request.params["message"], "Which branch?");
    }
}
