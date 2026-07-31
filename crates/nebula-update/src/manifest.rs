//! Release manifests and the verify-before-install flow.

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{Result, UpdateError, hash, verify_signature};

/// Which release stream a build belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Weekly builds.
    Nightly,
    /// Release candidates.
    Beta,
    /// General availability.
    Stable,
}

impl Channel {
    /// Whether a user on this channel should be offered a build from `other`.
    ///
    /// A stable user is never offered a beta build; a nightly user is offered
    /// everything, because that is what they signed up for.
    pub fn accepts(&self, other: Channel) -> bool {
        match self {
            Channel::Stable => other == Channel::Stable,
            Channel::Beta => matches!(other, Channel::Stable | Channel::Beta),
            Channel::Nightly => true,
        }
    }
}

/// A build target.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Platform {
    /// The Rust target triple.
    pub target: String,
}

impl Platform {
    /// A platform from a target triple.
    pub fn new(target: impl Into<String>) -> Self {
        Self { target: target.into() }
    }

    /// The platform this binary is running on.
    pub fn current() -> Self {
        Self::new(current_target())
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.target)
    }
}

/// The target triple of the running binary.
fn current_target() -> String {
    // Built from the compile-time cfgs, which is exact and needs no build
    // script.
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    match os {
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        other => format!("{arch}-unknown-{other}"),
    }
}

/// One published build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    /// Which platform it is for.
    pub platform: Platform,
    /// Where to download the full artefact.
    pub url: String,
    /// blake3 of the full artefact.
    pub hash: String,
    /// Size in bytes.
    pub size: u64,
    /// Delta patches from earlier versions, keyed by the version they apply to.
    #[serde(default)]
    pub deltas: Vec<DeltaLink>,
}

/// A patch from one version to this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaLink {
    /// The version this patch applies to.
    pub from_version: Version,
    /// Where to download it.
    pub url: String,
    /// blake3 of the patch file.
    pub hash: String,
    /// Size in bytes.
    pub size: u64,
}

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// The version.
    pub version: Version,
    /// Which channel it was published to.
    pub channel: Channel,
    /// When, RFC 3339.
    pub released_at: String,
    /// Release notes, as Markdown.
    #[serde(default)]
    pub notes: String,
    /// The builds.
    pub builds: Vec<Build>,
    /// Whether users must take this update.
    ///
    /// Reserved for security fixes. Overusing it trains people to distrust it.
    #[serde(default)]
    pub mandatory: bool,
}

impl Release {
    /// The build for `platform`, if there is one.
    pub fn build_for(&self, platform: &Platform) -> Option<&Build> {
        self.builds.iter().find(|build| &build.platform == platform)
    }
}

/// The signed list of available releases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// The releases, newest first.
    pub releases: Vec<Release>,
    /// When this manifest was generated, RFC 3339.
    pub generated_at: String,
    /// Ed25519 signature over the canonical form of `releases`.
    pub signature: String,
}

impl ReleaseManifest {
    /// The bytes the signature covers.
    ///
    /// Only the releases, not `generated_at` or the signature itself — so a
    /// manifest can be regenerated with a fresh timestamp without re-signing,
    /// while the content that matters stays committed to.
    pub fn signing_bytes(&self) -> Vec<u8> {
        // A canonical JSON rendering of the releases, with keys ordered by
        // serde's field order, is stable for a fixed struct definition.
        serde_json::to_vec(&self.releases).unwrap_or_default()
    }

    /// Parse and verify a manifest.
    ///
    /// Verification happens here rather than being a separate step a caller can
    /// forget, so there is no way to obtain a parsed manifest that has not been
    /// checked.
    pub fn parse_verified(json: &str, public_key: &str) -> Result<Self> {
        let manifest: ReleaseManifest =
            serde_json::from_str(json).map_err(|e| UpdateError::Manifest(e.to_string()))?;
        verify_signature(&manifest.signing_bytes(), &manifest.signature, public_key)?;
        Ok(manifest)
    }

    /// The newest release this user should be offered.
    pub fn best_update(
        &self,
        current: &Version,
        channel: Channel,
        platform: &Platform,
    ) -> Option<&Release> {
        self.releases
            .iter()
            .filter(|release| channel.accepts(release.channel))
            .filter(|release| release.version > *current)
            .filter(|release| release.build_for(platform).is_some())
            .max_by(|a, b| a.version.cmp(&b.version))
    }

    /// Whether a mandatory update is outstanding.
    pub fn has_mandatory_update(
        &self,
        current: &Version,
        channel: Channel,
        platform: &Platform,
    ) -> bool {
        self.releases.iter().any(|release| {
            release.mandatory
                && release.version > *current
                && channel.accepts(release.channel)
                && release.build_for(platform).is_some()
        })
    }
}

/// Downloaded bytes that have not yet been checked.
///
/// The only way out of this type is [`Update::verify`], which is what makes
/// "download then verify then install" the only expressible order.
#[must_use = "downloaded bytes must be verified before they are used"]
pub struct Update {
    bytes: Vec<u8>,
    expected_hash: String,
    version: Version,
}

impl Update {
    /// Wrap downloaded bytes with the hash they are expected to have.
    pub fn downloaded(bytes: Vec<u8>, expected_hash: String, version: Version) -> Self {
        Self { bytes, expected_hash, version }
    }

    /// How many bytes were downloaded.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing was downloaded.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Check the bytes against their published hash.
    pub fn verify(self) -> Result<VerifiedUpdate> {
        let actual = hash(&self.bytes);
        if actual != self.expected_hash {
            return Err(UpdateError::HashMismatch { expected: self.expected_hash, actual });
        }
        Ok(VerifiedUpdate { bytes: self.bytes, version: self.version })
    }
}

/// Bytes that have been verified against the signed manifest.
///
/// Only this type can be installed.
///
/// `Debug` prints the length rather than the bytes: a fifty-megabyte binary in
/// a log line helps nobody.
pub struct VerifiedUpdate {
    bytes: Vec<u8>,
    version: Version,
}

impl std::fmt::Debug for VerifiedUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedUpdate")
            .field("version", &self.version)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl VerifiedUpdate {
    /// The verified bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The version these bytes are.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Write the update alongside the current installation.
    ///
    /// Written next to the target and then renamed, so a crash mid-write cannot
    /// leave a truncated executable where the working one was.
    pub fn stage(&self, target: &std::path::Path) -> Result<std::path::PathBuf> {
        let staged = target.with_extension("nebula-staged");
        std::fs::write(&staged, &self.bytes)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A staged binary that is not executable would fail at the worst
            // possible moment — after the rename.
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(staged)
    }
}

/// Signs release manifests. Runs in the release pipeline, never in a client.
pub struct UpdateSigner {
    signing: SigningKey,
}

impl UpdateSigner {
    /// Generate a signing key.
    pub fn generate() -> Self {
        use rand::TryRngCore;

        let mut bytes = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .expect("the OS random source must be available to generate a signing key");
        Self { signing: SigningKey::from_bytes(&bytes) }
    }

    /// The public key clients verify against.
    pub fn public_key_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.signing.verifying_key().to_bytes())
    }

    /// Sign a manifest.
    pub fn sign(&self, releases: Vec<Release>) -> ReleaseManifest {
        let mut manifest = ReleaseManifest {
            releases,
            generated_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string()),
            signature: String::new(),
        };
        let signature = self.signing.sign(&manifest.signing_bytes());
        manifest.signature =
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(target: &str, bytes: &[u8]) -> Build {
        Build {
            platform: Platform::new(target),
            url: format!("https://downloads.example/{target}/nebula"),
            hash: hash(bytes),
            size: bytes.len() as u64,
            deltas: Vec::new(),
        }
    }

    fn release(version: &str, channel: Channel, mandatory: bool) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            channel,
            released_at: "2026-07-01T00:00:00Z".to_string(),
            notes: format!("Release {version}"),
            builds: vec![
                build("x86_64-unknown-linux-gnu", b"linux binary"),
                build("aarch64-apple-darwin", b"macos binary"),
            ],
            mandatory,
        }
    }

    fn signed(releases: Vec<Release>) -> (UpdateSigner, ReleaseManifest) {
        let signer = UpdateSigner::generate();
        let manifest = signer.sign(releases);
        (signer, manifest)
    }

    #[test]
    fn a_signed_manifest_verifies() {
        let (signer, manifest) = signed(vec![release("1.0.0", Channel::Stable, false)]);
        let json = serde_json::to_string(&manifest).unwrap();

        let parsed = ReleaseManifest::parse_verified(&json, &signer.public_key_base64()).unwrap();
        assert_eq!(parsed.releases.len(), 1);
    }

    #[test]
    fn a_manifest_signed_by_another_key_is_refused() {
        // The attack this closes: someone who controls the update server ships
        // their own binary.
        let (_signer, manifest) = signed(vec![release("1.0.0", Channel::Stable, false)]);
        let attacker = UpdateSigner::generate();
        let json = serde_json::to_string(&manifest).unwrap();

        let err =
            ReleaseManifest::parse_verified(&json, &attacker.public_key_base64()).unwrap_err();
        assert!(matches!(err, UpdateError::BadManifestSignature));
    }

    #[test]
    fn editing_a_manifest_invalidates_its_signature() {
        let (signer, mut manifest) = signed(vec![release("1.0.0", Channel::Stable, false)]);

        // Point the download at somewhere else.
        manifest.releases[0].builds[0].url = "https://evil.example/payload".to_string();
        let json = serde_json::to_string(&manifest).unwrap();

        assert!(ReleaseManifest::parse_verified(&json, &signer.public_key_base64()).is_err());
    }

    #[test]
    fn regenerating_the_timestamp_does_not_break_the_signature() {
        // A manifest can be republished without access to the signing key.
        let (signer, mut manifest) = signed(vec![release("1.0.0", Channel::Stable, false)]);
        manifest.generated_at = "2030-01-01T00:00:00Z".to_string();

        let json = serde_json::to_string(&manifest).unwrap();
        assert!(ReleaseManifest::parse_verified(&json, &signer.public_key_base64()).is_ok());
    }

    #[test]
    fn the_newest_applicable_release_is_offered() {
        let (_signer, manifest) = signed(vec![
            release("1.0.0", Channel::Stable, false),
            release("1.2.0", Channel::Stable, false),
            release("1.1.0", Channel::Stable, false),
        ]);
        let platform = Platform::new("x86_64-unknown-linux-gnu");

        let best = manifest
            .best_update(&Version::new(1, 0, 0), Channel::Stable, &platform)
            .unwrap();
        assert_eq!(best.version, Version::new(1, 2, 0));
    }

    #[test]
    fn a_user_on_the_current_version_is_offered_nothing() {
        let (_signer, manifest) = signed(vec![release("1.0.0", Channel::Stable, false)]);
        let platform = Platform::new("x86_64-unknown-linux-gnu");

        assert!(manifest.best_update(&Version::new(1, 0, 0), Channel::Stable, &platform).is_none());
        assert!(manifest.best_update(&Version::new(2, 0, 0), Channel::Stable, &platform).is_none());
    }

    #[test]
    fn a_stable_user_is_never_offered_a_beta_build() {
        let (_signer, manifest) = signed(vec![
            release("1.0.0", Channel::Stable, false),
            release("2.0.0", Channel::Beta, false),
        ]);
        let platform = Platform::new("x86_64-unknown-linux-gnu");

        assert!(
            manifest.best_update(&Version::new(1, 0, 0), Channel::Stable, &platform).is_none(),
            "a stable user must not be pushed onto a beta"
        );
        assert_eq!(
            manifest
                .best_update(&Version::new(1, 0, 0), Channel::Beta, &platform)
                .unwrap()
                .version,
            Version::new(2, 0, 0)
        );
    }

    #[test]
    fn channel_acceptance_is_ordered() {
        assert!(Channel::Stable.accepts(Channel::Stable));
        assert!(!Channel::Stable.accepts(Channel::Beta));
        assert!(!Channel::Stable.accepts(Channel::Nightly));

        assert!(Channel::Beta.accepts(Channel::Stable));
        assert!(Channel::Beta.accepts(Channel::Beta));
        assert!(!Channel::Beta.accepts(Channel::Nightly));

        assert!(Channel::Nightly.accepts(Channel::Nightly));
        assert!(Channel::Nightly.accepts(Channel::Stable));
    }

    #[test]
    fn a_release_without_a_build_for_this_platform_is_not_offered() {
        let mut only_windows = release("2.0.0", Channel::Stable, false);
        only_windows.builds = vec![build("x86_64-pc-windows-msvc", b"windows binary")];
        let (_signer, manifest) = signed(vec![only_windows]);

        assert!(
            manifest
                .best_update(
                    &Version::new(1, 0, 0),
                    Channel::Stable,
                    &Platform::new("x86_64-unknown-linux-gnu")
                )
                .is_none()
        );
    }

    #[test]
    fn a_mandatory_update_is_flagged() {
        let (_signer, manifest) = signed(vec![release("2.0.0", Channel::Stable, true)]);
        let platform = Platform::new("x86_64-unknown-linux-gnu");

        assert!(manifest.has_mandatory_update(&Version::new(1, 0, 0), Channel::Stable, &platform));
        assert!(
            !manifest.has_mandatory_update(&Version::new(2, 0, 0), Channel::Stable, &platform),
            "already up to date"
        );
    }

    #[test]
    fn downloaded_bytes_must_match_their_published_hash() {
        let bytes = b"the real release binary".to_vec();
        let update =
            Update::downloaded(bytes.clone(), hash(&bytes), Version::new(1, 0, 0));
        assert!(update.verify().is_ok());

        let swapped = Update::downloaded(
            b"a substituted payload".to_vec(),
            hash(&bytes),
            Version::new(1, 0, 0),
        );
        let err = swapped.verify().unwrap_err();
        assert!(matches!(err, UpdateError::HashMismatch { .. }));
    }

    #[test]
    fn a_verified_update_stages_next_to_the_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("nebula");
        std::fs::write(&target, b"the currently installed binary").unwrap();

        let bytes = b"the new binary".to_vec();
        let verified = Update::downloaded(bytes.clone(), hash(&bytes), Version::new(1, 1, 0))
            .verify()
            .unwrap();

        let staged = verified.stage(&target).unwrap();
        assert!(staged.exists());
        assert_eq!(std::fs::read(&staged).unwrap(), bytes);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"the currently installed binary",
            "staging must not touch the running binary"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&staged).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the staged binary must be executable");
        }
    }

    #[test]
    fn the_current_platform_is_a_plausible_triple() {
        let platform = Platform::current();
        assert!(platform.target.contains('-'), "{platform}");
        assert!(platform.target.starts_with(std::env::consts::ARCH));
    }

    #[test]
    fn a_malformed_manifest_is_reported() {
        assert!(ReleaseManifest::parse_verified("not json", "key").is_err());
        assert!(ReleaseManifest::parse_verified("{}", "key").is_err());
    }

    #[test]
    fn an_end_to_end_delta_update_verifies_against_the_release_hash() {
        // The full flow: a delta is applied to the installed binary, and the
        // result is checked against the hash the signed manifest committed to.
        use crate::delta::{apply_patch, make_patch};

        let installed = b"version one of the binary, with a lot of shared content".repeat(100);
        let published = b"version two of the binary, with a lot of shared content".repeat(100);

        let patch = make_patch(&installed, &published);
        let patched = apply_patch(&installed, &patch).unwrap();

        let update =
            Update::downloaded(patched, hash(&published), Version::new(1, 1, 0));
        let verified = update.verify().unwrap();
        assert_eq!(verified.bytes(), published.as_slice());
    }
}
