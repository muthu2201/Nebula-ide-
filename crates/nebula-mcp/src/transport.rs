//! Transports: stdio and Streamable HTTP.
//!
//! These are the only two transports 2026-07-28 defines; the legacy HTTP+SSE
//! transport is deprecated and is not implemented here.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
use crate::{McpError, Result};

/// Routing headers a request carries on Streamable HTTP.
#[derive(Debug, Clone, Default)]
pub struct RequestHeaders {
    /// `Mcp-Method`: the JSON-RPC method, so a gateway can route and meter
    /// without parsing the body. Mandatory from 2026-07-28.
    pub method: Option<String>,
    /// `Mcp-Name`: the tool, prompt or resource name being addressed.
    pub name: Option<String>,
    /// `MCP-Protocol-Version`: the revision this request is written against.
    pub protocol_version: Option<String>,
}

/// A bidirectional channel to an MCP server.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send a request and wait for its response.
    async fn request(
        &self,
        request: JsonRpcRequest,
        headers: &RequestHeaders,
    ) -> Result<JsonRpcResponse>;

    /// Send a notification, expecting no response.
    async fn notify(&self, request: JsonRpcRequest, headers: &RequestHeaders) -> Result<()>;

    /// Shut the transport down.
    async fn close(&self) -> Result<()>;

    /// A description for logs and error messages.
    fn describe(&self) -> String;
}

/// Allocates JSON-RPC request identifiers.
#[derive(Debug, Default)]
pub struct IdAllocator(AtomicI64);

impl IdAllocator {
    /// The next identifier.
    pub fn next(&self) -> i64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// A server spoken to over stdin/stdout of a child process.
///
/// Messages are newline-delimited JSON. A single reader task owns stdout and
/// dispatches each response to the waiting caller by identifier, which is what
/// lets several requests be in flight at once — MCP servers are free to answer
/// out of order, and a client that assumed FIFO would deadlock against one that
/// does.
pub struct StdioTransport {
    command: String,
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>>,
    child: tokio::sync::Mutex<tokio::process::Child>,
    timeout: Duration,
}

impl StdioTransport {
    /// Launch `program` and speak MCP over its stdio.
    pub async fn spawn(
        program: &str,
        args: &[String],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<Self> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            McpError::Transport("child process has no stdin".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            McpError::Transport("child process has no stdout".to_string())
        })?;

        // A server's stderr is diagnostics, not protocol. Draining it keeps the
        // child from blocking on a full pipe, and logging it is how a user finds
        // out why their server failed to start.
        if let Some(stderr) = child.stderr.take() {
            let program = program.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(server = %program, "{line}");
                }
            });
        }

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        {
            let pending = Arc::clone(&pending);
            let program = program.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<JsonRpcResponse>(&line) {
                        Ok(response) => {
                            let Some(id) = response.id.as_ref().map(id_key) else {
                                // A server→client request or notification. The
                                // stateless core does not use these, and older
                                // revisions' uses (sampling, roots) are handled
                                // by the client, not the transport.
                                tracing::debug!(server = %program, "unsolicited message: {line}");
                                continue;
                            };
                            if let Some(sender) = pending.lock().remove(&id) {
                                let _ = sender.send(response);
                            } else {
                                tracing::debug!(server = %program, id, "response for an unknown request");
                            }
                        }
                        Err(err) => {
                            tracing::warn!(server = %program, %err, "unparseable line from server: {line}");
                        }
                    }
                }
                // The server closed stdout: fail every waiter rather than
                // leaving them hanging until their individual timeouts.
                pending.lock().clear();
            });
        }

        Ok(Self {
            command: program.to_string(),
            stdin: tokio::sync::Mutex::new(stdin),
            pending,
            child: tokio::sync::Mutex::new(child),
            timeout,
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(
        &self,
        request: JsonRpcRequest,
        _headers: &RequestHeaders,
    ) -> Result<JsonRpcResponse> {
        // stdio has no headers; the same information travels in `_meta`.
        let id = request
            .id
            .as_ref()
            .map(id_key)
            .ok_or_else(|| McpError::Protocol("request has no id".to_string()))?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id.clone(), tx);

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                Err(McpError::Transport("server closed the connection".to_string()))
            }
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(McpError::Timeout(self.timeout))
            }
        }
    }

    async fn notify(&self, request: JsonRpcRequest, _headers: &RequestHeaders) -> Result<()> {
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
        Ok(())
    }

    fn describe(&self) -> String {
        format!("stdio://{}", self.command)
    }
}

/// A server spoken to over Streamable HTTP.
pub struct StreamableHttpTransport {
    endpoint: url::Url,
    client: reqwest::Client,
    /// Extra headers, typically `Authorization`.
    headers: Vec<(String, String)>,
    timeout: Duration,
}

impl StreamableHttpTransport {
    /// Connect to `endpoint`.
    pub fn new(endpoint: url::Url, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("nebula-ide/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| McpError::Transport(format!("building http client: {e}")))?;
        Ok(Self { endpoint, client, headers: Vec::new(), timeout })
    }

    /// Add a header sent with every request.
    ///
    /// Credentials set here are issuer-bound: 2026-07-28 forbids reusing a
    /// token across servers, so the caller supplies a distinct one per endpoint.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    async fn post(&self, request: &JsonRpcRequest, headers: &RequestHeaders) -> Result<String> {
        let mut builder = self
            .client
            .post(self.endpoint.clone())
            .header("Content-Type", "application/json")
            // A server may answer either way; accepting both means a server that
            // streams a long tool call still works.
            .header("Accept", "application/json, text/event-stream");

        if let Some(version) = &headers.protocol_version {
            builder = builder.header("MCP-Protocol-Version", version);
        }
        if let Some(method) = &headers.method {
            builder = builder.header("Mcp-Method", method);
        }
        if let Some(name) = &headers.name {
            builder = builder.header("Mcp-Name", name);
        }
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }

        let response = builder
            .json(request)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    McpError::Timeout(self.timeout)
                } else {
                    McpError::Transport(format!("request failed: {e}"))
                }
            })?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response
            .text()
            .await
            .map_err(|e| McpError::Transport(format!("reading response body: {e}")))?;

        if !status.is_success() {
            return Err(McpError::Transport(format!(
                "server returned HTTP {status}: {}",
                body.chars().take(400).collect::<String>()
            )));
        }

        if content_type.starts_with("text/event-stream") {
            extract_sse_payload(&body)
                .ok_or_else(|| McpError::Protocol("event stream contained no data".to_string()))
        } else {
            Ok(body)
        }
    }
}

/// Pull the last JSON payload out of an SSE body.
///
/// Streamable HTTP may answer a single request with a short event stream —
/// progress notifications followed by the result. The final `data:` payload
/// carrying an `id` is the response.
fn extract_sse_payload(body: &str) -> Option<String> {
    let mut last_with_id: Option<String> = None;
    let mut last_any: Option<String> = None;

    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        last_any = Some(data.to_string());
        if serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .is_some_and(|v| v.get("id").is_some())
        {
            last_with_id = Some(data.to_string());
        }
    }
    last_with_id.or(last_any)
}

#[async_trait]
impl Transport for StreamableHttpTransport {
    async fn request(
        &self,
        request: JsonRpcRequest,
        headers: &RequestHeaders,
    ) -> Result<JsonRpcResponse> {
        let body = self.post(&request, headers).await?;
        serde_json::from_str::<JsonRpcResponse>(&body).map_err(|e| {
            McpError::Protocol(format!(
                "could not parse response: {e}; body was {}",
                body.chars().take(400).collect::<String>()
            ))
        })
    }

    async fn notify(&self, request: JsonRpcRequest, headers: &RequestHeaders) -> Result<()> {
        // A notification gets an HTTP 202 with no body; anything parseable is
        // ignored.
        let _ = self.post(&request, headers).await?;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    fn describe(&self) -> String {
        format!("http://{}", self.endpoint)
    }
}

/// A stable string key for a JSON-RPC id, which may be a number or a string.
fn id_key(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(s) => format!("s:{s}"),
        other => format!("n:{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_keys_distinguish_string_and_numeric_ids() {
        // A server answering id 1 must not satisfy a caller waiting on id "1".
        let numeric = id_key(&serde_json::json!(1));
        let string = id_key(&serde_json::json!("1"));
        assert_ne!(numeric, string);
    }

    #[test]
    fn id_allocation_is_monotonic_and_starts_at_one() {
        let allocator = IdAllocator::default();
        assert_eq!(allocator.next(), 1);
        assert_eq!(allocator.next(), 2);
        assert_eq!(allocator.next(), 3);
    }

    #[test]
    fn sse_extraction_prefers_the_payload_carrying_an_id() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\
                    \n\
                    event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\
                    \n";
        let payload = extract_sse_payload(body).unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["ok"], true);
    }

    #[test]
    fn sse_extraction_ignores_the_done_sentinel() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\ndata: [DONE]\n\n";
        let payload = extract_sse_payload(body).unwrap();
        assert!(payload.contains("\"id\":2"));
    }

    #[test]
    fn an_empty_event_stream_yields_nothing() {
        assert_eq!(extract_sse_payload(""), None);
        assert_eq!(extract_sse_payload("event: ping\n\n"), None);
    }

    #[tokio::test]
    async fn stdio_transport_reports_a_missing_program() {
        let result = StdioTransport::spawn(
            "nebula-mcp-no-such-server",
            &[],
            &[],
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn http_transport_rejects_an_unusable_endpoint() {
        let url = url::Url::parse("http://127.0.0.1:1/mcp").unwrap();
        // Construction succeeds; the failure surfaces on use, which is the
        // right place for a network error.
        assert!(StreamableHttpTransport::new(url, Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn http_transport_describes_its_endpoint() {
        let url = url::Url::parse("https://example.invalid/mcp").unwrap();
        let transport = StreamableHttpTransport::new(url, Duration::from_secs(5)).unwrap();
        assert!(transport.describe().contains("example.invalid"));
    }
}
