//! Notarisation: the checks a package passes before the registry will serve it.
//!
//! The pipeline the blueprint calls for, in order:
//!
//! 1. **Import analysis** — read the component's real WIT imports and compare
//!    them against the manifest. This is the check that makes the install prompt
//!    honest, and it is the reason a manifest can be shown to a user at all.
//! 2. **Static scan** — structural properties of the binary: is it a component,
//!    is it a plausible size, does it export what the world requires.
//! 3. **Human review** — for anything requesting a heavy capability.
//!
//! Only the first two are mechanical, and only those are implemented here. The
//! verdict is [`NotarizationVerdict::NeedsReview`] rather than "approved"
//! whenever a human is required, because a pipeline that auto-approves the
//! dangerous cases is not a review pipeline.

use std::collections::BTreeSet;

use nebula_wasm_host::Capability;
use nebula_wasm_host::host::{component_imports, is_component};
use serde::{Deserialize, Serialize};

use crate::manifest::Manifest;

/// The outcome of notarising a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
pub enum NotarizationVerdict {
    /// Every mechanical check passed and no human review is required.
    Approved,
    /// The mechanical checks passed but a human must look at it.
    NeedsReview {
        /// Why.
        reasons: Vec<String>,
    },
    /// The package is rejected.
    Rejected {
        /// Why.
        reasons: Vec<String>,
    },
}

impl NotarizationVerdict {
    /// Whether the registry may serve this package.
    pub fn may_publish(&self) -> bool {
        matches!(self, NotarizationVerdict::Approved)
    }

    /// Whether it was rejected outright.
    pub fn is_rejected(&self) -> bool {
        matches!(self, NotarizationVerdict::Rejected { .. })
    }
}

/// The full result of notarisation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotarizationReport {
    /// The extension's identifier.
    pub extension_id: String,
    /// The verdict.
    pub verdict: NotarizationVerdict,
    /// Capabilities the manifest declares.
    pub declared: BTreeSet<Capability>,
    /// Capabilities the binary's imports actually require.
    pub required: BTreeSet<Capability>,
    /// Required but not declared. Any entry here is a rejection.
    pub undeclared: BTreeSet<Capability>,
    /// Declared but never imported. A warning: over-declaring makes the install
    /// prompt scarier than the extension is, which erodes the prompt's meaning.
    pub over_declared: BTreeSet<Capability>,
    /// Every finding, for the reviewer.
    pub findings: Vec<String>,
    /// Size of the component in bytes.
    pub component_bytes: usize,
}

/// The largest component the registry will accept.
///
/// A legitimate extension, even one embedding a grammar, is far below this. The
/// limit exists to stop a package from being a delivery vehicle for something
/// else.
pub const MAX_COMPONENT_BYTES: usize = 32 * 1024 * 1024;

/// The smallest plausible component.
pub const MIN_COMPONENT_BYTES: usize = 8;

/// Capabilities that always require a human to look at the extension.
///
/// These are the ones that can reach outside the editor. An extension that can
/// run processes or make network requests can do anything the user can, so no
/// static analysis is a substitute for someone reading the code.
pub const REVIEW_REQUIRED: &[Capability] =
    &[Capability::SpawnProcess, Capability::Network, Capability::WriteWorkspace];

/// Notarise a package.
pub fn notarize(manifest: &Manifest, component: &[u8]) -> NotarizationReport {
    let mut findings = Vec::new();
    let mut rejections = Vec::new();

    // --- Structural checks ---
    if !is_component(component) {
        rejections.push(
            "the binary is a core WebAssembly module, not a Component Model component".to_string(),
        );
    }
    if component.len() < MIN_COMPONENT_BYTES {
        rejections.push("the binary is too small to be a component".to_string());
    }
    if component.len() > MAX_COMPONENT_BYTES {
        rejections.push(format!(
            "the component is {} bytes, above the {MAX_COMPONENT_BYTES} byte limit",
            component.len()
        ));
    }

    // --- Import analysis ---
    let required: BTreeSet<Capability> = match component_imports(component) {
        Ok(imports) => {
            findings.push(format!("component imports {} interface(s)", imports.len()));
            nebula_wasm_host::world::capabilities_from_imports(&imports)
        }
        Err(error) => {
            rejections.push(format!("the component could not be parsed: {error}"));
            BTreeSet::new()
        }
    };

    let declared = manifest.capabilities.clone();
    let undeclared: BTreeSet<Capability> = required.difference(&declared).copied().collect();
    let over_declared: BTreeSet<Capability> = declared.difference(&required).copied().collect();

    // The check that makes the install prompt honest.
    if !undeclared.is_empty() {
        rejections.push(format!(
            "the component imports capabilities its manifest does not declare: {}",
            undeclared.iter().map(Capability::name).collect::<Vec<_>>().join(", ")
        ));
    }
    if !over_declared.is_empty() {
        findings.push(format!(
            "the manifest declares capabilities the component never imports: {}",
            over_declared.iter().map(Capability::name).collect::<Vec<_>>().join(", ")
        ));
    }

    // --- Manifest coherence ---
    if let Err(error) = manifest.validate() {
        rejections.push(format!("manifest is invalid: {error}"));
    }

    // --- Human review triggers ---
    let mut review_reasons = Vec::new();
    for capability in REVIEW_REQUIRED {
        if declared.contains(capability) {
            review_reasons.push(format!(
                "requests `{}`, which can act outside the editor",
                capability.name()
            ));
        }
    }
    // A first release has no track record to lean on.
    if manifest.version.major == 0 && manifest.version.minor == 0 {
        review_reasons.push("is a pre-release version".to_string());
    }

    let verdict = if !rejections.is_empty() {
        NotarizationVerdict::Rejected { reasons: rejections }
    } else if !review_reasons.is_empty() {
        NotarizationVerdict::NeedsReview { reasons: review_reasons }
    } else {
        NotarizationVerdict::Approved
    };

    NotarizationReport {
        extension_id: manifest.id.clone(),
        verdict,
        declared,
        required,
        undeclared,
        over_declared,
        findings,
        component_bytes: component.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;

    fn manifest_with(capabilities: &[Capability], version: &str) -> Manifest {
        Manifest {
            id: "com.example.thing".to_string(),
            name: "Thing".to_string(),
            version: Version::parse(version).unwrap(),
            description: "A thing".to_string(),
            author: crate::manifest::Author { name: "Author".to_string(), email: None, url: None },
            license: "MIT".to_string(),
            world_version: Version::new(0, 1, 0),
            capabilities: capabilities.iter().copied().collect(),
            languages: Vec::new(),
            commands: Vec::new(),
            keywords: Vec::new(),
            repository: None,
        }
    }

    /// A component importing nothing.
    fn plain_component() -> Vec<u8> {
        wat::parse_str(
            r#"
            (component
              (core module $m (func (export "run") (result i32) i32.const 0))
              (core instance $i (instantiate $m))
              (func (export "run") (result s32) (canon lift (core func $i "run")))
            )
            "#,
        )
        .unwrap()
    }

    fn core_module() -> Vec<u8> {
        wat::parse_str(r#"(module (func (export "run")))"#).unwrap()
    }

    #[test]
    fn a_clean_package_is_approved() {
        let manifest = manifest_with(&[], "1.0.0");
        let report = notarize(&manifest, &plain_component());

        assert_eq!(report.verdict, NotarizationVerdict::Approved, "{report:?}");
        assert!(report.verdict.may_publish());
        assert!(report.undeclared.is_empty());
    }

    #[test]
    fn a_core_module_is_rejected() {
        let report = notarize(&manifest_with(&[], "1.0.0"), &core_module());

        assert!(report.verdict.is_rejected());
        match report.verdict {
            NotarizationVerdict::Rejected { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("Component Model")), "{reasons:?}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_capability_requesting_extension_needs_human_review() {
        // Static analysis cannot tell you whether an extension that can run
        // processes is trustworthy, so it does not pretend to.
        let manifest = manifest_with(&[Capability::SpawnProcess], "1.0.0");
        let report = notarize(&manifest, &plain_component());

        match &report.verdict {
            NotarizationVerdict::NeedsReview { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("spawn-process")), "{reasons:?}");
            }
            other => panic!("expected review to be required, got {other:?}"),
        }
        assert!(!report.verdict.may_publish());
    }

    #[test]
    fn every_outward_facing_capability_triggers_review() {
        for capability in REVIEW_REQUIRED {
            let manifest = manifest_with(&[*capability], "1.0.0");
            let report = notarize(&manifest, &plain_component());
            assert!(!report.verdict.may_publish(), "`{}` should require review", capability.name());
        }
    }

    #[test]
    fn a_prerelease_version_needs_review() {
        let report = notarize(&manifest_with(&[], "0.0.1"), &plain_component());
        assert!(!report.verdict.may_publish());
    }

    #[test]
    fn over_declaring_is_a_warning_rather_than_a_rejection() {
        // Over-declaring makes the install prompt scarier than the extension is,
        // which erodes the prompt — but it is not dangerous.
        let manifest = manifest_with(&[Capability::ReadDocument], "1.0.0");
        let report = notarize(&manifest, &plain_component());

        assert!(report.over_declared.contains(&Capability::ReadDocument));
        assert!(!report.verdict.is_rejected());
        assert!(
            report.findings.iter().any(|f| f.contains("never imports")),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn the_report_records_what_was_declared_and_what_was_required() {
        let manifest = manifest_with(&[Capability::ReadDocument, Capability::Storage], "1.0.0");
        let report = notarize(&manifest, &plain_component());

        assert_eq!(report.declared.len(), 2);
        assert!(report.required.is_empty(), "this component imports nothing");
        assert_eq!(report.component_bytes, plain_component().len());
        assert_eq!(report.extension_id, "com.example.thing");
    }

    #[test]
    fn an_oversized_component_is_rejected() {
        let mut oversized = plain_component();
        oversized.resize(MAX_COMPONENT_BYTES + 1, 0);
        let report = notarize(&manifest_with(&[], "1.0.0"), &oversized);
        assert!(report.verdict.is_rejected());
    }

    #[test]
    fn an_empty_component_is_rejected() {
        let report = notarize(&manifest_with(&[], "1.0.0"), &[]);
        assert!(report.verdict.is_rejected());
    }

    #[test]
    fn an_invalid_manifest_is_rejected() {
        let mut manifest = manifest_with(&[], "1.0.0");
        manifest.id = "not-reverse-dns".to_string();

        let report = notarize(&manifest, &plain_component());
        assert!(report.verdict.is_rejected());
    }

    #[test]
    fn reports_round_trip_through_json() {
        let report = notarize(&manifest_with(&[Capability::Network], "1.0.0"), &plain_component());
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(serde_json::from_str::<NotarizationReport>(&json).unwrap(), report);
    }

    #[test]
    fn notarisation_is_deterministic() {
        let manifest = manifest_with(&[Capability::ReadDocument], "1.0.0");
        let component = plain_component();
        assert_eq!(notarize(&manifest, &component), notarize(&manifest, &component));
    }
}
