//! The extension manifest.

use std::collections::BTreeSet;

use nebula_wasm_host::Capability;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{PkgError, Result};

/// Who published an extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    /// Display name.
    pub name: String,
    /// Contact address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Homepage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// An extension's declared identity and requirements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Reverse-DNS identifier, unique in the registry.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Version.
    pub version: Version,
    /// One-line description.
    #[serde(default)]
    pub description: String,
    /// Who published it.
    pub author: Author,
    /// SPDX licence identifier.
    #[serde(default)]
    pub license: String,
    /// The host world version this extension targets.
    pub world_version: Version,
    /// The capabilities it declares.
    ///
    /// Checked against the component's real imports during notarisation, so this
    /// is a claim the pipeline verifies rather than trusts.
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    /// Languages it contributes support for.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Commands it registers.
    #[serde(default)]
    pub commands: Vec<Command>,
    /// Keywords for search.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Repository URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// A command an extension registers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// The identifier passed to `handle-command`.
    pub id: String,
    /// The title shown in the palette.
    pub title: String,
}

impl Manifest {
    /// Parse a manifest from TOML.
    pub fn from_toml(source: &str) -> Result<Self> {
        let manifest: Manifest =
            toml::from_str(source).map_err(|e| PkgError::Manifest(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Serialise to TOML.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| PkgError::Manifest(e.to_string()))
    }

    /// Check the manifest is well-formed.
    pub fn validate(&self) -> Result<()> {
        if !is_valid_id(&self.id) {
            return Err(PkgError::Manifest(format!(
                "`{}` is not a valid extension id; expected reverse-DNS such as `com.example.formatter`",
                self.id
            )));
        }
        if self.name.trim().is_empty() {
            return Err(PkgError::Manifest("name must not be empty".to_string()));
        }
        if self.name.chars().count() > 64 {
            return Err(PkgError::Manifest("name must be 64 characters or fewer".to_string()));
        }
        if self.description.chars().count() > 280 {
            return Err(PkgError::Manifest(
                "description must be 280 characters or fewer".to_string(),
            ));
        }

        let mut ids = BTreeSet::new();
        for command in &self.commands {
            if command.id.trim().is_empty() {
                return Err(PkgError::Manifest("a command id must not be empty".to_string()));
            }
            if !ids.insert(&command.id) {
                return Err(PkgError::Manifest(format!(
                    "command id `{}` is declared twice",
                    command.id
                )));
            }
        }

        // A command that cannot be registered is a manifest that lies to the
        // user about what the extension does.
        if !self.commands.is_empty() && !self.capabilities.contains(&Capability::RegisterCommands) {
            return Err(PkgError::Manifest(
                "commands are declared but the `register-commands` capability is not".to_string(),
            ));
        }
        if !self.languages.is_empty() && !self.capabilities.contains(&Capability::ProvideLanguage) {
            return Err(PkgError::Manifest(
                "languages are declared but the `provide-language` capability is not".to_string(),
            ));
        }

        Ok(())
    }

    /// The capabilities that require the user's consent at install time.
    pub fn consent_required(&self) -> Vec<Capability> {
        let mut capabilities: Vec<Capability> =
            self.capabilities.iter().copied().filter(|c| c.requires_consent()).collect();
        capabilities.sort();
        capabilities
    }

    /// The text of the install prompt.
    pub fn consent_prompt(&self) -> String {
        let capabilities = self.consent_required();
        if capabilities.is_empty() {
            return format!(
                "{} {} does not request any sensitive permissions.",
                self.name, self.version
            );
        }
        let mut prompt =
            format!("{} {} by {} would like to:\n", self.name, self.version, self.author.name);
        for capability in capabilities {
            prompt.push_str(&format!("  • {}\n", capability.describe()));
        }
        prompt
    }
}

/// Whether `id` is a plausible reverse-DNS identifier.
fn is_valid_id(id: &str) -> bool {
    if id.len() < 3 || id.len() > 128 {
        return false;
    }
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 2 {
        return false;
    }
    segments.iter().all(|segment| {
        !segment.is_empty()
            && segment.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest_toml() -> &'static str {
        r#"
id = "com.example.formatter"
name = "Example Formatter"
version = "1.2.3"
description = "Formats example files"
license = "MIT"
world_version = "0.1.0"
capabilities = ["read-document", "edit-document", "register-commands"]

[author]
name = "Example Author"
email = "author@example.com"

[[commands]]
id = "format"
title = "Format Document"
"#
    }

    #[test]
    fn a_valid_manifest_parses() {
        let manifest = Manifest::from_toml(valid_manifest_toml()).unwrap();

        assert_eq!(manifest.id, "com.example.formatter");
        assert_eq!(manifest.version, Version::new(1, 2, 3));
        assert_eq!(manifest.author.name, "Example Author");
        assert!(manifest.capabilities.contains(&Capability::EditDocument));
        assert_eq!(manifest.commands[0].id, "format");
    }

    #[test]
    fn manifests_round_trip_through_toml() {
        let manifest = Manifest::from_toml(valid_manifest_toml()).unwrap();
        let round_tripped = Manifest::from_toml(&manifest.to_toml().unwrap()).unwrap();
        assert_eq!(round_tripped, manifest);
    }

    #[test]
    fn identifiers_must_be_reverse_dns() {
        assert!(is_valid_id("com.example.thing"));
        assert!(is_valid_id("dev.nebula.official-formatter"));
        assert!(is_valid_id("io.a.b"));

        assert!(!is_valid_id("formatter"), "a bare name is ambiguous in a registry");
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("com..example"));
        assert!(!is_valid_id("com.Example.Thing"), "uppercase would collide case-insensitively");
        assert!(!is_valid_id("1com.example"), "a segment must start with a letter");
        assert!(!is_valid_id("com.example.thing!"));
    }

    #[test]
    fn an_invalid_id_is_rejected_with_an_actionable_message() {
        let toml = valid_manifest_toml().replace("com.example.formatter", "formatter");
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(err.to_string().contains("reverse-DNS"), "{err}");
    }

    #[test]
    fn an_empty_name_is_rejected() {
        let toml = valid_manifest_toml().replace("Example Formatter", "");
        assert!(Manifest::from_toml(&toml).is_err());
    }

    #[test]
    fn an_overlong_description_is_rejected() {
        let toml =
            valid_manifest_toml().replace("Formats example files", &"x".repeat(300));
        assert!(Manifest::from_toml(&toml).is_err());
    }

    #[test]
    fn declaring_a_command_without_the_capability_is_rejected() {
        // Otherwise the palette would advertise a command that can never run.
        let toml = valid_manifest_toml().replace(
            r#"capabilities = ["read-document", "edit-document", "register-commands"]"#,
            r#"capabilities = ["read-document"]"#,
        );
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(err.to_string().contains("register-commands"), "{err}");
    }

    #[test]
    fn declaring_a_language_without_the_capability_is_rejected() {
        // `languages` must land at the top level, before any table header.
        let toml = valid_manifest_toml().replace(
            r#"capabilities = ["read-document", "edit-document", "register-commands"]"#,
            "capabilities = [\"read-document\", \"edit-document\", \"register-commands\"]\nlanguages = [\"example\"]",
        );
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(err.to_string().contains("provide-language"), "{err}");
    }

    #[test]
    fn duplicate_command_ids_are_rejected() {
        let toml = format!(
            "{}\n[[commands]]\nid = \"format\"\ntitle = \"Format Again\"\n",
            valid_manifest_toml()
        );
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");
    }

    #[test]
    fn only_sensitive_capabilities_appear_in_the_consent_prompt() {
        let toml = valid_manifest_toml().replace(
            r#"capabilities = ["read-document", "edit-document", "register-commands"]"#,
            r#"capabilities = ["read-document", "register-commands", "network", "spawn-process"]"#,
        );
        let manifest = Manifest::from_toml(&toml).unwrap();

        let required = manifest.consent_required();
        assert!(required.contains(&Capability::Network));
        assert!(required.contains(&Capability::SpawnProcess));
        assert!(
            !required.contains(&Capability::RegisterCommands),
            "prompting for harmless capabilities trains users to click through"
        );

        let prompt = manifest.consent_prompt();
        assert!(prompt.contains("Example Formatter"));
        assert!(prompt.contains("Example Author"));
        assert!(prompt.contains("run programs on your computer"));
        assert!(prompt.contains("make network requests"));
    }

    #[test]
    fn a_harmless_extension_says_so_rather_than_showing_an_empty_prompt() {
        let toml = valid_manifest_toml().replace(
            r#"capabilities = ["read-document", "edit-document", "register-commands"]"#,
            r#"capabilities = ["read-document", "register-commands"]"#,
        );
        let manifest = Manifest::from_toml(&toml).unwrap();

        assert!(manifest.consent_required().is_empty());
        assert!(manifest.consent_prompt().contains("does not request any sensitive"));
    }

    #[test]
    fn an_unknown_capability_is_rejected_rather_than_ignored() {
        let toml = valid_manifest_toml().replace(
            r#""register-commands"]"#,
            r#""register-commands", "read-your-email"]"#,
        );
        assert!(
            Manifest::from_toml(&toml).is_err(),
            "an unrecognised capability must not be silently dropped"
        );
    }

    #[test]
    fn a_malformed_version_is_rejected() {
        let toml = valid_manifest_toml().replace(r#"version = "1.2.3""#, r#"version = "one""#);
        assert!(Manifest::from_toml(&toml).is_err());
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        assert!(Manifest::from_toml("id = \"com.example.thing\"").is_err());
        assert!(Manifest::from_toml("").is_err());
    }
}
