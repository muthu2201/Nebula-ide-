//! The registry's HTTP API.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::RegistryError;
use crate::store::{Registry, RegistryEntry};

/// The largest package the API will accept.
pub const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

/// A summary of one extension, as the marketplace UI shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionSummary {
    /// Reverse-DNS identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Newest published version.
    pub version: String,
    /// One-line description.
    pub description: String,
    /// Publisher name.
    pub publisher: String,
    /// Download count.
    pub downloads: u64,
    /// Capabilities the user will be asked to consent to.
    pub consent_required: Vec<String>,
}

impl From<&RegistryEntry> for ExtensionSummary {
    fn from(entry: &RegistryEntry) -> Self {
        Self {
            id: entry.manifest.id.clone(),
            name: entry.manifest.name.clone(),
            version: entry.manifest.version.to_string(),
            description: entry.manifest.description.clone(),
            publisher: entry.publisher.clone(),
            downloads: entry.downloads,
            consent_required: entry
                .manifest
                .consent_required()
                .iter()
                .map(|c| c.name().to_string())
                .collect(),
        }
    }
}

/// Query parameters for the search endpoint.
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// The search text.
    #[serde(default)]
    pub q: String,
}

/// An error, as the API returns it.
#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
    detail: String,
}

impl IntoResponse for RegistryError {
    fn into_response(self) -> Response {
        // The status codes are chosen so a client can act without parsing the
        // message: 409 means "your version number is wrong", 403 means "this is
        // not yours", 422 means "the package itself is bad".
        let status = match &self {
            RegistryError::NotFound(_) => StatusCode::NOT_FOUND,
            RegistryError::UnknownPublisher => StatusCode::UNAUTHORIZED,
            RegistryError::NamespaceNotOwned { .. } => StatusCode::FORBIDDEN,
            RegistryError::VersionExists { .. } | RegistryError::VersionNotNewer { .. } => {
                StatusCode::CONFLICT
            }
            RegistryError::Rejected(_) | RegistryError::InvalidPackage(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
        };
        let body = ApiError {
            error: match &self {
                RegistryError::NotFound(_) => "not-found",
                RegistryError::UnknownPublisher => "unknown-publisher",
                RegistryError::NamespaceNotOwned { .. } => "namespace-not-owned",
                RegistryError::VersionExists { .. } => "version-exists",
                RegistryError::VersionNotNewer { .. } => "version-not-newer",
                RegistryError::Rejected(_) => "notarisation-rejected",
                RegistryError::InvalidPackage(_) => "invalid-package",
            }
            .to_string(),
            detail: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

/// Build the API router.
pub fn router(registry: Arc<Registry>) -> Router {
    Router::new()
        .route("/v1/extensions", get(list))
        .route("/v1/extensions/search", get(search))
        .route("/v1/extensions/{id}", get(detail))
        .route("/v1/extensions/{id}/versions", get(versions))
        .route("/v1/extensions/{id}/{version}/download", get(download))
        .route("/v1/publish", post(publish))
        .route("/health", get(health))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(MAX_UPLOAD_BYTES))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(registry)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn list(State(registry): State<Arc<Registry>>) -> impl IntoResponse {
    let summaries: Vec<ExtensionSummary> =
        registry.list().iter().map(ExtensionSummary::from).collect();
    Json(summaries)
}

async fn search(
    State(registry): State<Arc<Registry>>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    let summaries: Vec<ExtensionSummary> =
        registry.search(&query.q).iter().map(ExtensionSummary::from).collect();
    Json(summaries)
}

async fn detail(
    State(registry): State<Arc<Registry>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<RegistryEntry>, RegistryError> {
    registry.latest(&id).map(Json).ok_or(RegistryError::NotFound(id))
}

async fn versions(
    State(registry): State<Arc<Registry>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<Vec<String>>, RegistryError> {
    let versions = registry.versions(&id);
    if versions.is_empty() {
        return Err(RegistryError::NotFound(id));
    }
    Ok(Json(versions.iter().map(|e| e.manifest.version.to_string()).collect()))
}

async fn download(
    State(registry): State<Arc<Registry>>,
    Path((id, version)): Path<(String, String)>,
) -> std::result::Result<Response, RegistryError> {
    let version = Version::parse(&version)
        .map_err(|e| RegistryError::InvalidPackage(format!("bad version: {e}")))?;
    let bytes = registry.download(&id, &version)?;

    Ok((
        StatusCode::OK,
        [
            ("content-type", "application/octet-stream".to_string()),
            ("content-disposition", format!("attachment; filename=\"{id}-{version}.nbx\"")),
        ],
        bytes,
    )
        .into_response())
}

async fn publish(
    State(registry): State<Arc<Registry>>,
    body: axum::body::Bytes,
) -> std::result::Result<Json<crate::store::PublishOutcome>, RegistryError> {
    registry.publish(&body).map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Publisher;
    use nebula_pkg::manifest::{Author, Manifest};
    use nebula_pkg::{KeyPair, Package, PackageContents};
    use nebula_wasm_host::Capability;

    fn component() -> Vec<u8> {
        wat::parse_str(
            r#"
            (component
              (core module $m (func (export "run") (result i32) i32.const 1))
              (core instance $i (instantiate $m))
              (func (export "run") (result s32) (canon lift (core func $i "run")))
            )
            "#,
        )
        .unwrap()
    }

    fn manifest(id: &str, version: &str, capabilities: &[Capability]) -> Manifest {
        Manifest {
            id: id.to_string(),
            name: "Test Extension".to_string(),
            version: Version::parse(version).unwrap(),
            description: "An extension used in tests".to_string(),
            author: Author { name: "Author".to_string(), email: None, url: None },
            license: "MIT".to_string(),
            world_version: Version::new(0, 1, 0),
            capabilities: capabilities.iter().copied().collect(),
            languages: Vec::new(),
            commands: Vec::new(),
            keywords: vec!["testing".to_string()],
            repository: None,
        }
    }

    fn package(keys: &KeyPair, manifest: Manifest) -> Vec<u8> {
        Package::sign(
            PackageContents { manifest, component: component(), assets: Vec::new() },
            keys,
        )
        .unwrap()
        .to_bytes()
        .unwrap()
    }

    /// Start the API on a real socket and return its address.
    async fn serve() -> (String, Arc<Registry>, KeyPair) {
        let registry = Arc::new(Registry::new());
        let keys = KeyPair::generate();
        registry.register_publisher(Publisher {
            name: "Example Ltd".to_string(),
            keys: vec![keys.public().to_base64()],
            namespaces: vec!["com.example".to_string()],
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());

        let app = router(Arc::clone(&registry));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (address, registry, keys)
    }

    #[tokio::test]
    async fn the_health_endpoint_answers() {
        let (address, _registry, _keys) = serve().await;
        let response = reqwest::get(format!("{address}/health")).await.unwrap();

        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn publishing_over_http_makes_an_extension_listable_and_downloadable() {
        let (address, _registry, keys) = serve().await;
        let bytes = package(&keys, manifest("com.example.thing", "1.0.0", &[]));

        let response = reqwest::Client::new()
            .post(format!("{address}/v1/publish"))
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());

        let listed: Vec<ExtensionSummary> =
            reqwest::get(format!("{address}/v1/extensions")).await.unwrap().json().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "com.example.thing");
        assert_eq!(listed[0].publisher, "Example Ltd");

        let downloaded =
            reqwest::get(format!("{address}/v1/extensions/com.example.thing/1.0.0/download"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
        assert_eq!(
            downloaded.as_ref(),
            bytes.as_slice(),
            "the download must be byte-identical to what was published"
        );
    }

    #[tokio::test]
    async fn an_unknown_publisher_gets_401() {
        let (address, _registry, _keys) = serve().await;
        let attacker = KeyPair::generate();
        let bytes = package(&attacker, manifest("com.example.thing", "1.0.0", &[]));

        let response = reqwest::Client::new()
            .post(format!("{address}/v1/publish"))
            .body(bytes)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 401);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"], "unknown-publisher");
    }

    #[tokio::test]
    async fn publishing_into_another_namespace_gets_403() {
        let (address, _registry, keys) = serve().await;
        let bytes = package(&keys, manifest("com.competitor.thing", "1.0.0", &[]));

        let response = reqwest::Client::new()
            .post(format!("{address}/v1/publish"))
            .body(bytes)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 403);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"], "namespace-not-owned");
    }

    #[tokio::test]
    async fn republishing_a_version_gets_409() {
        let (address, _registry, keys) = serve().await;
        let bytes = package(&keys, manifest("com.example.thing", "1.0.0", &[]));
        let client = reqwest::Client::new();

        client.post(format!("{address}/v1/publish")).body(bytes.clone()).send().await.unwrap();
        let response =
            client.post(format!("{address}/v1/publish")).body(bytes).send().await.unwrap();

        assert_eq!(response.status(), 409);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"], "version-exists");
    }

    #[tokio::test]
    async fn a_held_extension_is_not_listed_or_downloadable() {
        let (address, _registry, keys) = serve().await;
        let bytes =
            package(&keys, manifest("com.example.runner", "1.0.0", &[Capability::SpawnProcess]));

        let response = reqwest::Client::new()
            .post(format!("{address}/v1/publish"))
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());

        let outcome: serde_json::Value = response.json().await.unwrap();
        assert_eq!(outcome["outcome"], "held-for-review");

        let listed: Vec<ExtensionSummary> =
            reqwest::get(format!("{address}/v1/extensions")).await.unwrap().json().await.unwrap();
        assert!(listed.is_empty(), "a held version must not be listed");

        let download =
            reqwest::get(format!("{address}/v1/extensions/com.example.runner/1.0.0/download"))
                .await
                .unwrap();
        assert_eq!(download.status(), 404);
    }

    #[tokio::test]
    async fn search_filters_the_listing() {
        let (address, _registry, keys) = serve().await;
        let client = reqwest::Client::new();

        for (id, name) in
            [("com.example.formatter", "Prettifier"), ("com.example.linter", "Checker")]
        {
            let mut manifest = manifest(id, "1.0.0", &[]);
            manifest.name = name.to_string();
            client
                .post(format!("{address}/v1/publish"))
                .body(package(&keys, manifest))
                .send()
                .await
                .unwrap();
        }

        let found: Vec<ExtensionSummary> =
            reqwest::get(format!("{address}/v1/extensions/search?q=prettifier"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Prettifier");
    }

    #[tokio::test]
    async fn the_summary_tells_a_user_what_consent_will_be_asked_for() {
        // The listing has to show this, or the install prompt is the first the
        // user hears of it.
        let (address, registry, keys) = serve().await;
        let bytes = package(
            &keys,
            manifest("com.example.tool", "1.0.0", &[Capability::Network, Capability::ReadDocument]),
        );
        reqwest::Client::new()
            .post(format!("{address}/v1/publish"))
            .body(bytes)
            .send()
            .await
            .unwrap();
        registry.approve_pending("com.example.tool", &Version::new(1, 0, 0)).unwrap();

        let listed: Vec<ExtensionSummary> =
            reqwest::get(format!("{address}/v1/extensions")).await.unwrap().json().await.unwrap();

        assert_eq!(listed.len(), 1);
        assert!(listed[0].consent_required.contains(&"network".to_string()));
        assert!(
            !listed[0].consent_required.contains(&"read-document".to_string()),
            "reading the open document is not a consent-worthy capability"
        );
    }

    #[tokio::test]
    async fn an_unknown_extension_gets_404() {
        let (address, _registry, _keys) = serve().await;
        let response =
            reqwest::get(format!("{address}/v1/extensions/com.example.nope")).await.unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn versions_are_listed_oldest_first() {
        let (address, _registry, keys) = serve().await;
        let client = reqwest::Client::new();
        for version in ["1.0.0", "1.1.0", "2.0.0"] {
            client
                .post(format!("{address}/v1/publish"))
                .body(package(&keys, manifest("com.example.thing", version, &[])))
                .send()
                .await
                .unwrap();
        }

        let versions: Vec<String> =
            reqwest::get(format!("{address}/v1/extensions/com.example.thing/versions"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        assert_eq!(versions, vec!["1.0.0", "1.1.0", "2.0.0"]);
    }

    #[tokio::test]
    async fn a_malformed_upload_gets_422() {
        let (address, _registry, _keys) = serve().await;
        let response = reqwest::Client::new()
            .post(format!("{address}/v1/publish"))
            .body(b"not a package".to_vec())
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 422);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"], "invalid-package");
    }
}
