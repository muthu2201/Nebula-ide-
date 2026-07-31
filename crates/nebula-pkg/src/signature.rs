//! Ed25519 signing.
//!
//! Self-managed Ed25519 keys, the simpler of the two options the blueprint
//! identifies. Keyless signing through Sigstore (Fulcio certificates and a Rekor
//! transparency log) gives public, auditable provenance and is the better answer
//! for a public registry — but it requires an OIDC identity and network access at
//! signing time, which a developer signing a private extension on a laptop does
//! not have. The formats do not conflict: a package can carry both, and
//! [`Signature`] records which scheme produced it so a future Sigstore signature
//! is distinguishable rather than ambiguous.

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Errors from signing and verification.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    /// The key material is not a valid Ed25519 key.
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// The signature is malformed.
    #[error("malformed signature: {0}")]
    MalformedSignature(String),

    /// The signature does not match the content.
    #[error("signature does not match the content it claims to sign")]
    Mismatch,

    /// The signature was made by a key that is not trusted here.
    #[error("signature is from an untrusted key: {0}")]
    UntrustedKey(String),

    /// Encoding or decoding failed.
    #[error("encoding error: {0}")]
    Encoding(String),
}

/// A signing scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scheme {
    /// A self-managed Ed25519 key.
    Ed25519,
    /// Sigstore keyless, with the certificate and transparency-log entry
    /// carried alongside.
    SigstoreKeyless,
}

/// A public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    key: VerifyingKey,
}

impl PublicKey {
    /// Parse a base64-encoded key.
    pub fn from_base64(encoded: &str) -> Result<Self, SigningError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| SigningError::InvalidKey(format!("not valid base64: {e}")))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| SigningError::InvalidKey("an Ed25519 key is 32 bytes".to_string()))?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|e| SigningError::InvalidKey(e.to_string()))?;
        Ok(Self { key })
    }

    /// The key, base64-encoded.
    pub fn to_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.key.to_bytes())
    }

    /// A short fingerprint, for display and for pinning.
    pub fn fingerprint(&self) -> String {
        let hash = blake3::hash(&self.key.to_bytes());
        hash.to_hex()[..16].to_string()
    }

    /// Verify `signature` over `content`.
    pub fn verify(&self, content: &[u8], signature: &Signature) -> Result<(), SigningError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&signature.value)
            .map_err(|e| SigningError::MalformedSignature(e.to_string()))?;
        let bytes: [u8; 64] = bytes
            .try_into()
            .map_err(|_| SigningError::MalformedSignature("expected 64 bytes".to_string()))?;
        let signature = ed25519_dalek::Signature::from_bytes(&bytes);

        self.key.verify(content, &signature).map_err(|_| SigningError::Mismatch)
    }
}

/// A signing key pair.
///
/// The private half is never serialised by this type. Exporting it is a
/// deliberate act through [`KeyPair::to_private_base64`], so a key cannot end up
/// in a log or a manifest by accident.
pub struct KeyPair {
    signing: SigningKey,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Printing the public fingerprint is useful; printing the private key
        // would be a leak.
        f.debug_struct("KeyPair").field("fingerprint", &self.public().fingerprint()).finish()
    }
}

impl KeyPair {
    /// Generate a new key pair from the operating system's random source.
    ///
    /// # Panics
    ///
    /// If the OS random source fails. That is not a recoverable condition —
    /// generating a signing key from a degraded entropy source would be far
    /// worse than stopping.
    pub fn generate() -> Self {
        use rand::TryRngCore;

        let mut bytes = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .expect("the operating system random source must be available to generate a key");
        Self { signing: SigningKey::from_bytes(&bytes) }
    }

    /// Reconstruct a key pair from a base64-encoded private key.
    pub fn from_private_base64(encoded: &str) -> Result<Self, SigningError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| SigningError::InvalidKey(format!("not valid base64: {e}")))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| SigningError::InvalidKey("an Ed25519 key is 32 bytes".to_string()))?;
        Ok(Self { signing: SigningKey::from_bytes(&bytes) })
    }

    /// Export the private key. Handle the result carefully.
    pub fn to_private_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.signing.to_bytes())
    }

    /// The public half.
    pub fn public(&self) -> PublicKey {
        PublicKey { key: self.signing.verifying_key() }
    }

    /// Sign `content`.
    pub fn sign(&self, content: &[u8]) -> Signature {
        let signature = self.signing.sign(content);
        Signature {
            scheme: Scheme::Ed25519,
            value: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            public_key: self.public().to_base64(),
            signed_at: now_rfc3339(),
            certificate: None,
            transparency_log_entry: None,
        }
    }
}

/// A detached signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Which scheme produced it.
    pub scheme: Scheme,
    /// The signature, base64-encoded.
    pub value: String,
    /// The public key, base64-encoded.
    pub public_key: String,
    /// RFC 3339 timestamp.
    pub signed_at: String,
    /// The Fulcio certificate, for a Sigstore signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    /// The Rekor transparency-log entry, for a Sigstore signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transparency_log_entry: Option<String>,
}

impl Signature {
    /// The public key that made this signature.
    pub fn public_key(&self) -> Result<PublicKey, SigningError> {
        PublicKey::from_base64(&self.public_key)
    }

    /// Verify against the key embedded in the signature.
    ///
    /// This proves only integrity — that the content has not changed since it
    /// was signed. It does **not** prove the signer is anyone in particular,
    /// because the signature carries its own key. Use
    /// [`Signature::verify_against_trusted`] wherever identity matters.
    pub fn verify_integrity(&self, content: &[u8]) -> Result<(), SigningError> {
        self.public_key()?.verify(content, self)
    }

    /// Verify against a set of keys the caller trusts.
    ///
    /// This is what an installer must use: a signature that carries its own key
    /// is trivially forged by anyone with a keyboard.
    pub fn verify_against_trusted(
        &self,
        content: &[u8],
        trusted: &[PublicKey],
    ) -> Result<(), SigningError> {
        let key = self.public_key()?;
        if !trusted.contains(&key) {
            return Err(SigningError::UntrustedKey(key.fingerprint()));
        }
        key.verify(content, self)
    }
}

/// The digest a signature is made over.
///
/// Signing a digest rather than the archive means the signature does not depend
/// on tar metadata — timestamps, ordering, permissions — which differ between
/// machines and would otherwise make a rebuild fail to verify.
pub fn content_digest(manifest: &[u8], component: &[u8], assets: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"nebula-package-v1");

    // Length-prefixed so that concatenating two fields differently cannot
    // produce the same digest.
    let mut field = |label: &[u8], bytes: &[u8]| {
        hasher.update(&(label.len() as u64).to_le_bytes());
        hasher.update(label);
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };

    field(b"manifest", manifest);
    field(b"component", component);

    // Assets are sorted so the digest does not depend on directory iteration
    // order.
    let mut sorted: Vec<&(String, Vec<u8>)> = assets.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, bytes) in sorted {
        field(name.as_bytes(), bytes);
    }

    hasher.finalize().as_bytes().to_vec()
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_verifies_against_the_content_it_signed() {
        let keys = KeyPair::generate();
        let content = b"the package digest";

        let signature = keys.sign(content);
        assert!(signature.verify_integrity(content).is_ok());
    }

    #[test]
    fn a_signature_fails_against_altered_content() {
        let keys = KeyPair::generate();
        let signature = keys.sign(b"original content");

        let err = signature.verify_integrity(b"altered content").unwrap_err();
        assert!(matches!(err, SigningError::Mismatch));
    }

    #[test]
    fn a_signature_fails_against_a_different_key() {
        let signer = KeyPair::generate();
        let other = KeyPair::generate();
        let content = b"content";

        let signature = signer.sign(content);
        let err = other.public().verify(content, &signature).unwrap_err();
        assert!(matches!(err, SigningError::Mismatch));
    }

    #[test]
    fn an_untrusted_key_is_rejected_even_when_the_signature_is_valid() {
        // The attack this closes: anyone can generate a key, sign a malicious
        // package with it, and produce a signature that verifies against itself.
        let attacker = KeyPair::generate();
        let publisher = KeyPair::generate();
        let content = b"malicious package";

        let signature = attacker.sign(content);
        assert!(
            signature.verify_integrity(content).is_ok(),
            "the signature is internally consistent"
        );

        let err = signature
            .verify_against_trusted(content, &[publisher.public()])
            .unwrap_err();
        assert!(
            matches!(err, SigningError::UntrustedKey(_)),
            "but it must not verify against the trusted set"
        );
    }

    #[test]
    fn a_trusted_key_verifies() {
        let publisher = KeyPair::generate();
        let content = b"legitimate package";
        let signature = publisher.sign(content);

        assert!(signature.verify_against_trusted(content, &[publisher.public()]).is_ok());
    }

    #[test]
    fn one_of_several_trusted_keys_is_enough() {
        let publisher = KeyPair::generate();
        let signature = publisher.sign(b"content");
        let trusted =
            vec![KeyPair::generate().public(), publisher.public(), KeyPair::generate().public()];

        assert!(signature.verify_against_trusted(b"content", &trusted).is_ok());
    }

    #[test]
    fn keys_round_trip_through_base64() {
        let keys = KeyPair::generate();
        let public = keys.public();

        let restored = PublicKey::from_base64(&public.to_base64()).unwrap();
        assert_eq!(restored, public);

        let restored_pair = KeyPair::from_private_base64(&keys.to_private_base64()).unwrap();
        assert_eq!(restored_pair.public(), public);

        // A key restored from its private half signs identically.
        let content = b"content";
        assert!(public.verify(content, &restored_pair.sign(content)).is_ok());
    }

    #[test]
    fn malformed_key_material_is_reported_not_panicked() {
        assert!(PublicKey::from_base64("not base64 at all!!!").is_err());
        assert!(PublicKey::from_base64("dG9vIHNob3J0").is_err());
        assert!(KeyPair::from_private_base64("").is_err());
    }

    #[test]
    fn a_malformed_signature_is_reported() {
        let keys = KeyPair::generate();
        let mut signature = keys.sign(b"content");
        signature.value = "not-a-signature".to_string();

        assert!(matches!(
            signature.verify_integrity(b"content"),
            Err(SigningError::MalformedSignature(_))
        ));
    }

    #[test]
    fn fingerprints_are_short_stable_and_distinct() {
        let first = KeyPair::generate().public();
        let second = KeyPair::generate().public();

        assert_eq!(first.fingerprint().len(), 16);
        assert_eq!(first.fingerprint(), first.fingerprint());
        assert_ne!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn the_private_key_never_appears_in_debug_output() {
        let keys = KeyPair::generate();
        let debug = format!("{keys:?}");
        assert!(
            !debug.contains(&keys.to_private_base64()),
            "the private key leaked into Debug output"
        );
        assert!(debug.contains(&keys.public().fingerprint()));
    }

    #[test]
    fn signatures_round_trip_through_json() {
        let keys = KeyPair::generate();
        let signature = keys.sign(b"content");

        let json = serde_json::to_string(&signature).unwrap();
        let restored: Signature = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, signature);
        assert!(restored.verify_integrity(b"content").is_ok());

        // Sigstore-only fields are omitted for an Ed25519 signature.
        assert!(!json.contains("certificate"));
    }

    #[test]
    fn the_digest_covers_every_field() {
        let base = content_digest(b"manifest", b"component", &[]);

        assert_ne!(base, content_digest(b"MANIFEST", b"component", &[]));
        assert_ne!(base, content_digest(b"manifest", b"COMPONENT", &[]));
        assert_ne!(
            base,
            content_digest(b"manifest", b"component", &[("a.txt".into(), b"x".to_vec())])
        );
    }

    #[test]
    fn the_digest_does_not_depend_on_asset_order() {
        // Directory iteration order varies by filesystem; a digest that depended
        // on it would make a rebuild fail to verify.
        let assets_one = vec![
            ("b.txt".to_string(), b"second".to_vec()),
            ("a.txt".to_string(), b"first".to_vec()),
        ];
        let assets_two = vec![
            ("a.txt".to_string(), b"first".to_vec()),
            ("b.txt".to_string(), b"second".to_vec()),
        ];

        assert_eq!(
            content_digest(b"m", b"c", &assets_one),
            content_digest(b"m", b"c", &assets_two)
        );
    }

    #[test]
    fn field_boundaries_cannot_be_shifted() {
        // Without length prefixes, ("ab", "c") and ("a", "bc") would hash the
        // same, letting content move between fields undetected.
        assert_ne!(content_digest(b"ab", b"c", &[]), content_digest(b"a", b"bc", &[]));
    }

    #[test]
    fn generated_keys_are_distinct() {
        let mut fingerprints: Vec<String> =
            (0..16).map(|_| KeyPair::generate().public().fingerprint()).collect();
        let count = fingerprints.len();
        fingerprints.sort();
        fingerprints.dedup();
        assert_eq!(fingerprints.len(), count, "key generation is not producing unique keys");
    }
}
