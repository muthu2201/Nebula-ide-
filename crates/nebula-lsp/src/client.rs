//! The language server client.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use nebula_core::TextBuffer;
use parking_lot::RwLock;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

use crate::framing::{FrameDecoder, encode_message};
use crate::types::{CompletionItem, Diagnostic, Location, Position, path_to_uri};
use crate::{LspError, Result};

/// How to run a language server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Per-request timeout.
    pub timeout: Duration,
    /// The project root sent as the workspace folder.
    pub root_path: Option<std::path::PathBuf>,
    /// Client name reported in `initialize`.
    pub client_name: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // Generous: rust-analyzer can take a long time to answer the first
            // request on a cold project while it indexes.
            timeout: Duration::from_secs(30),
            root_path: None,
            client_name: "nebula-ide".to_string(),
        }
    }
}

/// State shared with the background reader task.
#[derive(Default)]
struct Shared {
    /// Requests awaiting a response, by id.
    pending: parking_lot::Mutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, LspError>>>>,
    /// Latest diagnostics per document URI.
    diagnostics: RwLock<HashMap<String, Vec<Diagnostic>>>,
    /// Whether the server has closed its output.
    closed: std::sync::atomic::AtomicBool,
}

/// A connected language server.
pub struct LanguageServer {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    shared: Arc<Shared>,
    next_id: AtomicI64,
    config: ServerConfig,
    initialized: std::sync::atomic::AtomicBool,
    capabilities: RwLock<Value>,
    /// Version counter per open document, as LSP requires.
    versions: parking_lot::Mutex<HashMap<String, i32>>,
}

impl LanguageServer {
    /// Connect over an arbitrary byte stream.
    ///
    /// Generic over the transport so the same client code is exercised in tests
    /// against an in-process server, rather than testing a different code path
    /// than production uses.
    pub fn connect<R, W>(reader: R, writer: W, config: ServerConfig) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let shared = Arc::new(Shared::default());
        let server = Arc::new(Self {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            shared: Arc::clone(&shared),
            next_id: AtomicI64::new(1),
            config,
            initialized: std::sync::atomic::AtomicBool::new(false),
            capabilities: RwLock::new(json!({})),
            versions: parking_lot::Mutex::new(HashMap::new()),
        });

        tokio::spawn(read_loop(reader, shared));
        server
    }

    /// Launch `program` and speak LSP over its stdio.
    pub async fn spawn(
        program: &str,
        args: &[String],
        config: ServerConfig,
    ) -> Result<Arc<Self>> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| LspError::Spawn {
            program: program.to_string(),
            source,
        })?;

        let stdin = child.stdin.take().ok_or(LspError::Exited)?;
        let stdout = child.stdout.take().ok_or(LspError::Exited)?;

        // A language server's stderr is where it explains why it is unhappy;
        // draining it also stops the child blocking on a full pipe.
        if let Some(stderr) = child.stderr.take() {
            let program = program.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(server = %program, "{line}");
                }
            });
        }

        // The child is owned by a task that reaps it, so it does not become a
        // zombie when the handle is dropped.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        Ok(Self::connect(stdout, stdin, config))
    }

    /// Perform the `initialize`/`initialized` exchange.
    ///
    /// Nothing else may be sent until this completes: a server that receives a
    /// request before `initialize` is entitled to reject it, and most simply
    /// stop responding.
    pub async fn initialize(&self) -> Result<Value> {
        let root_uri = self.config.root_path.as_deref().map(path_to_uri);

        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": self.config.client_name, "version": env!("CARGO_PKG_VERSION") },
            "rootUri": root_uri,
            "workspaceFolders": root_uri.as_ref().map(|uri| json!([{
                "uri": uri,
                "name": self.config.root_path.as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "workspace".to_string())
            }])),
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": true, "willSave": false },
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true,
                            "documentationFormat": ["markdown", "plaintext"]
                        }
                    },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": false },
                    "references": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "publishDiagnostics": { "relatedInformation": true }
                },
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true
                },
                "general": {
                    // Declaring only utf-16 keeps the offset contract explicit:
                    // every position on this connection is in UTF-16 code units.
                    "positionEncodings": ["utf-16"]
                }
            }
        });

        let result = self.request("initialize", params).await?;
        *self.capabilities.write() =
            result.get("capabilities").cloned().unwrap_or_else(|| json!({}));

        self.notify("initialized", json!({})).await?;
        self.initialized.store(true, Ordering::Release);
        Ok(result)
    }

    /// The server's advertised capabilities.
    pub fn capabilities(&self) -> Value {
        self.capabilities.read().clone()
    }

    /// Whether the server supports a capability, by dotted path.
    pub fn supports(&self, capability_path: &str) -> bool {
        let capabilities = self.capabilities.read();
        let mut current: &Value = &capabilities;
        for segment in capability_path.split('.') {
            match current.get(segment) {
                Some(next) => current = next,
                None => return false,
            }
        }
        !current.is_null() && current != &json!(false)
    }

    /// Tell the server a document is open.
    pub async fn did_open(
        &self,
        path: &Path,
        language_id: &str,
        text: &str,
    ) -> Result<()> {
        self.require_initialized("textDocument/didOpen")?;
        let uri = path_to_uri(path);
        self.versions.lock().insert(uri.clone(), 1);

        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        )
        .await
    }

    /// Send a full-document change.
    ///
    /// Nebula uses full sync rather than incremental. Incremental sync saves
    /// bandwidth on a local pipe that has none to spare, and every desync bug
    /// between an editor and a server traces back to it. The cost is sending the
    /// document text on each change, which is measured in microseconds for the
    /// file sizes an editor holds open.
    pub async fn did_change(&self, path: &Path, text: &str) -> Result<()> {
        self.require_initialized("textDocument/didChange")?;
        let uri = path_to_uri(path);

        let version = {
            let mut versions = self.versions.lock();
            let entry = versions.entry(uri.clone()).or_insert(1);
            *entry += 1;
            *entry
        };

        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        )
        .await
    }

    /// Tell the server a document was saved.
    pub async fn did_save(&self, path: &Path, text: Option<&str>) -> Result<()> {
        self.require_initialized("textDocument/didSave")?;
        let mut params = json!({ "textDocument": { "uri": path_to_uri(path) } });
        if let Some(text) = text {
            params["text"] = json!(text);
        }
        self.notify("textDocument/didSave", params).await
    }

    /// Tell the server a document was closed.
    pub async fn did_close(&self, path: &Path) -> Result<()> {
        self.require_initialized("textDocument/didClose")?;
        let uri = path_to_uri(path);
        self.versions.lock().remove(&uri);
        // Diagnostics for a closed document are stale by definition.
        self.shared.diagnostics.write().remove(&uri);
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } })).await
    }

    /// Request completions at a buffer offset.
    pub async fn completion(
        &self,
        path: &Path,
        buffer: &TextBuffer,
        offset: usize,
    ) -> Result<Vec<CompletionItem>> {
        self.require_initialized("textDocument/completion")?;
        let position = Position::from_offset(buffer, offset)?;

        let result = self
            .request(
                "textDocument/completion",
                json!({
                    "textDocument": { "uri": path_to_uri(path) },
                    "position": position
                }),
            )
            .await?;

        // The response is either a bare array or a CompletionList with an
        // `items` field; servers disagree and both are legal.
        let items = if result.is_array() {
            result
        } else {
            result.get("items").cloned().unwrap_or_else(|| json!([]))
        };
        Ok(serde_json::from_value(items)?)
    }

    /// Request hover text at a buffer offset.
    pub async fn hover(
        &self,
        path: &Path,
        buffer: &TextBuffer,
        offset: usize,
    ) -> Result<Option<String>> {
        self.require_initialized("textDocument/hover")?;
        let position = Position::from_offset(buffer, offset)?;

        let result = self
            .request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": path_to_uri(path) },
                    "position": position
                }),
            )
            .await?;

        Ok(extract_hover_text(&result))
    }

    /// Resolve the definition of the symbol at a buffer offset.
    pub async fn goto_definition(
        &self,
        path: &Path,
        buffer: &TextBuffer,
        offset: usize,
    ) -> Result<Vec<Location>> {
        self.require_initialized("textDocument/definition")?;
        let position = Position::from_offset(buffer, offset)?;

        let result = self
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": path_to_uri(path) },
                    "position": position
                }),
            )
            .await?;

        Ok(parse_locations(&result))
    }

    /// Find references to the symbol at a buffer offset.
    pub async fn references(
        &self,
        path: &Path,
        buffer: &TextBuffer,
        offset: usize,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        self.require_initialized("textDocument/references")?;
        let position = Position::from_offset(buffer, offset)?;

        let result = self
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": path_to_uri(path) },
                    "position": position,
                    "context": { "includeDeclaration": include_declaration }
                }),
            )
            .await?;

        Ok(parse_locations(&result))
    }

    /// The most recent diagnostics for a file.
    ///
    /// Diagnostics are pushed by the server, not requested, so this reads the
    /// latest set the reader task recorded.
    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.shared.diagnostics.read().get(&path_to_uri(path)).cloned().unwrap_or_default()
    }

    /// Diagnostics for every file, keyed by URI.
    pub fn all_diagnostics(&self) -> HashMap<String, Vec<Diagnostic>> {
        self.shared.diagnostics.read().clone()
    }

    /// Shut the server down cleanly.
    ///
    /// `shutdown` then `exit`, in that order: a server that receives `exit`
    /// without `shutdown` is entitled to treat it as a crash and may leave
    /// state behind.
    pub async fn shutdown(&self) -> Result<()> {
        if !self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        // A server that is already gone should not turn shutdown into an error.
        let _ = self.request("shutdown", json!(null)).await;
        let _ = self.notify("exit", json!(null)).await;
        self.initialized.store(false, Ordering::Release);
        Ok(())
    }

    /// Send a request and await its response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(LspError::Exited);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().insert(id, tx);

        let body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        self.write(&body).await?;

        match tokio::time::timeout(self.config.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(LspError::Exited),
            Err(_) => {
                self.shared.pending.lock().remove(&id);
                Err(LspError::Timeout { method: method.to_string(), timeout: self.config.timeout })
            }
        }
    }

    /// Send a notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))?;
        self.write(&body).await
    }

    async fn write(&self, body: &str) -> Result<()> {
        let framed = encode_message(body);
        let mut writer = self.writer.lock().await;
        writer.write_all(&framed).await?;
        writer.flush().await?;
        Ok(())
    }

    fn require_initialized(&self, method: &str) -> Result<()> {
        if self.initialized.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(LspError::NotInitialized(method.to_string()))
        }
    }
}

/// Read framed messages and dispatch them.
async fn read_loop<R: AsyncRead + Unpin>(mut reader: R, shared: Arc<Shared>) {
    let mut decoder = FrameDecoder::new();
    let mut chunk = [0u8; 16 * 1024];

    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
                tracing::debug!(%err, "language server read failed");
                break;
            }
        };
        decoder.feed(&chunk[..read]);

        loop {
            match decoder.next_message() {
                Ok(Some(message)) => dispatch(&message, &shared),
                Ok(None) => break,
                Err(err) => {
                    // A framing error means the stream position is no longer
                    // trustworthy; continuing would misinterpret every
                    // subsequent byte.
                    tracing::error!(%err, "unrecoverable framing error; dropping connection");
                    shared.closed.store(true, Ordering::Release);
                    shared.pending.lock().clear();
                    return;
                }
            }
        }
    }

    shared.closed.store(true, Ordering::Release);
    // Waking every waiter beats leaving them to time out one by one.
    shared.pending.lock().clear();
}

fn dispatch(message: &str, shared: &Arc<Shared>) {
    let Ok(value) = serde_json::from_str::<Value>(message) else {
        tracing::warn!("unparseable message from language server");
        return;
    };

    // A response carries an id and either a result or an error.
    if let Some(id) = value.get("id").and_then(|v| v.as_i64())
        && (value.get("result").is_some() || value.get("error").is_some())
    {
        let Some(sender) = shared.pending.lock().remove(&id) else {
            tracing::debug!(id, "response for an unknown request");
            return;
        };
        let outcome = match value.get("error") {
            Some(error) => Err(LspError::Server {
                code: error.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
            }),
            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = sender.send(outcome);
        return;
    }

    // Otherwise it is a notification or a server→client request.
    match value.get("method").and_then(|m| m.as_str()) {
        Some("textDocument/publishDiagnostics") => {
            let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
            let Some(uri) = params.get("uri").and_then(|u| u.as_str()) else {
                return;
            };
            let diagnostics: Vec<Diagnostic> = params
                .get("diagnostics")
                .cloned()
                .and_then(|d| serde_json::from_value(d).ok())
                .unwrap_or_default();
            shared.diagnostics.write().insert(uri.to_string(), diagnostics);
        }
        Some(other) => {
            tracing::trace!(method = other, "unhandled server message");
        }
        None => {}
    }
}

/// Pull the text out of a hover response.
///
/// Servers return three different shapes here, all legal: a MarkupContent
/// object, a MarkedString, or an array of MarkedStrings.
fn extract_hover_text(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;

    if let Some(text) = contents.as_str() {
        return Some(text.to_string());
    }
    if let Some(value) = contents.get("value").and_then(|v| v.as_str()) {
        return Some(value.to_string());
    }
    if let Some(items) = contents.as_array() {
        let joined: Vec<String> = items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .or_else(|| item.get("value").and_then(|v| v.as_str()).map(str::to_string))
            })
            .collect();
        if !joined.is_empty() {
            return Some(joined.join("\n"));
        }
    }
    None
}

/// Parse a definition/references response.
///
/// May be a single Location, an array of Locations, an array of LocationLinks,
/// or null.
fn parse_locations(result: &Value) -> Vec<Location> {
    fn one(value: &Value) -> Option<Location> {
        if let Ok(location) = serde_json::from_value::<Location>(value.clone()) {
            return Some(location);
        }
        // LocationLink: `targetUri` plus `targetSelectionRange`.
        let uri = value.get("targetUri")?.as_str()?.to_string();
        let range = value
            .get("targetSelectionRange")
            .or_else(|| value.get("targetRange"))?
            .clone();
        Some(Location { uri, range: serde_json::from_value(range).ok()? })
    }

    match result {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DiagnosticSeverity, Range};
    use tokio::io::{AsyncWriteExt, duplex};

    /// A real LSP server implementation, driven over an in-memory duplex.
    ///
    /// It parses the framing, enforces the lifecycle, and answers with
    /// spec-shaped payloads — including the awkward response shapes real servers
    /// produce. Testing against it exercises exactly the code path production
    /// uses, unlike stubbing out the transport.
    struct TestServer {
        reader: tokio::io::DuplexStream,
        writer: tokio::io::DuplexStream,
        decoder: FrameDecoder,
        initialized: bool,
    }

    impl TestServer {
        async fn write(&mut self, value: Value) {
            let body = serde_json::to_string(&value).unwrap();
            self.writer.write_all(&encode_message(&body)).await.unwrap();
            self.writer.flush().await.unwrap();
        }

        /// Read one message from the client.
        async fn next(&mut self) -> Value {
            loop {
                if let Some(message) = self.decoder.next_message().unwrap() {
                    return serde_json::from_str(&message).unwrap();
                }
                let mut chunk = [0u8; 4096];
                let read = self.reader.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed the connection unexpectedly");
                self.decoder.feed(&chunk[..read]);
            }
        }

        /// Serve requests until the client disconnects.
        async fn serve(mut self) {
            loop {
                let message = tokio::time::timeout(Duration::from_secs(5), self.next()).await;
                let Ok(message) = message else {
                    return;
                };
                let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = message.get("id").cloned();

                match method {
                    "initialize" => {
                        self.initialized = true;
                        self.write(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "capabilities": {
                                    "completionProvider": { "triggerCharacters": ["."] },
                                    "hoverProvider": true,
                                    "definitionProvider": true,
                                    "referencesProvider": false,
                                    "textDocumentSync": 1
                                },
                                "serverInfo": { "name": "test-lsp", "version": "1.0" }
                            }
                        }))
                        .await;
                    }
                    "initialized" => {
                        // Push a diagnostic unprompted, as a real server does
                        // once it has indexed the workspace.
                        self.write(json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {
                                "uri": "file:///project/src/main.rs",
                                "diagnostics": [{
                                    "range": {
                                        "start": { "line": 1, "character": 4 },
                                        "end": { "line": 1, "character": 9 }
                                    },
                                    "severity": 2,
                                    "code": "unused_variables",
                                    "source": "rustc",
                                    "message": "unused variable: `value`"
                                }]
                            }
                        }))
                        .await;
                    }
                    "textDocument/completion" => {
                        // The CompletionList shape, with sortText overriding
                        // alphabetical order.
                        self.write(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "isIncomplete": false,
                                "items": [
                                    { "label": "zebra", "kind": 6, "sortText": "0000" },
                                    { "label": "apple", "kind": 3, "insertText": "apple()" }
                                ]
                            }
                        }))
                        .await;
                    }
                    "textDocument/hover" => {
                        self.write(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "contents": { "kind": "markdown", "value": "```rust\nfn main()\n```" }
                            }
                        }))
                        .await;
                    }
                    "textDocument/definition" => {
                        // The LocationLink shape, which a naive parser misses.
                        self.write(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": [{
                                "targetUri": "file:///project/src/lib.rs",
                                "targetRange": {
                                    "start": { "line": 10, "character": 0 },
                                    "end": { "line": 12, "character": 1 }
                                },
                                "targetSelectionRange": {
                                    "start": { "line": 10, "character": 7 },
                                    "end": { "line": 10, "character": 13 }
                                }
                            }]
                        }))
                        .await;
                    }
                    "textDocument/references" => {
                        self.write(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "references are not supported" }
                        }))
                        .await;
                    }
                    "shutdown" => {
                        self.write(json!({ "jsonrpc": "2.0", "id": id, "result": null })).await;
                    }
                    "exit" => return,
                    _ => {
                        if let Some(id) = id {
                            self.write(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32601, "message": format!("no such method: {method}") }
                            }))
                            .await;
                        }
                    }
                }
            }
        }
    }

    /// Start a client wired to a running test server.
    async fn connected() -> Arc<LanguageServer> {
        let (client_side, server_side) = duplex(64 * 1024);
        let (server_read, client_read) = duplex(64 * 1024);

        let server = TestServer {
            reader: server_side,
            writer: server_read,
            decoder: FrameDecoder::new(),
            initialized: false,
        };
        tokio::spawn(server.serve());

        let config = ServerConfig {
            root_path: Some(std::path::PathBuf::from("/project")),
            timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let client = LanguageServer::connect(client_read, client_side, config);
        client.initialize().await.unwrap();
        client
    }

    #[tokio::test]
    async fn initialize_records_the_servers_capabilities() {
        let client = connected().await;
        assert!(client.supports("hoverProvider"));
        assert!(client.supports("completionProvider"));
        assert!(client.supports("completionProvider.triggerCharacters"));
        assert!(
            !client.supports("referencesProvider"),
            "a capability explicitly set to false must read as unsupported"
        );
        assert!(!client.supports("nonexistentProvider"));
    }

    #[tokio::test]
    async fn requests_before_initialize_are_refused_locally() {
        let (client_side, _server_side) = duplex(1024);
        let (_server_read, client_read) = duplex(1024);
        let client =
            LanguageServer::connect(client_read, client_side, ServerConfig::default());

        let buffer = TextBuffer::from_str("fn main() {}");
        let err = client
            .completion(Path::new("/project/src/main.rs"), &buffer, 0)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LspError::NotInitialized(_)),
            "a request before initialize hangs most servers, so it is caught here: {err:?}"
        );
    }

    #[tokio::test]
    async fn completions_parse_from_the_completion_list_shape() {
        let client = connected().await;
        let path = Path::new("/project/src/main.rs");
        let buffer = TextBuffer::from_str("fn main() {\n    let value = 1;\n}\n");

        client.did_open(path, "rust", &buffer.to_string()).await.unwrap();
        let items = client.completion(path, &buffer, 20).await.unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "zebra");
        assert_eq!(items[1].insertion(), "apple()", "insertText overrides the label");

        let mut sorted = items.clone();
        sorted.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));
        assert_eq!(sorted[0].label, "zebra", "the server's sortText wins");
    }

    #[tokio::test]
    async fn hover_extracts_markup_content() {
        let client = connected().await;
        let path = Path::new("/project/src/main.rs");
        let buffer = TextBuffer::from_str("fn main() {}");
        client.did_open(path, "rust", &buffer.to_string()).await.unwrap();

        let hover = client.hover(path, &buffer, 3).await.unwrap();
        assert_eq!(hover.as_deref(), Some("```rust\nfn main()\n```"));
    }

    #[tokio::test]
    async fn goto_definition_parses_the_location_link_shape() {
        let client = connected().await;
        let path = Path::new("/project/src/main.rs");
        let buffer = TextBuffer::from_str("fn main() { helper(); }");
        client.did_open(path, "rust", &buffer.to_string()).await.unwrap();

        let locations = client.goto_definition(path, &buffer, 12).await.unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].uri, "file:///project/src/lib.rs");
        assert_eq!(
            locations[0].range.start,
            Position::new(10, 7),
            "targetSelectionRange is the one to jump to, not targetRange"
        );
        assert_eq!(
            locations[0].path().unwrap(),
            std::path::PathBuf::from("/project/src/lib.rs")
        );
    }

    #[tokio::test]
    async fn a_server_error_response_becomes_an_error() {
        let client = connected().await;
        let path = Path::new("/project/src/main.rs");
        let buffer = TextBuffer::from_str("fn main() {}");
        client.did_open(path, "rust", &buffer.to_string()).await.unwrap();

        let err = client.references(path, &buffer, 3, true).await.unwrap_err();
        match err {
            LspError::Server { code, message } => {
                assert_eq!(code, -32601);
                assert!(message.contains("references"));
            }
            other => panic!("expected a server error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pushed_diagnostics_are_recorded() {
        let client = connected().await;
        let path = Path::new("/project/src/main.rs");

        // Diagnostics arrive unprompted; wait for the push rather than sleeping
        // a fixed amount.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while client.diagnostics(path).is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let diagnostics = client.diagnostics(path);
        assert_eq!(diagnostics.len(), 1, "expected the pushed diagnostic");
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::Warning));
        assert_eq!(diagnostics[0].message, "unused variable: `value`");
        assert_eq!(diagnostics[0].range.start, Position::new(1, 4));
    }

    #[tokio::test]
    async fn closing_a_document_clears_its_diagnostics() {
        let client = connected().await;
        let path = Path::new("/project/src/main.rs");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while client.diagnostics(path).is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!client.diagnostics(path).is_empty());

        client.did_open(path, "rust", "fn main() {}").await.unwrap();
        client.did_close(path).await.unwrap();
        assert!(
            client.diagnostics(path).is_empty(),
            "diagnostics for a closed document are stale"
        );
    }

    #[tokio::test]
    async fn document_versions_increment_on_every_change() {
        let client = connected().await;
        let path = Path::new("/project/src/main.rs");

        client.did_open(path, "rust", "one").await.unwrap();
        client.did_change(path, "two").await.unwrap();
        client.did_change(path, "three").await.unwrap();

        let uri = path_to_uri(path);
        assert_eq!(
            client.versions.lock().get(&uri).copied(),
            Some(3),
            "a version that does not advance makes a server ignore the change"
        );
    }

    #[tokio::test]
    async fn a_request_to_a_dead_server_fails_rather_than_hanging() {
        let (client_side, server_side) = duplex(1024);
        let (server_read, client_read) = duplex(1024);
        drop(server_side);
        drop(server_read);

        let client =
            LanguageServer::connect(client_read, client_side, ServerConfig::default());
        let err = client.request("initialize", json!({})).await.unwrap_err();
        assert!(matches!(err, LspError::Exited | LspError::Io(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_request_that_is_never_answered_times_out() {
        let (client_side, _server_side) = duplex(64 * 1024);
        let (_server_read, client_read) = duplex(64 * 1024);

        let config = ServerConfig { timeout: Duration::from_millis(200), ..Default::default() };
        let client = LanguageServer::connect(client_read, client_side, config);

        let started = std::time::Instant::now();
        let err = client.request("initialize", json!({})).await.unwrap_err();
        assert!(matches!(err, LspError::Timeout { .. }), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn shutdown_is_clean_and_idempotent() {
        let client = connected().await;
        client.shutdown().await.unwrap();
        // A second shutdown after the server is gone must not error.
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn spawning_a_missing_server_is_reported() {
        let result = LanguageServer::spawn(
            "nebula-definitely-not-a-language-server",
            &[],
            ServerConfig::default(),
        )
        .await;
        assert!(
            matches!(result.as_ref().err(), Some(LspError::Spawn { .. })),
            "expected a spawn error"
        );
    }

    #[test]
    fn hover_parses_every_shape_servers_send() {
        assert_eq!(
            extract_hover_text(&json!({ "contents": "plain string" })).as_deref(),
            Some("plain string")
        );
        assert_eq!(
            extract_hover_text(&json!({ "contents": { "kind": "markdown", "value": "**bold**" } }))
                .as_deref(),
            Some("**bold**")
        );
        assert_eq!(
            extract_hover_text(&json!({ "contents": ["first", { "value": "second" }] })).as_deref(),
            Some("first\nsecond")
        );
        assert_eq!(extract_hover_text(&json!(null)), None);
        assert_eq!(extract_hover_text(&json!({ "contents": [] })), None);
    }

    #[test]
    fn locations_parse_from_every_shape_servers_send() {
        let range = json!({
            "start": { "line": 1, "character": 2 },
            "end": { "line": 1, "character": 8 }
        });

        // A single Location.
        let single = parse_locations(&json!({ "uri": "file:///a.rs", "range": range }));
        assert_eq!(single.len(), 1);

        // An array of Locations.
        let array = parse_locations(&json!([
            { "uri": "file:///a.rs", "range": range },
            { "uri": "file:///b.rs", "range": range }
        ]));
        assert_eq!(array.len(), 2);

        // An array of LocationLinks.
        let links = parse_locations(&json!([{
            "targetUri": "file:///c.rs",
            "targetRange": range,
            "targetSelectionRange": range
        }]));
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].uri, "file:///c.rs");

        // Null, which means "no definition found".
        assert!(parse_locations(&json!(null)).is_empty());
    }

    #[test]
    fn a_location_link_without_a_selection_range_falls_back_to_the_target_range() {
        let range = json!({
            "start": { "line": 3, "character": 0 },
            "end": { "line": 5, "character": 1 }
        });
        let parsed = parse_locations(&json!([{
            "targetUri": "file:///d.rs",
            "targetRange": range
        }]));
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].range,
            Range::new(Position::new(3, 0), Position::new(5, 1))
        );
    }
}
