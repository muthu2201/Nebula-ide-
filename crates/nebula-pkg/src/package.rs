//! Building and opening `.nbx` packages.

use std::io::{Read, Write};
use std::path::Path;

use crate::manifest::Manifest;
use crate::signature::{KeyPair, PublicKey, Signature, content_digest};
use crate::{PkgError, Result, files};

/// The contents of a package, before or after archiving.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageContents {
    /// The manifest.
    pub manifest: Manifest,
    /// The component binary.
    pub component: Vec<u8>,
    /// Assets, keyed by their path relative to `assets/`.
    pub assets: Vec<(String, Vec<u8>)>,
}

impl PackageContents {
    /// The digest a signature is made over.
    pub fn digest(&self) -> Result<Vec<u8>> {
        let manifest = self.manifest.to_toml()?;
        Ok(content_digest(manifest.as_bytes(), &self.component, &self.assets))
    }
}

/// A signed package.
#[derive(Debug, Clone)]
pub struct Package {
    /// What is inside.
    pub contents: PackageContents,
    /// The signature over its digest.
    pub signature: Signature,
}

impl Package {
    /// Sign `contents` with `keys`.
    pub fn sign(contents: PackageContents, keys: &KeyPair) -> Result<Self> {
        let signature = keys.sign(&contents.digest()?);
        Ok(Self { contents, signature })
    }

    /// Verify the signature is internally consistent.
    ///
    /// Proves integrity only. Anyone can generate a key and sign anything, so an
    /// installer must use [`Package::verify_against_trusted`].
    pub fn verify_integrity(&self) -> Result<()> {
        self.signature.verify_integrity(&self.contents.digest()?)?;
        Ok(())
    }

    /// Verify the signature was made by one of `trusted`.
    pub fn verify_against_trusted(&self, trusted: &[PublicKey]) -> Result<()> {
        self.signature.verify_against_trusted(&self.contents.digest()?, trusted)?;
        Ok(())
    }

    /// Write the package as a gzipped tar.
    pub fn write(&self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = self.to_bytes()?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Serialise the package to bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);

        let manifest = self.contents.manifest.to_toml()?;
        append(&mut archive, files::MANIFEST, manifest.as_bytes())?;
        append(&mut archive, files::COMPONENT, &self.contents.component)?;

        let signature = serde_json::to_vec_pretty(&self.signature)
            .map_err(|e| PkgError::Archive(e.to_string()))?;
        append(&mut archive, files::SIGNATURE, &signature)?;

        // Sorted, so that building the same contents twice produces the same
        // archive layout.
        let mut assets: Vec<&(String, Vec<u8>)> = self.contents.assets.iter().collect();
        assets.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, bytes) in assets {
            append(&mut archive, &format!("{}{name}", files::ASSETS), bytes)?;
        }

        let encoder = archive.into_inner().map_err(|e| PkgError::Archive(e.to_string()))?;
        encoder.finish().map_err(|e| PkgError::Archive(e.to_string()))
    }

    /// Read a package from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let decoder = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decoder);

        let mut manifest_bytes: Option<Vec<u8>> = None;
        let mut component: Option<Vec<u8>> = None;
        let mut signature_bytes: Option<Vec<u8>> = None;
        let mut assets: Vec<(String, Vec<u8>)> = Vec::new();

        for entry in archive.entries().map_err(|e| PkgError::Archive(e.to_string()))? {
            let mut entry = entry.map_err(|e| PkgError::Archive(e.to_string()))?;
            let path = entry
                .path()
                .map_err(|e| PkgError::Archive(e.to_string()))?
                .to_string_lossy()
                .to_string();

            // A tar entry naming `../` or an absolute path is the classic
            // archive-extraction escape. Nebula never extracts to disk from
            // here, but rejecting them keeps a future extractor safe and is a
            // strong signal the package is hostile.
            if path.contains("..") || path.starts_with('/') {
                return Err(PkgError::Archive(format!(
                    "package contains an unsafe entry path: {path}"
                )));
            }

            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|e| PkgError::Archive(e.to_string()))?;

            match path.as_str() {
                files::MANIFEST => manifest_bytes = Some(bytes),
                files::COMPONENT => component = Some(bytes),
                files::SIGNATURE => signature_bytes = Some(bytes),
                other if other.starts_with(files::ASSETS) => {
                    assets.push((other[files::ASSETS.len()..].to_string(), bytes));
                }
                other => {
                    // An unexpected file is not fatal, but it is worth noticing:
                    // it is not covered by the digest and so is not signed.
                    tracing::warn!(path = other, "ignoring an unexpected file in a package");
                }
            }
        }

        let manifest_bytes =
            manifest_bytes.ok_or_else(|| PkgError::MissingFile(files::MANIFEST.to_string()))?;
        let component =
            component.ok_or_else(|| PkgError::MissingFile(files::COMPONENT.to_string()))?;
        let signature_bytes =
            signature_bytes.ok_or_else(|| PkgError::MissingFile(files::SIGNATURE.to_string()))?;

        let manifest = Manifest::from_toml(
            std::str::from_utf8(&manifest_bytes)
                .map_err(|e| PkgError::Manifest(format!("manifest is not UTF-8: {e}")))?,
        )?;
        let signature: Signature = serde_json::from_slice(&signature_bytes)
            .map_err(|e| PkgError::Archive(format!("signature.json is malformed: {e}")))?;

        assets.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(Self { contents: PackageContents { manifest, component, assets }, signature })
    }

    /// Read a package from a file.
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    /// Open a package and verify it in one step.
    ///
    /// The order matters: verify before anything is written to disk or handed to
    /// the WASM host, so an unsigned or altered package never reaches code that
    /// would act on it.
    pub fn open_verified(path: impl AsRef<Path>, trusted: &[PublicKey]) -> Result<Self> {
        let package = Self::read(path)?;
        package.verify_against_trusted(trusted)?;
        Ok(package)
    }
}

fn append<W: Write>(archive: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    // A fixed mtime keeps the archive reproducible: the same contents build
    // byte-identically on any machine at any time.
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();

    archive.append_data(&mut header, name, bytes).map_err(|e| PkgError::Archive(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Author;
    use nebula_wasm_host::Capability;
    use semver::Version;
    use tempfile::TempDir;

    fn component() -> Vec<u8> {
        wat::parse_str(
            r#"
            (component
              (core module $m (func (export "run") (result i32) i32.const 7))
              (core instance $i (instantiate $m))
              (func (export "run") (result s32) (canon lift (core func $i "run")))
            )
            "#,
        )
        .unwrap()
    }

    fn contents() -> PackageContents {
        PackageContents {
            manifest: Manifest {
                id: "com.example.formatter".to_string(),
                name: "Formatter".to_string(),
                version: Version::new(1, 0, 0),
                description: "Formats things".to_string(),
                author: Author {
                    name: "Author".to_string(),
                    email: Some("a@example.com".to_string()),
                    url: None,
                },
                license: "MIT".to_string(),
                world_version: Version::new(0, 1, 0),
                capabilities: [Capability::ReadDocument, Capability::EditDocument]
                    .into_iter()
                    .collect(),
                languages: Vec::new(),
                commands: Vec::new(),
                keywords: vec!["format".to_string()],
                repository: None,
            },
            component: component(),
            assets: vec![
                ("themes/dark.json".to_string(), b"{\"name\":\"dark\"}".to_vec()),
                ("icon.png".to_string(), vec![0x89, 0x50, 0x4E, 0x47]),
            ],
        }
    }

    #[test]
    fn a_package_round_trips_through_its_archive() {
        let keys = KeyPair::generate();
        let package = Package::sign(contents(), &keys).unwrap();

        let bytes = package.to_bytes().unwrap();
        let reopened = Package::from_bytes(&bytes).unwrap();

        assert_eq!(reopened.contents.manifest, package.contents.manifest);
        assert_eq!(reopened.contents.component, package.contents.component);
        assert_eq!(reopened.contents.assets.len(), 2);
        assert!(reopened.verify_integrity().is_ok());
    }

    #[test]
    fn a_package_round_trips_through_a_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("formatter.nbx");
        let keys = KeyPair::generate();

        Package::sign(contents(), &keys).unwrap().write(&path).unwrap();
        let reopened = Package::open_verified(&path, &[keys.public()]).unwrap();

        assert_eq!(reopened.contents.manifest.id, "com.example.formatter");
    }

    #[test]
    fn a_package_signed_by_an_untrusted_key_is_refused_on_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("evil.nbx");

        let attacker = KeyPair::generate();
        Package::sign(contents(), &attacker).unwrap().write(&path).unwrap();

        let publisher = KeyPair::generate();
        let err = Package::open_verified(&path, &[publisher.public()]).unwrap_err();
        assert!(matches!(err, PkgError::Signature(_)), "{err:?}");
    }

    #[test]
    fn altering_the_component_invalidates_the_signature() {
        // The property that matters: a package modified in transit must not
        // verify.
        let keys = KeyPair::generate();
        let mut package = Package::sign(contents(), &keys).unwrap();

        package.contents.component.extend_from_slice(b"\x00malicious payload");

        assert!(package.verify_integrity().is_err(), "a modified component must not verify");
    }

    #[test]
    fn altering_the_manifest_invalidates_the_signature() {
        // Specifically: an attacker cannot widen the declared capabilities of a
        // signed package.
        let keys = KeyPair::generate();
        let mut package = Package::sign(contents(), &keys).unwrap();

        package.contents.manifest.capabilities.insert(Capability::SpawnProcess);
        assert!(package.verify_integrity().is_err());
    }

    #[test]
    fn altering_an_asset_invalidates_the_signature() {
        let keys = KeyPair::generate();
        let mut package = Package::sign(contents(), &keys).unwrap();

        package.contents.assets[0].1 = b"replaced".to_vec();
        assert!(package.verify_integrity().is_err());
    }

    #[test]
    fn adding_an_asset_invalidates_the_signature() {
        let keys = KeyPair::generate();
        let mut package = Package::sign(contents(), &keys).unwrap();

        package.contents.assets.push(("extra.txt".to_string(), b"smuggled".to_vec()));
        assert!(package.verify_integrity().is_err());
    }

    #[test]
    fn the_archive_is_reproducible() {
        // The same contents must build byte-identically, or a rebuild cannot be
        // compared against a published artefact.
        let keys = KeyPair::from_private_base64(&KeyPair::generate().to_private_base64()).unwrap();
        let first =
            Package { contents: contents(), signature: keys.sign(&contents().digest().unwrap()) };
        let second = Package { contents: contents(), signature: first.signature.clone() };

        assert_eq!(
            first.to_bytes().unwrap(),
            second.to_bytes().unwrap(),
            "archive bytes must not depend on the machine or the clock"
        );
    }

    #[test]
    fn asset_order_does_not_affect_the_digest() {
        let mut reordered = contents();
        reordered.assets.reverse();
        assert_eq!(contents().digest().unwrap(), reordered.digest().unwrap());
    }

    #[test]
    fn a_package_missing_its_manifest_is_rejected() {
        // Build an archive by hand with the manifest left out.
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append(&mut archive, files::COMPONENT, &component()).unwrap();
        append(&mut archive, files::SIGNATURE, b"{}").unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();

        let err = Package::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, PkgError::MissingFile(name) if name == files::MANIFEST));
    }

    #[test]
    fn a_package_missing_its_signature_is_rejected() {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let manifest = contents().manifest.to_toml().unwrap();
        append(&mut archive, files::MANIFEST, manifest.as_bytes()).unwrap();
        append(&mut archive, files::COMPONENT, &component()).unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();

        let err = Package::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, PkgError::MissingFile(name) if name == files::SIGNATURE));
    }

    /// Build a tar archive containing one entry with an arbitrary, unvalidated
    /// name.
    ///
    /// `tar::Builder` refuses to *write* a traversing path, which is why the
    /// header is filled in by hand here — a hostile package would be produced by
    /// a tool that does not have those scruples, and the reader is what has to
    /// refuse it.
    fn archive_with_raw_entry_name(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        // mode, uid, gid: octal, NUL-terminated.
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        // size and mtime, octal.
        let size = format!("{:011o}\0", contents.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        // The checksum field is treated as spaces while the checksum is computed.
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0'; // a regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        let checksum: u32 = header.iter().map(|b| *b as u32).sum();
        let checksum_field = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum_field.as_bytes());

        let mut tar = Vec::new();
        tar.extend_from_slice(&header);
        tar.extend_from_slice(contents);
        // Entries are padded to a 512-byte boundary.
        tar.resize(tar.len().div_ceil(512) * 512, 0);
        // Two zero blocks terminate the archive.
        tar.extend_from_slice(&[0u8; 1024]);

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn a_traversal_entry_path_is_rejected() {
        // The classic archive escape: an entry named `../../.bashrc`.
        let bytes = archive_with_raw_entry_name("../../.bashrc", b"malicious");

        let err = Package::from_bytes(&bytes).unwrap_err();
        assert!(
            matches!(&err, PkgError::Archive(message) if message.contains("unsafe entry path")),
            "an archive escape must be refused, got {err:?}"
        );
    }

    #[test]
    fn an_absolute_entry_path_is_rejected() {
        let bytes = archive_with_raw_entry_name("/etc/cron.d/backdoor", b"malicious");

        let err = Package::from_bytes(&bytes).unwrap_err();
        assert!(
            matches!(&err, PkgError::Archive(message) if message.contains("unsafe entry path")),
            "an absolute entry path must be refused, got {err:?}"
        );
    }

    #[test]
    fn corrupt_bytes_are_reported_rather_than_panicking() {
        assert!(Package::from_bytes(b"not a gzip archive").is_err());
        assert!(Package::from_bytes(&[]).is_err());

        let keys = KeyPair::generate();
        let mut truncated = Package::sign(contents(), &keys).unwrap().to_bytes().unwrap();
        truncated.truncate(truncated.len() / 2);
        assert!(Package::from_bytes(&truncated).is_err());
    }

    #[test]
    fn a_package_with_no_assets_works() {
        let mut bare = contents();
        bare.assets.clear();

        let keys = KeyPair::generate();
        let package = Package::sign(bare, &keys).unwrap();
        let reopened = Package::from_bytes(&package.to_bytes().unwrap()).unwrap();

        assert!(reopened.contents.assets.is_empty());
        assert!(reopened.verify_integrity().is_ok());
    }

    #[test]
    fn a_signed_package_passes_notarisation_end_to_end() {
        // The full publisher path: build, sign, archive, reopen, verify,
        // notarise.
        let keys = KeyPair::generate();
        let package = Package::sign(contents(), &keys).unwrap();
        let reopened = Package::from_bytes(&package.to_bytes().unwrap()).unwrap();

        reopened.verify_against_trusted(&[keys.public()]).unwrap();

        let report = crate::notarize(&reopened.contents.manifest, &reopened.contents.component);
        assert!(!report.verdict.is_rejected(), "{report:?}");
        assert!(report.undeclared.is_empty());
    }
}
