//! The MCP client.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde_json::json;

use crate::protocol::{InputRequest, JsonRpcRequest, Prompt, Resource, Tool, ToolResult};
use crate::transport::{IdAllocator, RequestHeaders, Transport};
use crate::version::{Capabilities, ProtocolVersion, negotiate};
use crate::{McpError, Result};

/// Client configuration.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Client name sent to servers.
    pub client_name: String,
    /// Client version sent to servers.
    pub client_version: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// How many MRTR rounds to allow before giving up.
    ///
    /// A server that keeps asking for input without ever resolving would
    /// otherwise loop forever.
    pub max_rounds: usize,
    /// What the client can do.
    pub capabilities: Capabilities,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            client_name: "nebula-ide".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            timeout: Duration::from_secs(60),
            max_rounds: 8,
            capabilities: Capabilities::client_default(),
        }
    }
}

/// A cached list result, with the TTL the server attached.
#[derive(Debug, Clone)]
struct CachedList<T> {
    items: Vec<T>,
    fetched_at: Instant,
    ttl: Option<Duration>,
}

impl<T> CachedList<T> {
    fn is_fresh(&self) -> bool {
        match self.ttl {
            // No TTL means the server did not authorise caching, so the entry is
            // never reused.
            None => false,
            Some(ttl) => self.fetched_at.elapsed() < ttl,
        }
    }
}

/// Answers the client supplies when a server asks for input.
///
/// Implemented by the agent layer, which decides whether to prompt the user,
/// answer from context, or refuse.
pub trait InputProvider: Send + Sync {
    /// Answer one input request, or return `None` to decline.
    fn provide(&self, request: &InputRequest) -> Option<serde_json::Value>;
}

/// An input provider that declines everything.
///
/// The right default: a server should not be able to extract information from
/// the user by asking, unless the caller has deliberately wired up a provider
/// that knows how to prompt.
pub struct DeclineAll;

impl InputProvider for DeclineAll {
    fn provide(&self, _request: &InputRequest) -> Option<serde_json::Value> {
        None
    }
}

/// A connected MCP server.
pub struct Client {
    transport: Arc<dyn Transport>,
    config: ClientConfig,
    version: ProtocolVersion,
    server_capabilities: Capabilities,
    server_name: String,
    ids: IdAllocator,
    tools_cache: RwLock<Option<CachedList<Tool>>>,
    prompts_cache: RwLock<Option<CachedList<Prompt>>>,
    resources_cache: RwLock<Option<CachedList<Resource>>>,
    input_provider: Arc<dyn InputProvider>,
}

/// A handle describing a connected server, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHandle {
    /// Server-reported name.
    pub name: String,
    /// The negotiated protocol revision.
    pub version: ProtocolVersion,
    /// What the server said it can do.
    pub capabilities: Capabilities,
    /// How the client reaches it.
    pub transport: String,
}

impl Client {
    /// Connect over `transport`, negotiating the protocol revision.
    ///
    /// On a stateless server this sends `server/discover`; on an older one it
    /// performs the `initialize`/`initialized` handshake. Which path runs is
    /// decided by what the server answers, not by configuration.
    pub async fn connect(transport: Arc<dyn Transport>, config: ClientConfig) -> Result<Self> {
        let ids = IdAllocator::default();

        // Try the stateless path first. A 2026-07-28 server answers
        // `server/discover`; an older one returns method-not-found, which is the
        // signal to fall back to the handshake.
        let discover = JsonRpcRequest::new(ids.next(), "server/discover").with_meta(json!({
            "protocolVersion": ProtocolVersion::PREFERRED.as_str(),
            "client": { "name": config.client_name, "version": config.client_version },
        }));
        let headers = RequestHeaders {
            method: Some("server/discover".to_string()),
            name: None,
            protocol_version: Some(ProtocolVersion::PREFERRED.as_str().to_string()),
        };

        let discovered = transport.request(discover, &headers).await;

        let (version, server_capabilities, server_name) = match discovered {
            Ok(response) if response.error.is_none() => {
                let result = response.result.unwrap_or_else(|| json!({}));
                let versions = result
                    .get("protocolVersions")
                    .and_then(|v| v.as_array())
                    .map(|items| items.iter().filter_map(|i| i.as_str()).collect::<Vec<_>>())
                    .or_else(|| {
                        result.get("protocolVersion").and_then(|v| v.as_str()).map(|v| vec![v])
                    })
                    .unwrap_or_else(|| vec![ProtocolVersion::PREFERRED.as_str()]);

                let version = negotiate(&versions).ok_or_else(|| {
                    McpError::Protocol(format!(
                        "no shared protocol revision; server offers {versions:?}, client supports {:?}",
                        ProtocolVersion::SUPPORTED
                            .iter()
                            .map(|v| v.as_str())
                            .collect::<Vec<_>>()
                    ))
                })?;

                let name = result
                    .get("serverInfo")
                    .and_then(|i| i.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                (version, Capabilities::from_json(&result), name)
            }
            // Either an explicit error or a transport-level failure: fall back
            // to the stateful handshake.
            _ => Self::handshake(&transport, &config, &ids).await?,
        };

        tracing::info!(
            server = %server_name,
            version = %version,
            transport = %transport.describe(),
            "connected to MCP server"
        );

        Ok(Self {
            transport,
            config,
            version,
            server_capabilities,
            server_name,
            ids,
            tools_cache: RwLock::new(None),
            prompts_cache: RwLock::new(None),
            resources_cache: RwLock::new(None),
            input_provider: Arc::new(DeclineAll),
        })
    }

    /// The stateful `initialize`/`initialized` exchange, for pre-2026 servers.
    async fn handshake(
        transport: &Arc<dyn Transport>,
        config: &ClientConfig,
        ids: &IdAllocator,
    ) -> Result<(ProtocolVersion, Capabilities, String)> {
        let request = JsonRpcRequest::new(ids.next(), "initialize").with_params(json!({
            "protocolVersion": ProtocolVersion::V2025_11_25.as_str(),
            "capabilities": {
                "roots": { "listChanged": true },
                "sampling": {},
                "elicitation": {},
            },
            "clientInfo": { "name": config.client_name, "version": config.client_version },
        }));
        let headers = RequestHeaders {
            method: Some("initialize".to_string()),
            name: None,
            protocol_version: Some(ProtocolVersion::V2025_11_25.as_str().to_string()),
        };

        let response = transport.request(request, &headers).await?;
        if let Some(error) = response.error {
            return Err(McpError::Server {
                code: error.code,
                message: error.message,
                data: error.data,
            });
        }
        let result = response.result.unwrap_or_else(|| json!({}));

        let reported = result.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("");
        let version = ProtocolVersion::parse(reported).ok_or_else(|| {
            McpError::Protocol(format!("server reported unsupported protocol `{reported}`"))
        })?;

        let name = result
            .get("serverInfo")
            .and_then(|i| i.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string();

        // The handshake is only complete once the client acknowledges.
        transport
            .notify(JsonRpcRequest::notification("notifications/initialized"), &headers)
            .await?;

        Ok((version, Capabilities::from_json(&result), name))
    }

    /// Install a provider that answers server input requests.
    pub fn with_input_provider(mut self, provider: Arc<dyn InputProvider>) -> Self {
        self.input_provider = provider;
        self
    }

    /// The negotiated protocol revision.
    pub fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// What the server said it can do.
    pub fn capabilities(&self) -> &Capabilities {
        &self.server_capabilities
    }

    /// A handle describing this connection.
    pub fn handle(&self) -> ServerHandle {
        ServerHandle {
            name: self.server_name.clone(),
            version: self.version,
            capabilities: self.server_capabilities.clone(),
            transport: self.transport.describe(),
        }
    }

    /// List the server's tools, using the cache when the server allowed it.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        if let Some(cached) = self.tools_cache.read().as_ref()
            && cached.is_fresh()
        {
            return Ok(cached.items.clone());
        }
        if !self.server_capabilities.tools {
            return Ok(Vec::new());
        }

        let result = self.call_method("tools/list", None, json!({})).await?;
        let tools: Vec<Tool> =
            serde_json::from_value(result.get("tools").cloned().unwrap_or_else(|| json!([])))?;

        *self.tools_cache.write() = Some(CachedList {
            items: tools.clone(),
            fetched_at: Instant::now(),
            ttl: self.cache_ttl(&result),
        });
        Ok(tools)
    }

    /// List the server's prompts.
    pub async fn list_prompts(&self) -> Result<Vec<Prompt>> {
        if let Some(cached) = self.prompts_cache.read().as_ref()
            && cached.is_fresh()
        {
            return Ok(cached.items.clone());
        }
        if !self.server_capabilities.prompts {
            return Ok(Vec::new());
        }

        let result = self.call_method("prompts/list", None, json!({})).await?;
        let prompts: Vec<Prompt> =
            serde_json::from_value(result.get("prompts").cloned().unwrap_or_else(|| json!([])))?;

        *self.prompts_cache.write() = Some(CachedList {
            items: prompts.clone(),
            fetched_at: Instant::now(),
            ttl: self.cache_ttl(&result),
        });
        Ok(prompts)
    }

    /// List the server's resources.
    pub async fn list_resources(&self) -> Result<Vec<Resource>> {
        if let Some(cached) = self.resources_cache.read().as_ref()
            && cached.is_fresh()
        {
            return Ok(cached.items.clone());
        }
        if !self.server_capabilities.resources {
            return Ok(Vec::new());
        }

        let result = self.call_method("resources/list", None, json!({})).await?;
        let resources: Vec<Resource> =
            serde_json::from_value(result.get("resources").cloned().unwrap_or_else(|| json!([])))?;

        *self.resources_cache.write() = Some(CachedList {
            items: resources.clone(),
            fetched_at: Instant::now(),
            ttl: self.cache_ttl(&result),
        });
        Ok(resources)
    }

    /// Read a resource.
    pub async fn read_resource(&self, uri: &str) -> Result<serde_json::Value> {
        self.call_method("resources/read", Some(uri), json!({ "uri": uri })).await
    }

    /// Call a tool, resolving any Multi Round-Trip input requests.
    ///
    /// On 2026-07-28 a server that needs input returns
    /// `resultType: "input_required"` with the questions it needs answered; the
    /// client retries the *same* call with `inputResponses` filled in. That loop
    /// lives here, bounded by [`ClientConfig::max_rounds`], so callers see a
    /// single request/response.
    pub async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolResult> {
        let mut responses: HashMap<String, serde_json::Value> = HashMap::new();

        for round in 0..self.config.max_rounds {
            let mut params = json!({ "name": name, "arguments": arguments });
            if !responses.is_empty() {
                params["inputResponses"] = json!(responses);
            }

            let result = self.call_method("tools/call", Some(name), params).await?;

            let needs_input =
                result.get("resultType").and_then(|v| v.as_str()) == Some("input_required");
            if !needs_input {
                return Ok(serde_json::from_value(result)?);
            }
            if !self.version.uses_mrtr() {
                return Err(McpError::Protocol(format!(
                    "server returned an MRTR input_required result on protocol {}",
                    self.version
                )));
            }

            let requests: Vec<InputRequest> = serde_json::from_value(
                result.get("inputRequests").cloned().unwrap_or_else(|| json!([])),
            )?;
            if requests.is_empty() {
                return Err(McpError::Protocol(
                    "server asked for input but listed no requests".to_string(),
                ));
            }

            let mut answered_any = false;
            for request in &requests {
                match self.input_provider.provide(request) {
                    Some(answer) => {
                        responses.insert(request.id.clone(), answer);
                        answered_any = true;
                    }
                    None => {
                        // Declining is a legitimate answer and must be recorded,
                        // or the next round asks the same question forever.
                        responses.insert(request.id.clone(), json!({ "declined": true }));
                    }
                }
            }
            tracing::debug!(
                server = %self.server_name,
                round,
                answered = answered_any,
                "resolved an MRTR round"
            );
        }

        Err(McpError::TooManyRounds(self.config.max_rounds))
    }

    /// Get a prompt, with arguments substituted by the server.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.call_method("prompts/get", Some(name), json!({ "name": name, "arguments": arguments }))
            .await
    }

    /// Disconnect.
    pub async fn close(&self) -> Result<()> {
        self.transport.close().await
    }

    /// Issue one JSON-RPC call with the right envelope for the negotiated
    /// revision.
    async fn call_method(
        &self,
        method: &str,
        name: Option<&str>,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut request = JsonRpcRequest::new(self.ids.next(), method).with_params(params);

        // The stateless core carries identity and capabilities in every request,
        // because any request may land on any instance behind a load balancer.
        if self.version.is_stateless() {
            request = request.with_meta(json!({
                "protocolVersion": self.version.as_str(),
                "client": {
                    "name": self.config.client_name,
                    "version": self.config.client_version,
                },
                "capabilities": {
                    "elicitation": self.config.capabilities.elicitation,
                    "tasks": self.config.capabilities.tasks,
                },
            }));
        }

        let headers = if self.version.requires_routing_headers() {
            RequestHeaders {
                method: Some(method.to_string()),
                name: name.map(str::to_string),
                protocol_version: Some(self.version.as_str().to_string()),
            }
        } else {
            RequestHeaders {
                method: None,
                name: None,
                protocol_version: Some(self.version.as_str().to_string()),
            }
        };

        let response = self.transport.request(request, &headers).await?;
        if let Some(error) = response.error {
            return Err(McpError::Server {
                code: error.code,
                message: error.message,
                data: error.data,
            });
        }
        Ok(response.result.unwrap_or_else(|| json!({})))
    }

    /// The TTL a list result authorises, if this revision allows caching.
    fn cache_ttl(&self, result: &serde_json::Value) -> Option<Duration> {
        if !self.version.supports_cacheable_lists() {
            return None;
        }
        result.get("ttlMs").and_then(|v| v.as_u64()).filter(|ms| *ms > 0).map(Duration::from_millis)
    }

    /// Drop every cached list, e.g. after a `listChanged` notification.
    pub fn invalidate_caches(&self) {
        *self.tools_cache.write() = None;
        *self.prompts_cache.write() = None;
        *self.resources_cache.write() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::JsonRpcResponse;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// An in-process MCP server implementing the revision it is configured with.
    ///
    /// This is a real server, not a stand-in: it parses requests, enforces the
    /// handshake when the revision requires one, and produces spec-shaped
    /// responses including MRTR input requests. It is the only practical way to
    /// test both sides of a protocol adapter, since the point of the adapter is
    /// behaviour that differs by revision.
    struct TestServer {
        version: ProtocolVersion,
        /// Every request the server saw, for assertions.
        seen: Mutex<Vec<(JsonRpcRequest, RequestHeaders)>>,
        /// Whether `initialize` has been received, for stateful revisions.
        initialized: Mutex<bool>,
        /// How many rounds a call to `needs_input` should demand.
        input_rounds: usize,
        /// TTL advertised on list results, in milliseconds.
        list_ttl_ms: Option<u64>,
    }

    impl TestServer {
        fn new(version: ProtocolVersion) -> Self {
            Self {
                version,
                seen: Mutex::new(Vec::new()),
                initialized: Mutex::new(false),
                input_rounds: 0,
                list_ttl_ms: None,
            }
        }

        fn with_input_rounds(mut self, rounds: usize) -> Self {
            self.input_rounds = rounds;
            self
        }

        fn with_list_ttl(mut self, ms: u64) -> Self {
            self.list_ttl_ms = Some(ms);
            self
        }

        fn requests_for(&self, method: &str) -> Vec<(JsonRpcRequest, RequestHeaders)> {
            self.seen.lock().unwrap().iter().filter(|(r, _)| r.method == method).cloned().collect()
        }

        fn call_count(&self, method: &str) -> usize {
            self.requests_for(method).len()
        }

        fn handle(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
            let id = request.id.clone().unwrap_or(json!(0));
            match request.method.as_str() {
                "server/discover" => {
                    if !self.version.supports_discover() {
                        return JsonRpcResponse::failure(
                            Some(id),
                            crate::protocol::error_codes::METHOD_NOT_FOUND,
                            "server/discover is not available on this protocol revision",
                        );
                    }
                    JsonRpcResponse::success(
                        id,
                        json!({
                            "protocolVersions": [self.version.as_str()],
                            "serverInfo": { "name": "test-server", "version": "1.0.0" },
                            "tools": { "count": 2 },
                            "prompts": {},
                            "resources": {},
                            "extensions": ["io.modelcontextprotocol/tasks"],
                        }),
                    )
                }
                "initialize" => {
                    *self.initialized.lock().unwrap() = true;
                    JsonRpcResponse::success(
                        id,
                        json!({
                            "protocolVersion": self.version.as_str(),
                            "serverInfo": { "name": "test-server", "version": "1.0.0" },
                            "capabilities": { "tools": {}, "prompts": {}, "resources": {} },
                        }),
                    )
                }
                "tools/list" => {
                    if self.version.requires_handshake() && !*self.initialized.lock().unwrap() {
                        return JsonRpcResponse::failure(
                            Some(id),
                            crate::protocol::error_codes::INVALID_REQUEST,
                            "not initialized",
                        );
                    }
                    let mut result = json!({
                        "tools": [
                            {
                                "name": "echo",
                                "description": "Echoes its input",
                                "inputSchema": { "type": "object" },
                                "annotations": { "readOnlyHint": true }
                            },
                            {
                                "name": "needs_input",
                                "description": "Asks a question first",
                                "inputSchema": { "type": "object" }
                            }
                        ]
                    });
                    if let Some(ttl) = self.list_ttl_ms {
                        result["ttlMs"] = json!(ttl);
                        result["cacheScope"] = json!("session");
                    }
                    JsonRpcResponse::success(id, result)
                }
                "prompts/list" => JsonRpcResponse::success(
                    id,
                    json!({ "prompts": [{ "name": "review", "description": "Review code" }] }),
                ),
                "resources/list" => JsonRpcResponse::success(
                    id,
                    json!({ "resources": [{ "uri": "file:///a.txt", "name": "a" }] }),
                ),
                "tools/call" => {
                    let params = request.params.clone().unwrap_or_else(|| json!({}));
                    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");

                    if name == "needs_input" && self.input_rounds > 0 {
                        let answered = params
                            .get("inputResponses")
                            .and_then(|v| v.as_object())
                            .map(|o| o.len())
                            .unwrap_or(0);
                        if answered < self.input_rounds {
                            return JsonRpcResponse::success(
                                id,
                                json!({
                                    "resultType": "input_required",
                                    "inputRequests": [{
                                        "id": format!("q{answered}"),
                                        "type": "elicitation",
                                        "params": { "message": "Which branch?" }
                                    }]
                                }),
                            );
                        }
                        return JsonRpcResponse::success(
                            id,
                            json!({
                                "content": [{ "type": "text", "text": format!("answered {answered}") }]
                            }),
                        );
                    }

                    if name == "explode" {
                        return JsonRpcResponse::failure(
                            Some(id),
                            crate::protocol::error_codes::INTERNAL_ERROR,
                            "tool blew up",
                        );
                    }
                    if name == "reports_error" {
                        return JsonRpcResponse::success(
                            id,
                            json!({
                                "content": [{ "type": "text", "text": "could not do that" }],
                                "isError": true
                            }),
                        );
                    }

                    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    JsonRpcResponse::success(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": arguments.to_string() }]
                        }),
                    )
                }
                other => JsonRpcResponse::failure(
                    Some(id),
                    crate::protocol::error_codes::METHOD_NOT_FOUND,
                    format!("no such method: {other}"),
                ),
            }
        }
    }

    /// Wires a `TestServer` to a `Transport` without any I/O.
    struct DirectTransport(Arc<TestServer>);

    #[async_trait]
    impl Transport for DirectTransport {
        async fn request(
            &self,
            request: JsonRpcRequest,
            headers: &RequestHeaders,
        ) -> Result<JsonRpcResponse> {
            self.0.seen.lock().unwrap().push((request.clone(), headers.clone()));
            Ok(self.0.handle(&request))
        }

        async fn notify(&self, request: JsonRpcRequest, headers: &RequestHeaders) -> Result<()> {
            self.0.seen.lock().unwrap().push((request, headers.clone()));
            Ok(())
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }

        fn describe(&self) -> String {
            "test://direct".to_string()
        }
    }

    async fn connect(server: Arc<TestServer>) -> Client {
        let transport: Arc<dyn Transport> = Arc::new(DirectTransport(Arc::clone(&server)));
        Client::connect(transport, ClientConfig::default()).await.unwrap()
    }

    #[tokio::test]
    async fn a_stateless_server_is_discovered_without_a_handshake() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(Arc::clone(&server)).await;

        assert_eq!(client.version(), ProtocolVersion::V2026_07_28);
        assert_eq!(server.call_count("server/discover"), 1);
        assert_eq!(server.call_count("initialize"), 0, "the stateless core has no handshake");
        assert_eq!(
            server.call_count("notifications/initialized"),
            0,
            "and no initialized notification"
        );
    }

    #[tokio::test]
    async fn an_older_server_falls_back_to_the_handshake() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2025_11_25));
        let client = connect(Arc::clone(&server)).await;

        assert_eq!(client.version(), ProtocolVersion::V2025_11_25);
        assert_eq!(server.call_count("initialize"), 1);
        assert_eq!(
            server.call_count("notifications/initialized"),
            1,
            "the handshake is only complete once acknowledged"
        );
        assert!(*server.initialized.lock().unwrap());
    }

    #[tokio::test]
    async fn the_oldest_supported_revision_still_works() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2025_06_18));
        let client = connect(Arc::clone(&server)).await;
        assert_eq!(client.version(), ProtocolVersion::V2025_06_18);

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[tokio::test]
    async fn stateless_requests_are_self_describing() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(Arc::clone(&server)).await;
        client.list_tools().await.unwrap();

        let (request, _) = server.requests_for("tools/list").pop().unwrap();
        let meta = &request.params.unwrap()["_meta"];
        assert_eq!(
            meta["protocolVersion"], "2026-07-28",
            "every stateless request must carry its own version"
        );
        assert_eq!(meta["client"]["name"], "nebula-ide");
        assert!(meta["capabilities"].is_object());
    }

    #[tokio::test]
    async fn older_revisions_do_not_get_stateless_meta() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2025_11_25));
        let client = connect(Arc::clone(&server)).await;
        client.list_tools().await.unwrap();

        let (request, _) = server.requests_for("tools/list").pop().unwrap();
        let params = request.params.unwrap_or_else(|| json!({}));
        assert!(
            params.get("_meta").is_none(),
            "a 2025 server must not receive stateless-core metadata"
        );
    }

    #[tokio::test]
    async fn routing_headers_are_sent_only_where_they_are_required() {
        let new_server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(Arc::clone(&new_server)).await;
        client.call_tool("echo", json!({ "a": 1 })).await.unwrap();

        let (_, headers) = new_server.requests_for("tools/call").pop().unwrap();
        assert_eq!(headers.method.as_deref(), Some("tools/call"));
        assert_eq!(
            headers.name.as_deref(),
            Some("echo"),
            "a gateway routes on Mcp-Name without parsing the body"
        );

        let old_server = Arc::new(TestServer::new(ProtocolVersion::V2025_06_18));
        let client = connect(Arc::clone(&old_server)).await;
        client.call_tool("echo", json!({})).await.unwrap();

        let (_, headers) = old_server.requests_for("tools/call").pop().unwrap();
        assert_eq!(headers.method, None, "older revisions have no routing headers");
    }

    #[tokio::test]
    async fn tools_prompts_and_resources_are_listed() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(server).await;

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools[0].name, "echo");
        assert!(tools[0].is_read_only());

        assert_eq!(client.list_prompts().await.unwrap()[0].name, "review");
        assert_eq!(client.list_resources().await.unwrap()[0].uri, "file:///a.txt");
    }

    #[tokio::test]
    async fn calling_a_tool_returns_its_content() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(server).await;

        let result = client.call_tool("echo", json!({ "message": "hi" })).await.unwrap();
        assert!(result.text().contains("hi"));
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn a_tool_reported_failure_is_returned_not_raised() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(server).await;

        // The call succeeded; the tool is telling the model it failed. That has
        // to reach the model, not become a client-side error.
        let result = client.call_tool("reports_error", json!({})).await.unwrap();
        assert!(result.is_error);
        assert_eq!(result.text(), "could not do that");
    }

    #[tokio::test]
    async fn a_jsonrpc_error_becomes_a_client_error() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(server).await;

        let err = client.call_tool("explode", json!({})).await.unwrap_err();
        match err {
            McpError::Server { code, message, .. } => {
                assert_eq!(code, crate::protocol::error_codes::INTERNAL_ERROR);
                assert_eq!(message, "tool blew up");
            }
            other => panic!("expected a server error, got {other:?}"),
        }
    }

    /// Answers every elicitation with a fixed value.
    struct FixedAnswer(serde_json::Value);

    impl InputProvider for FixedAnswer {
        fn provide(&self, _request: &InputRequest) -> Option<serde_json::Value> {
            Some(self.0.clone())
        }
    }

    #[tokio::test]
    async fn multi_round_trip_input_is_resolved_transparently() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28).with_input_rounds(2));
        let transport: Arc<dyn Transport> = Arc::new(DirectTransport(Arc::clone(&server)));
        let client = Client::connect(transport, ClientConfig::default())
            .await
            .unwrap()
            .with_input_provider(Arc::new(FixedAnswer(json!({ "branch": "main" }))));

        let result = client.call_tool("needs_input", json!({})).await.unwrap();
        assert_eq!(result.text(), "answered 2");
        assert_eq!(
            server.call_count("tools/call"),
            3,
            "two input rounds plus the final answer, all inside one call_tool"
        );
    }

    #[tokio::test]
    async fn declining_input_still_terminates() {
        // The default provider declines everything. A decline must be recorded
        // as an answer, or the server asks the same question forever.
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28).with_input_rounds(1));
        let client = connect(Arc::clone(&server)).await;

        let result = client.call_tool("needs_input", json!({})).await.unwrap();
        assert_eq!(result.text(), "answered 1");
    }

    #[tokio::test]
    async fn an_endlessly_asking_server_hits_the_round_limit() {
        let server =
            Arc::new(TestServer::new(ProtocolVersion::V2026_07_28).with_input_rounds(1_000));
        let transport: Arc<dyn Transport> = Arc::new(DirectTransport(Arc::clone(&server)));
        let config = ClientConfig { max_rounds: 4, ..Default::default() };
        let client = Client::connect(transport, config)
            .await
            .unwrap()
            .with_input_provider(Arc::new(FixedAnswer(json!({}))));

        let err = client.call_tool("needs_input", json!({})).await.unwrap_err();
        assert!(matches!(err, McpError::TooManyRounds(4)), "{err:?}");
    }

    #[tokio::test]
    async fn list_results_are_cached_when_the_server_sets_a_ttl() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28).with_list_ttl(60_000));
        let client = connect(Arc::clone(&server)).await;

        client.list_tools().await.unwrap();
        client.list_tools().await.unwrap();
        client.list_tools().await.unwrap();
        assert_eq!(server.call_count("tools/list"), 1, "the TTL authorises reuse");
    }

    #[tokio::test]
    async fn list_results_are_not_cached_without_a_ttl() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(Arc::clone(&server)).await;

        client.list_tools().await.unwrap();
        client.list_tools().await.unwrap();
        assert_eq!(
            server.call_count("tools/list"),
            2,
            "no TTL means the server did not authorise caching"
        );
    }

    #[tokio::test]
    async fn older_revisions_never_cache_even_if_a_ttl_appears() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2025_11_25).with_list_ttl(60_000));
        let client = connect(Arc::clone(&server)).await;

        client.list_tools().await.unwrap();
        client.list_tools().await.unwrap();
        assert_eq!(
            server.call_count("tools/list"),
            2,
            "cacheable lists arrived in 2026-07-28; earlier TTLs are not honoured"
        );
    }

    #[tokio::test]
    async fn invalidating_the_cache_forces_a_refetch() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28).with_list_ttl(60_000));
        let client = connect(Arc::clone(&server)).await;

        client.list_tools().await.unwrap();
        client.invalidate_caches();
        client.list_tools().await.unwrap();
        assert_eq!(server.call_count("tools/list"), 2);
    }

    #[tokio::test]
    async fn the_handle_describes_the_connection() {
        let server = Arc::new(TestServer::new(ProtocolVersion::V2026_07_28));
        let client = connect(server).await;
        let handle = client.handle();

        assert_eq!(handle.name, "test-server");
        assert_eq!(handle.version, ProtocolVersion::V2026_07_28);
        assert!(handle.capabilities.tools);
        assert!(
            handle.capabilities.has_extension("io.modelcontextprotocol/tasks"),
            "tasks moved out of experimental core into an official extension"
        );
        assert_eq!(handle.transport, "test://direct");
    }

    #[tokio::test]
    async fn capabilities_the_server_lacks_yield_empty_lists_not_errors() {
        struct NoCapabilities;

        #[async_trait]
        impl Transport for NoCapabilities {
            async fn request(
                &self,
                request: JsonRpcRequest,
                _headers: &RequestHeaders,
            ) -> Result<JsonRpcResponse> {
                let id = request.id.clone().unwrap_or(json!(0));
                Ok(JsonRpcResponse::success(
                    id,
                    json!({
                        "protocolVersions": ["2026-07-28"],
                        "serverInfo": { "name": "bare" }
                    }),
                ))
            }
            async fn notify(&self, _r: JsonRpcRequest, _h: &RequestHeaders) -> Result<()> {
                Ok(())
            }
            async fn close(&self) -> Result<()> {
                Ok(())
            }
            fn describe(&self) -> String {
                "test://bare".into()
            }
        }

        let transport: Arc<dyn Transport> = Arc::new(NoCapabilities);
        let client = Client::connect(transport, ClientConfig::default()).await.unwrap();

        assert!(client.list_tools().await.unwrap().is_empty());
        assert!(client.list_prompts().await.unwrap().is_empty());
        assert!(client.list_resources().await.unwrap().is_empty());
    }
}
