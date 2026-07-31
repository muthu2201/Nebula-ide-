//! Protocol revisions and capability gating.

use serde::{Deserialize, Serialize};

/// An MCP protocol revision.
///
/// Revisions are date strings, not semver, so they are modelled as a closed
/// enum: the set of revisions a build understands is fixed at compile time and
/// an unknown one from a server is an explicit variant rather than a silent
/// downgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProtocolVersion {
    /// The 2025-06-18 revision: stateful, `initialize` handshake, HTTP+SSE.
    V2025_06_18,
    /// The 2025-11-25 revision: stateful, adds tasks as an experimental core
    /// feature.
    V2025_11_25,
    /// The 2026-07-28 revision: stateless core, MRTR, header-based routing,
    /// cacheable list results, CIMD authorization.
    V2026_07_28,
}

impl ProtocolVersion {
    /// The revision Nebula prefers and advertises first.
    pub const PREFERRED: ProtocolVersion = ProtocolVersion::V2026_07_28;

    /// The oldest revision still supported, in ascending order.
    pub const SUPPORTED: &'static [ProtocolVersion] = &[
        ProtocolVersion::V2025_06_18,
        ProtocolVersion::V2025_11_25,
        ProtocolVersion::V2026_07_28,
    ];

    /// The wire representation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            ProtocolVersion::V2025_06_18 => "2025-06-18",
            ProtocolVersion::V2025_11_25 => "2025-11-25",
            ProtocolVersion::V2026_07_28 => "2026-07-28",
        }
    }

    /// Parse a revision from the wire.
    pub fn parse(value: &str) -> Option<ProtocolVersion> {
        Self::SUPPORTED.iter().copied().find(|v| v.as_str() == value)
    }

    /// Whether this revision uses the stateless core.
    ///
    /// Stateless revisions carry identity and capabilities in every request's
    /// `_meta` and have no `initialize` handshake and no session header.
    pub const fn is_stateless(&self) -> bool {
        matches!(self, ProtocolVersion::V2026_07_28)
    }

    /// Whether this revision requires the `initialize`/`initialized` handshake.
    pub const fn requires_handshake(&self) -> bool {
        !self.is_stateless()
    }

    /// Whether this revision uses Multi Round-Trip Requests for elicitation and
    /// sampling, rather than a held-open server→client stream.
    pub const fn uses_mrtr(&self) -> bool {
        self.is_stateless()
    }

    /// Whether `Mcp-Method` and `Mcp-Name` routing headers are mandatory.
    ///
    /// They let a gateway route and meter a request without parsing its body.
    pub const fn requires_routing_headers(&self) -> bool {
        matches!(self, ProtocolVersion::V2026_07_28)
    }

    /// Whether list results may carry `ttlMs`/`cacheScope` and be cached.
    pub const fn supports_cacheable_lists(&self) -> bool {
        matches!(self, ProtocolVersion::V2026_07_28)
    }

    /// Whether `server/discover` is available for up-front capability discovery.
    pub const fn supports_discover(&self) -> bool {
        matches!(self, ProtocolVersion::V2026_07_28)
    }

    /// Whether the legacy HTTP+SSE transport is still permitted.
    ///
    /// Deprecated in 2026-07-28; the transports are stdio and Streamable HTTP.
    pub const fn allows_http_sse(&self) -> bool {
        !matches!(self, ProtocolVersion::V2026_07_28)
    }

    /// Whether a feature is in the Active phase for this revision.
    ///
    /// Sampling, Roots and Logging are Deprecated in 2026-07-28 with a
    /// ≥12-month removal window. Nebula still speaks them to servers on older
    /// revisions, and refuses to newly depend on them on 2026-07-28.
    pub fn feature_status(&self, feature: &str) -> FeatureStatus {
        match feature {
            "sampling" | "roots" | "logging" => {
                if matches!(self, ProtocolVersion::V2026_07_28) {
                    FeatureStatus::Deprecated
                } else {
                    FeatureStatus::Active
                }
            }
            "tasks" => match self {
                ProtocolVersion::V2025_06_18 => FeatureStatus::Unavailable,
                // Experimental in 2025-11-25, promoted to an official extension
                // (`io.modelcontextprotocol/tasks`) in 2026-07-28.
                _ => FeatureStatus::Active,
            },
            "elicitation" => FeatureStatus::Active,
            "tools" | "prompts" | "resources" => FeatureStatus::Active,
            _ => FeatureStatus::Unavailable,
        }
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::PREFERRED
    }
}

/// Where a feature sits in its lifecycle for a given revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureStatus {
    /// Supported and safe to depend on.
    Active,
    /// Still functional but scheduled for removal; usable, not to be newly
    /// depended on.
    Deprecated,
    /// Not present in this revision.
    Unavailable,
}

impl FeatureStatus {
    /// Whether the feature can be used at all.
    pub fn is_usable(&self) -> bool {
        matches!(self, FeatureStatus::Active | FeatureStatus::Deprecated)
    }
}

/// What a server says it can do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Server exposes tools.
    #[serde(default)]
    pub tools: bool,
    /// Server exposes prompts.
    #[serde(default)]
    pub prompts: bool,
    /// Server exposes resources.
    #[serde(default)]
    pub resources: bool,
    /// Server can ask the client for input mid-request (elicitation).
    #[serde(default)]
    pub elicitation: bool,
    /// Server supports the tasks extension.
    #[serde(default)]
    pub tasks: bool,
    /// Extension identifiers the server declares, in reverse-DNS form.
    #[serde(default)]
    pub extensions: Vec<String>,
}

impl Capabilities {
    /// Everything a general-purpose client advertises.
    pub fn client_default() -> Capabilities {
        Capabilities {
            tools: true,
            prompts: true,
            resources: true,
            elicitation: true,
            tasks: true,
            extensions: Vec::new(),
        }
    }

    /// Parse the `capabilities` object as sent by any supported revision.
    ///
    /// The shape differs between revisions — 2025-06-18 nests objects under
    /// `capabilities`, 2026-07-28 returns a flatter form from `server/discover`
    /// — so presence rather than shape is what is tested here.
    pub fn from_json(value: &serde_json::Value) -> Capabilities {
        let capabilities = value.get("capabilities").unwrap_or(value);
        let has = |key: &str| capabilities.get(key).is_some_and(|v| !v.is_null());

        let extensions: Vec<String> = capabilities
            .get("extensions")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.as_str()
                            .map(str::to_string)
                            .or_else(|| item.get("id").and_then(|i| i.as_str()).map(str::to_string))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Capabilities {
            tools: has("tools"),
            prompts: has("prompts"),
            resources: has("resources"),
            elicitation: has("elicitation"),
            tasks: has("tasks")
                || extensions.iter().any(|e| e == "io.modelcontextprotocol/tasks"),
            extensions,
        }
    }

    /// Whether the server declares a named extension.
    pub fn has_extension(&self, id: &str) -> bool {
        self.extensions.iter().any(|e| e == id)
    }
}

/// Pick the best revision both sides support.
///
/// Returns `None` when there is no overlap, which is a hard failure: guessing
/// and hoping the wire format is close enough is how a client corrupts a
/// server's state.
pub fn negotiate(server_versions: &[&str]) -> Option<ProtocolVersion> {
    let mut supported: Vec<ProtocolVersion> =
        server_versions.iter().filter_map(|v| ProtocolVersion::parse(v)).collect();
    supported.sort();
    supported.last().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_round_trip_through_their_wire_form() {
        for version in ProtocolVersion::SUPPORTED {
            assert_eq!(ProtocolVersion::parse(version.as_str()), Some(*version));
        }
        assert_eq!(ProtocolVersion::parse("1999-01-01"), None);
    }

    #[test]
    fn the_preferred_revision_is_the_newest_supported() {
        assert_eq!(ProtocolVersion::PREFERRED, ProtocolVersion::V2026_07_28);
        assert_eq!(ProtocolVersion::SUPPORTED.last(), Some(&ProtocolVersion::PREFERRED));
    }

    #[test]
    fn only_the_newest_revision_is_stateless() {
        assert!(ProtocolVersion::V2026_07_28.is_stateless());
        assert!(!ProtocolVersion::V2026_07_28.requires_handshake());

        assert!(!ProtocolVersion::V2025_06_18.is_stateless());
        assert!(ProtocolVersion::V2025_06_18.requires_handshake());
        assert!(ProtocolVersion::V2025_11_25.requires_handshake());
    }

    #[test]
    fn the_stateless_revision_gates_its_new_features() {
        let new = ProtocolVersion::V2026_07_28;
        assert!(new.uses_mrtr());
        assert!(new.requires_routing_headers());
        assert!(new.supports_cacheable_lists());
        assert!(new.supports_discover());

        let old = ProtocolVersion::V2025_06_18;
        assert!(!old.uses_mrtr());
        assert!(!old.requires_routing_headers());
        assert!(!old.supports_cacheable_lists());
        assert!(!old.supports_discover());
    }

    #[test]
    fn http_sse_is_only_allowed_on_older_revisions() {
        assert!(ProtocolVersion::V2025_06_18.allows_http_sse());
        assert!(ProtocolVersion::V2025_11_25.allows_http_sse());
        assert!(
            !ProtocolVersion::V2026_07_28.allows_http_sse(),
            "HTTP+SSE was deprecated in 2026-07-28"
        );
    }

    #[test]
    fn deprecated_features_stay_usable_during_the_offramp() {
        for feature in ["sampling", "roots", "logging"] {
            let status = ProtocolVersion::V2026_07_28.feature_status(feature);
            assert_eq!(status, FeatureStatus::Deprecated, "{feature}");
            assert!(
                status.is_usable(),
                "{feature} must keep working through its 12-month window"
            );
            assert_eq!(
                ProtocolVersion::V2025_06_18.feature_status(feature),
                FeatureStatus::Active
            );
        }
    }

    #[test]
    fn tasks_became_available_after_the_oldest_revision() {
        assert_eq!(
            ProtocolVersion::V2025_06_18.feature_status("tasks"),
            FeatureStatus::Unavailable
        );
        assert_eq!(ProtocolVersion::V2025_11_25.feature_status("tasks"), FeatureStatus::Active);
        assert_eq!(ProtocolVersion::V2026_07_28.feature_status("tasks"), FeatureStatus::Active);
    }

    #[test]
    fn an_unknown_feature_is_unavailable_not_assumed_present() {
        assert_eq!(
            ProtocolVersion::V2026_07_28.feature_status("telepathy"),
            FeatureStatus::Unavailable
        );
    }

    #[test]
    fn negotiation_picks_the_newest_shared_revision() {
        assert_eq!(
            negotiate(&["2025-06-18", "2026-07-28", "2025-11-25"]),
            Some(ProtocolVersion::V2026_07_28)
        );
        assert_eq!(negotiate(&["2025-06-18"]), Some(ProtocolVersion::V2025_06_18));
    }

    #[test]
    fn negotiation_ignores_revisions_we_do_not_know() {
        assert_eq!(
            negotiate(&["2099-01-01", "2025-11-25"]),
            Some(ProtocolVersion::V2025_11_25),
            "an unknown future revision must not be guessed at"
        );
    }

    #[test]
    fn no_shared_revision_is_a_hard_failure() {
        assert_eq!(negotiate(&["1999-01-01"]), None);
        assert_eq!(negotiate(&[]), None);
    }

    #[test]
    fn capabilities_parse_from_the_nested_2025_shape() {
        let value = serde_json::json!({
            "capabilities": {
                "tools": { "listChanged": true },
                "resources": {},
                "logging": {}
            }
        });
        let capabilities = Capabilities::from_json(&value);
        assert!(capabilities.tools);
        assert!(capabilities.resources);
        assert!(!capabilities.prompts);
        assert!(!capabilities.elicitation);
    }

    #[test]
    fn capabilities_parse_from_the_flat_discover_shape() {
        let value = serde_json::json!({
            "tools": { "count": 4 },
            "prompts": {},
            "extensions": ["io.modelcontextprotocol/tasks", "com.example/thing"]
        });
        let capabilities = Capabilities::from_json(&value);
        assert!(capabilities.tools);
        assert!(capabilities.prompts);
        assert!(capabilities.tasks, "the tasks extension implies the tasks capability");
        assert!(capabilities.has_extension("com.example/thing"));
        assert!(!capabilities.has_extension("com.example/absent"));
    }

    #[test]
    fn a_null_capability_does_not_count_as_present() {
        let value = serde_json::json!({ "capabilities": { "tools": null } });
        assert!(!Capabilities::from_json(&value).tools);
    }

    #[test]
    fn extensions_parse_from_both_string_and_object_forms() {
        let value = serde_json::json!({
            "extensions": ["com.example/a", { "id": "com.example/b", "version": "1" }]
        });
        let capabilities = Capabilities::from_json(&value);
        assert!(capabilities.has_extension("com.example/a"));
        assert!(capabilities.has_extension("com.example/b"));
    }
}
