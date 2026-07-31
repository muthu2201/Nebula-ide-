//! Locating, building and packaging an extension project.

use std::path::{Path, PathBuf};

use nebula_pkg::{KeyPair, Manifest, NotarizationVerdict, Package, PackageContents};

use crate::{Result, SdkError, WASM_TARGET};

/// An extension project on disk.
#[derive(Debug, Clone)]
pub struct Project {
    /// The directory holding `nebula.toml`.
    pub root: PathBuf,
    /// The parsed manifest.
    pub manifest: Manifest,
}

/// What a build produced.
#[derive(Debug, Clone)]
pub struct BuildOutput {
    /// The compiled component.
    pub component: Vec<u8>,
    /// Where it was written.
    pub component_path: PathBuf,
    /// Assets found under `assets/`.
    pub assets: Vec<(String, Vec<u8>)>,
}

/// The manifest file's name.
pub const MANIFEST_FILE: &str = "nebula.toml";

impl Project {
    /// Find the project containing `start`, walking up to the filesystem root.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        let start = start.as_ref().to_path_buf();
        let mut directory = start.canonicalize().unwrap_or(start.clone());

        loop {
            let candidate = directory.join(MANIFEST_FILE);
            if candidate.is_file() {
                let manifest = Manifest::from_toml(&std::fs::read_to_string(&candidate)?)?;
                return Ok(Self { root: directory, manifest });
            }
            if !directory.pop() {
                return Err(SdkError::NotAProject(start));
            }
        }
    }

    /// Where the packaged extension is written.
    pub fn package_path(&self) -> PathBuf {
        self.root.join("target").join(format!(
            "{}-{}.{}",
            self.manifest.id, self.manifest.version, nebula_pkg::PACKAGE_EXTENSION
        ))
    }

    /// Compile the extension to a WebAssembly component.
    pub fn build(&self, release: bool) -> Result<BuildOutput> {
        // Check the toolchain before running a build that would fail with a
        // message about a missing target buried in cargo's output.
        which::which("cargo").map_err(|_| SdkError::NoCargo)?;
        self.check_target_installed()?;

        let mut command = nebula_exec::Command::new("cargo")
            .arg("build")
            .arg("--target")
            .arg(WASM_TARGET)
            .current_dir(&self.root)
            // A build needs the developer's environment: their cargo registry
            // credentials, their PATH, their RUSTFLAGS.
            .inherit_env(true)
            .limits(nebula_exec::ResourceLimits::build());
        if release {
            command = command.arg("--release");
        }

        let output = command.run_blocking()?;
        if !output.is_success() {
            return Err(SdkError::BuildFailed(output.combined()));
        }

        let profile = if release { "release" } else { "debug" };
        let artifacts = self.root.join("target").join(WASM_TARGET).join(profile);
        let component_path = find_component(&artifacts)?;
        let component = std::fs::read(&component_path)?;

        Ok(BuildOutput { component, component_path, assets: self.collect_assets()? })
    }

    /// Read everything under `assets/`.
    fn collect_assets(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let assets_dir = self.root.join("assets");
        if !assets_dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut assets = Vec::new();
        let mut stack = vec![assets_dir.clone()];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(relative) = path.strip_prefix(&assets_dir) else {
                    continue;
                };
                // Forward slashes, so a package built on Windows and one built
                // on Linux produce the same digest.
                let name = relative.to_string_lossy().replace('\\', "/");
                assets.push((name, std::fs::read(&path)?));
            }
        }
        assets.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(assets)
    }

    /// Run the same notarisation the registry will.
    ///
    /// Locally, before an upload, so a developer finds out about an undeclared
    /// capability in seconds rather than after a rejected publish.
    pub fn check(&self, component: &[u8]) -> Result<nebula_pkg::NotarizationReport> {
        let report = nebula_pkg::notarize(&self.manifest, component);
        if let NotarizationVerdict::Rejected { reasons } = &report.verdict {
            return Err(SdkError::WouldBeRejected(
                reasons.iter().map(|r| format!("  • {r}")).collect::<Vec<_>>().join("\n"),
            ));
        }
        Ok(report)
    }

    /// Build, check and sign, writing the package.
    pub fn package(&self, release: bool, keys: &KeyPair) -> Result<PathBuf> {
        let built = self.build(release)?;
        self.check(&built.component)?;

        let package = Package::sign(
            PackageContents {
                manifest: self.manifest.clone(),
                component: built.component,
                assets: built.assets,
            },
            keys,
        )?;

        let path = self.package_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        package.write(&path)?;
        Ok(path)
    }

    fn check_target_installed(&self) -> Result<()> {
        let output = nebula_exec::Command::new("rustup")
            .arg("target")
            .arg("list")
            .arg("--installed")
            .inherit_env(true)
            .limits(nebula_exec::ResourceLimits::quick())
            .run_blocking();

        match output {
            // No rustup: the developer may be on a distribution toolchain, where
            // the target either works or produces a clear error from cargo.
            Err(_) => Ok(()),
            Ok(output) if !output.is_success() => Ok(()),
            Ok(output) => {
                if output.stdout.lines().any(|line| line.trim() == WASM_TARGET) {
                    Ok(())
                } else {
                    Err(SdkError::MissingTarget(WASM_TARGET.to_string()))
                }
            }
        }
    }
}

/// Find the single `.wasm` artefact a build produced.
fn find_component(directory: &Path) -> Result<PathBuf> {
    let entries = std::fs::read_dir(directory)
        .map_err(|_| SdkError::NoArtifact(directory.to_path_buf()))?;

    let mut candidates: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wasm"))
        .collect();

    // Deterministic when a project somehow produces several.
    candidates.sort();
    candidates.into_iter().next().ok_or_else(|| SdkError::NoArtifact(directory.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_pkg::manifest::Author;
    use semver::Version;
    use tempfile::TempDir;

    fn manifest_toml() -> &'static str {
        r#"
id = "com.example.formatter"
name = "Example Formatter"
version = "0.1.0"
description = "Formats example files"
license = "MIT"
world_version = "0.1.0"
capabilities = ["read-document", "edit-document"]

[author]
name = "Example Author"
"#
    }

    fn project_at(root: &Path) -> Project {
        std::fs::write(root.join(MANIFEST_FILE), manifest_toml()).unwrap();
        Project::discover(root).unwrap()
    }

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

    #[test]
    fn a_project_is_found_in_the_current_directory() {
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());

        assert_eq!(project.manifest.id, "com.example.formatter");
        assert_eq!(project.manifest.version, Version::new(0, 1, 0));
    }

    #[test]
    fn a_project_is_found_from_a_subdirectory() {
        // Running `nebula-sdk build` from `src/` must work, as `cargo` does.
        let dir = TempDir::new().unwrap();
        project_at(dir.path());

        let nested = dir.path().join("src").join("deeply").join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let found = Project::discover(&nested).unwrap();
        assert_eq!(found.manifest.id, "com.example.formatter");
        assert_eq!(found.root.canonicalize().unwrap(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn a_directory_with_no_manifest_is_reported_clearly() {
        let dir = TempDir::new().unwrap();
        let err = Project::discover(dir.path()).unwrap_err();

        assert!(matches!(err, SdkError::NotAProject(_)));
        assert!(err.to_string().contains("nebula.toml"), "{err}");
    }

    #[test]
    fn a_malformed_manifest_is_reported_rather_than_ignored() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(MANIFEST_FILE), "id = \"not-reverse-dns\"").unwrap();

        assert!(Project::discover(dir.path()).is_err());
    }

    #[test]
    fn the_package_path_names_the_extension_and_version() {
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());

        let path = project.package_path();
        let name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "com.example.formatter-0.1.0.nbx");
    }

    #[test]
    fn checking_accepts_a_component_that_imports_nothing() {
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());

        let report = project.check(&component()).unwrap();
        assert!(report.undeclared.is_empty());
    }

    #[test]
    fn checking_rejects_a_core_module_before_an_upload_can_fail() {
        // The mistake a developer makes once: building for wasip1.
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());
        let core_module = wat::parse_str(r#"(module (func (export "run")))"#).unwrap();

        let err = project.check(&core_module).unwrap_err();
        assert!(matches!(err, SdkError::WouldBeRejected(_)));
        assert!(err.to_string().contains("Component Model"), "{err}");
    }

    #[test]
    fn assets_are_collected_recursively_with_forward_slashes() {
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());

        std::fs::create_dir_all(dir.path().join("assets/themes/dark")).unwrap();
        std::fs::write(dir.path().join("assets/icon.png"), b"png").unwrap();
        std::fs::write(dir.path().join("assets/themes/dark/theme.json"), b"{}").unwrap();

        let assets = project.collect_assets().unwrap();
        let names: Vec<&str> = assets.iter().map(|(name, _)| name.as_str()).collect();

        assert_eq!(names, vec!["icon.png", "themes/dark/theme.json"]);
    }

    #[test]
    fn a_project_with_no_assets_directory_collects_nothing() {
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());
        assert!(project.collect_assets().unwrap().is_empty());
    }

    #[test]
    fn asset_collection_is_deterministic() {
        // Directory iteration order varies; the package digest must not.
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        for name in ["c.txt", "a.txt", "b.txt"] {
            std::fs::write(dir.path().join("assets").join(name), name).unwrap();
        }

        assert_eq!(project.collect_assets().unwrap(), project.collect_assets().unwrap());
    }

    #[test]
    fn packaging_produces_a_signed_archive_the_registry_would_accept() {
        // The full local path, short of invoking cargo: check, sign, write,
        // reopen, verify.
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());
        let keys = KeyPair::generate();

        let package = Package::sign(
            PackageContents {
                manifest: project.manifest.clone(),
                component: component(),
                assets: Vec::new(),
            },
            &keys,
        )
        .unwrap();

        let path = project.package_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        package.write(&path).unwrap();

        let reopened = Package::open_verified(&path, &[keys.public()]).unwrap();
        assert_eq!(reopened.contents.manifest.id, "com.example.formatter");

        let report = nebula_pkg::notarize(&reopened.contents.manifest, &reopened.contents.component);
        assert!(!report.verdict.is_rejected(), "{report:?}");
    }

    #[test]
    fn finding_the_artefact_is_deterministic_and_reports_when_there_is_none() {
        let dir = TempDir::new().unwrap();
        assert!(find_component(dir.path()).is_err());

        std::fs::write(dir.path().join("zebra.wasm"), b"").unwrap();
        std::fs::write(dir.path().join("alpha.wasm"), b"").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();

        assert_eq!(find_component(dir.path()).unwrap().file_name().unwrap(), "alpha.wasm");
    }

    #[test]
    fn the_manifest_author_survives_a_round_trip() {
        let dir = TempDir::new().unwrap();
        let project = project_at(dir.path());
        assert_eq!(
            project.manifest.author,
            Author { name: "Example Author".to_string(), email: None, url: None }
        );
    }
}
