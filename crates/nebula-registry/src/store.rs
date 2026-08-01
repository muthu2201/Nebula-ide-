//! The registry's storage and publishing gate.

use std::collections::BTreeMap;

use nebula_pkg::{Manifest, NotarizationReport, NotarizationVerdict, Package, PublicKey};
use parking_lot::RwLock;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{RegistryError, Result};

/// Someone allowed to publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publisher {
    /// Display name.
    pub name: String,
    /// The base64 public keys this publisher signs with.
    ///
    /// Several, so a key can be rotated without a flag day: the new key is
    /// added, releases are signed with it, and the old one is removed later.
    pub keys: Vec<String>,
    /// Namespaces this publisher owns, as reverse-DNS prefixes.
    pub namespaces: Vec<String>,
}

impl Publisher {
    /// Whether `id` falls inside a namespace this publisher owns.
    pub fn owns(&self, id: &str) -> bool {
        self.namespaces.iter().any(|namespace| {
            // Exact match, or a proper prefix ending at a segment boundary — so
            // owning `com.example` does not confer `com.exampleother`.
            id == namespace || id.starts_with(&format!("{namespace}."))
        })
    }

    /// Whether this publisher signs with `key`.
    pub fn has_key(&self, key: &str) -> bool {
        self.keys.iter().any(|k| k == key)
    }
}

/// One published version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// The manifest as published.
    pub manifest: Manifest,
    /// Who published it.
    pub publisher: String,
    /// When, RFC 3339.
    pub published_at: String,
    /// blake3 of the package bytes.
    pub package_hash: String,
    /// Package size in bytes.
    pub package_bytes: usize,
    /// The notarisation report.
    pub notarization: NotarizationReport,
    /// How many times it has been downloaded.
    pub downloads: u64,
}

/// What happened when a package was submitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum PublishOutcome {
    /// Published and downloadable.
    Published {
        /// The extension.
        id: String,
        /// The version.
        version: String,
    },
    /// Accepted, but held until a human reviews it.
    HeldForReview {
        /// The extension.
        id: String,
        /// The version.
        version: String,
        /// Why.
        reasons: Vec<String>,
    },
}

impl PublishOutcome {
    /// Whether the version is downloadable now.
    pub fn is_live(&self) -> bool {
        matches!(self, PublishOutcome::Published { .. })
    }
}

/// The registry.
#[derive(Default)]
pub struct Registry {
    publishers: RwLock<BTreeMap<String, Publisher>>,
    /// Published versions, keyed by extension id then version.
    entries: RwLock<BTreeMap<String, BTreeMap<Version, RegistryEntry>>>,
    /// Package bytes, keyed by `id@version`.
    blobs: RwLock<BTreeMap<String, Vec<u8>>>,
    /// Versions awaiting human review.
    pending: RwLock<Vec<(RegistryEntry, Vec<u8>)>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a publisher.
    pub fn register_publisher(&self, publisher: Publisher) {
        self.publishers.write().insert(publisher.name.clone(), publisher);
    }

    /// The registered publishers.
    pub fn publishers(&self) -> Vec<Publisher> {
        self.publishers.read().values().cloned().collect()
    }

    /// Submit a package.
    ///
    /// The gate, in order. Each step is a hard stop, and the order matters:
    /// nothing is notarised before it is known who signed it, and nothing is
    /// stored before it is notarised.
    pub fn publish(&self, package_bytes: &[u8]) -> Result<PublishOutcome> {
        let package = Package::from_bytes(package_bytes)
            .map_err(|e| RegistryError::InvalidPackage(e.to_string()))?;

        // 1. Who signed this? A signature that verifies against its own embedded
        // key proves nothing, so the key must be one a publisher registered.
        let signing_key = package.signature.public_key.clone();
        let publisher = {
            let publishers = self.publishers.read();
            publishers
                .values()
                .find(|publisher| publisher.has_key(&signing_key))
                .cloned()
                .ok_or(RegistryError::UnknownPublisher)?
        };

        // The signature itself must verify against that key.
        let trusted = PublicKey::from_base64(&signing_key)
            .map_err(|e| RegistryError::InvalidPackage(e.to_string()))?;
        package
            .verify_against_trusted(&[trusted])
            .map_err(|e| RegistryError::InvalidPackage(e.to_string()))?;

        let manifest = package.contents.manifest.clone();

        // 2. Does this publisher own the namespace? Otherwise anyone with an
        // account could ship an update to someone else's extension.
        if !publisher.owns(&manifest.id) {
            return Err(RegistryError::NamespaceNotOwned {
                publisher: publisher.name,
                namespace: manifest.id,
            });
        }

        // 3. Versions are immutable and must move forwards.
        {
            let entries = self.entries.read();
            if let Some(versions) = entries.get(&manifest.id) {
                if versions.contains_key(&manifest.version) {
                    return Err(RegistryError::VersionExists {
                        id: manifest.id.clone(),
                        version: manifest.version.to_string(),
                    });
                }
                if let Some(latest) = versions.keys().max()
                    && manifest.version < *latest
                {
                    return Err(RegistryError::VersionNotNewer {
                        id: manifest.id.clone(),
                        version: manifest.version.to_string(),
                        latest: latest.to_string(),
                    });
                }
            }
        }

        // 4. Notarisation, which compares declared capabilities against the ones
        // the binary actually imports.
        let notarization = nebula_pkg::notarize(&manifest, &package.contents.component);
        if let NotarizationVerdict::Rejected { reasons } = &notarization.verdict {
            return Err(RegistryError::Rejected(reasons.join("; ")));
        }

        let entry = RegistryEntry {
            manifest: manifest.clone(),
            publisher: publisher.name.clone(),
            published_at: now_rfc3339(),
            package_hash: blake3::hash(package_bytes).to_hex().to_string(),
            package_bytes: package_bytes.len(),
            notarization: notarization.clone(),
            downloads: 0,
        };

        // 5. Anything reaching outside the editor waits for a person.
        if let NotarizationVerdict::NeedsReview { reasons } = notarization.verdict {
            self.pending.write().push((entry, package_bytes.to_vec()));
            return Ok(PublishOutcome::HeldForReview {
                id: manifest.id,
                version: manifest.version.to_string(),
                reasons,
            });
        }

        self.store(entry, package_bytes.to_vec());
        Ok(PublishOutcome::Published { id: manifest.id, version: manifest.version.to_string() })
    }

    fn store(&self, entry: RegistryEntry, bytes: Vec<u8>) {
        let key = format!("{}@{}", entry.manifest.id, entry.manifest.version);
        self.blobs.write().insert(key, bytes);
        self.entries
            .write()
            .entry(entry.manifest.id.clone())
            .or_default()
            .insert(entry.manifest.version.clone(), entry);
    }

    /// Approve a version that was held for review.
    pub fn approve_pending(&self, id: &str, version: &Version) -> Result<()> {
        let position = {
            let pending = self.pending.read();
            pending
                .iter()
                .position(|(entry, _)| {
                    entry.manifest.id == id && entry.manifest.version == *version
                })
                .ok_or_else(|| RegistryError::NotFound(format!("{id}@{version}")))?
        };
        let (entry, bytes) = self.pending.write().remove(position);
        self.store(entry, bytes);
        Ok(())
    }

    /// Versions awaiting review.
    pub fn pending_review(&self) -> Vec<RegistryEntry> {
        self.pending.read().iter().map(|(entry, _)| entry.clone()).collect()
    }

    /// Every published extension's newest version.
    pub fn list(&self) -> Vec<RegistryEntry> {
        self.entries
            .read()
            .values()
            .filter_map(|versions| versions.values().next_back().cloned())
            .collect()
    }

    /// Search published extensions by name, description or keyword.
    pub fn search(&self, query: &str) -> Vec<RegistryEntry> {
        let query = query.to_lowercase();
        if query.is_empty() {
            return self.list();
        }
        self.list()
            .into_iter()
            .filter(|entry| {
                let manifest = &entry.manifest;
                manifest.id.to_lowercase().contains(&query)
                    || manifest.name.to_lowercase().contains(&query)
                    || manifest.description.to_lowercase().contains(&query)
                    || manifest.keywords.iter().any(|k| k.to_lowercase().contains(&query))
            })
            .collect()
    }

    /// The newest published version of an extension.
    pub fn latest(&self, id: &str) -> Option<RegistryEntry> {
        self.entries.read().get(id)?.values().next_back().cloned()
    }

    /// Every published version of an extension, oldest first.
    pub fn versions(&self, id: &str) -> Vec<RegistryEntry> {
        self.entries.read().get(id).map(|v| v.values().cloned().collect()).unwrap_or_default()
    }

    /// Fetch a package's bytes, counting the download.
    pub fn download(&self, id: &str, version: &Version) -> Result<Vec<u8>> {
        let key = format!("{id}@{version}");
        let bytes = self
            .blobs
            .read()
            .get(&key)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(key.clone()))?;

        if let Some(versions) = self.entries.write().get_mut(id)
            && let Some(entry) = versions.get_mut(version)
        {
            entry.downloads += 1;
        }
        Ok(bytes)
    }

    /// How many extensions are published.
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Whether nothing is published.
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_pkg::manifest::Author;
    use nebula_pkg::{KeyPair, PackageContents};
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

    fn registry_with_publisher() -> (Registry, KeyPair) {
        let registry = Registry::new();
        let keys = KeyPair::generate();
        registry.register_publisher(Publisher {
            name: "Example Ltd".to_string(),
            keys: vec![keys.public().to_base64()],
            namespaces: vec!["com.example".to_string()],
        });
        (registry, keys)
    }

    #[test]
    fn a_well_formed_package_publishes() {
        let (registry, keys) = registry_with_publisher();
        let bytes = package(&keys, manifest("com.example.thing", "1.0.0", &[]));

        let outcome = registry.publish(&bytes).unwrap();
        assert!(outcome.is_live(), "{outcome:?}");
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.latest("com.example.thing").unwrap().manifest.version,
            Version::new(1, 0, 0)
        );
    }

    #[test]
    fn a_package_from_an_unregistered_key_is_refused() {
        // The attack this closes: anyone can generate a key and sign a package
        // that verifies against itself.
        let (registry, _keys) = registry_with_publisher();
        let attacker = KeyPair::generate();
        let bytes = package(&attacker, manifest("com.example.thing", "1.0.0", &[]));

        assert!(matches!(registry.publish(&bytes), Err(RegistryError::UnknownPublisher)));
        assert!(registry.is_empty());
    }

    #[test]
    fn a_publisher_cannot_publish_into_someone_elses_namespace() {
        let (registry, keys) = registry_with_publisher();
        let bytes = package(&keys, manifest("com.competitor.thing", "1.0.0", &[]));

        let err = registry.publish(&bytes).unwrap_err();
        assert!(matches!(err, RegistryError::NamespaceNotOwned { .. }), "{err:?}");
    }

    #[test]
    fn namespace_ownership_stops_at_a_segment_boundary() {
        // Owning `com.example` must not confer `com.exampleother`.
        let publisher = Publisher {
            name: "Example".to_string(),
            keys: Vec::new(),
            namespaces: vec!["com.example".to_string()],
        };

        assert!(publisher.owns("com.example"));
        assert!(publisher.owns("com.example.thing"));
        assert!(publisher.owns("com.example.deeply.nested"));
        assert!(!publisher.owns("com.exampleother.thing"));
        assert!(!publisher.owns("com.other.thing"));
    }

    #[test]
    fn a_published_version_cannot_be_replaced() {
        // An installed extension must not change under a user who pinned it.
        let (registry, keys) = registry_with_publisher();
        let first = package(&keys, manifest("com.example.thing", "1.0.0", &[]));
        registry.publish(&first).unwrap();

        let mut altered = manifest("com.example.thing", "1.0.0", &[]);
        altered.description = "something else entirely".to_string();
        let second = package(&keys, altered);

        let err = registry.publish(&second).unwrap_err();
        assert!(matches!(err, RegistryError::VersionExists { .. }), "{err:?}");
        assert_eq!(
            registry.latest("com.example.thing").unwrap().manifest.description,
            "An extension used in tests"
        );
    }

    #[test]
    fn versions_must_move_forwards() {
        let (registry, keys) = registry_with_publisher();
        registry.publish(&package(&keys, manifest("com.example.thing", "2.0.0", &[]))).unwrap();

        let err = registry
            .publish(&package(&keys, manifest("com.example.thing", "1.0.0", &[])))
            .unwrap_err();
        assert!(matches!(err, RegistryError::VersionNotNewer { .. }), "{err:?}");
    }

    #[test]
    fn a_capability_requesting_extension_is_held_for_review() {
        let (registry, keys) = registry_with_publisher();
        let bytes =
            package(&keys, manifest("com.example.runner", "1.0.0", &[Capability::SpawnProcess]));

        let outcome = registry.publish(&bytes).unwrap();
        match &outcome {
            PublishOutcome::HeldForReview { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("spawn-process")), "{reasons:?}");
            }
            other => panic!("expected a review hold, got {other:?}"),
        }

        assert!(!outcome.is_live());
        assert!(
            registry.latest("com.example.runner").is_none(),
            "a held version must not be downloadable"
        );
        assert_eq!(registry.pending_review().len(), 1);
    }

    #[test]
    fn approving_a_held_version_publishes_it() {
        let (registry, keys) = registry_with_publisher();
        let bytes = package(&keys, manifest("com.example.runner", "1.0.0", &[Capability::Network]));
        registry.publish(&bytes).unwrap();

        registry.approve_pending("com.example.runner", &Version::new(1, 0, 0)).unwrap();
        assert!(registry.latest("com.example.runner").is_some());
        assert!(registry.pending_review().is_empty());
    }

    #[test]
    fn approving_something_that_is_not_pending_is_an_error() {
        let (registry, _keys) = registry_with_publisher();
        assert!(registry.approve_pending("com.example.nope", &Version::new(1, 0, 0)).is_err());
    }

    #[test]
    fn a_tampered_package_is_refused() {
        let (registry, keys) = registry_with_publisher();
        let mut bytes = package(&keys, manifest("com.example.thing", "1.0.0", &[]));

        // Flip a byte in the middle of the archive.
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xFF;

        assert!(registry.publish(&bytes).is_err());
        assert!(registry.is_empty());
    }

    #[test]
    fn a_key_can_be_rotated_without_a_flag_day() {
        let registry = Registry::new();
        let old = KeyPair::generate();
        let new = KeyPair::generate();

        registry.register_publisher(Publisher {
            name: "Example Ltd".to_string(),
            keys: vec![old.public().to_base64(), new.public().to_base64()],
            namespaces: vec!["com.example".to_string()],
        });

        registry.publish(&package(&old, manifest("com.example.a", "1.0.0", &[]))).unwrap();
        registry.publish(&package(&new, manifest("com.example.b", "1.0.0", &[]))).unwrap();
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn several_versions_are_kept_and_the_newest_is_returned() {
        let (registry, keys) = registry_with_publisher();
        for version in ["1.0.0", "1.1.0", "2.0.0"] {
            registry.publish(&package(&keys, manifest("com.example.thing", version, &[]))).unwrap();
        }

        assert_eq!(registry.versions("com.example.thing").len(), 3);
        assert_eq!(
            registry.latest("com.example.thing").unwrap().manifest.version,
            Version::new(2, 0, 0)
        );
        assert_eq!(registry.len(), 1, "three versions of one extension is one extension");
    }

    #[test]
    fn downloading_returns_the_exact_published_bytes() {
        let (registry, keys) = registry_with_publisher();
        let bytes = package(&keys, manifest("com.example.thing", "1.0.0", &[]));
        registry.publish(&bytes).unwrap();

        let downloaded = registry.download("com.example.thing", &Version::new(1, 0, 0)).unwrap();
        assert_eq!(downloaded, bytes);

        // And the round trip still verifies for the installer.
        let package = Package::from_bytes(&downloaded).unwrap();
        assert!(package.verify_against_trusted(&[keys.public()]).is_ok());
    }

    #[test]
    fn downloads_are_counted() {
        let (registry, keys) = registry_with_publisher();
        registry.publish(&package(&keys, manifest("com.example.thing", "1.0.0", &[]))).unwrap();

        for _ in 0..3 {
            registry.download("com.example.thing", &Version::new(1, 0, 0)).unwrap();
        }
        assert_eq!(registry.latest("com.example.thing").unwrap().downloads, 3);
    }

    #[test]
    fn downloading_something_unpublished_is_an_error() {
        let (registry, _keys) = registry_with_publisher();
        assert!(registry.download("com.example.nope", &Version::new(1, 0, 0)).is_err());
    }

    #[test]
    fn search_matches_id_name_description_and_keywords() {
        let (registry, keys) = registry_with_publisher();
        let mut manifest = manifest("com.example.formatter", "1.0.0", &[]);
        manifest.name = "Prettifier".to_string();
        manifest.description = "Reformats source files".to_string();
        manifest.keywords = vec!["style".to_string()];
        registry.publish(&package(&keys, manifest)).unwrap();

        for query in ["formatter", "PRETTIFIER", "reformats", "style"] {
            assert_eq!(registry.search(query).len(), 1, "query `{query}` found nothing");
        }
        assert!(registry.search("unrelated").is_empty());
        assert_eq!(registry.search("").len(), 1, "an empty query lists everything");
    }

    #[test]
    fn the_notarisation_report_is_kept_with_the_entry() {
        // So a reviewer, or a user asking why an extension wants what it wants,
        // can see what the gate actually found.
        let (registry, keys) = registry_with_publisher();
        registry.publish(&package(&keys, manifest("com.example.thing", "1.0.0", &[]))).unwrap();

        let entry = registry.latest("com.example.thing").unwrap();
        assert_eq!(entry.notarization.extension_id, "com.example.thing");
        assert!(entry.notarization.undeclared.is_empty());
        assert_eq!(entry.package_hash.len(), 64);
        assert!(entry.package_bytes > 0);
    }

    #[test]
    fn corrupt_bytes_are_refused_without_panicking() {
        let (registry, _keys) = registry_with_publisher();
        assert!(registry.publish(b"not a package").is_err());
        assert!(registry.publish(&[]).is_err());
    }
}
