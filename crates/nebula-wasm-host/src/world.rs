//! The WIT world: what an extension may ask the host for.
//!
//! The world is the entire attack surface. An extension can import exactly the
//! interfaces named here and nothing else, so adding an interface is a security
//! decision and removing one is a breaking change.
//!
//! ## Versioning
//!
//! The world is semver'd and the host serves **several versions at once**. An
//! extension built against `0.1` keeps working when `0.2` ships, for at least
//! the twelve-month window the deprecation policy promises — the same shape MCP
//! adopted, and for the same reason: an ecosystem where every host update breaks
//! every extension does not accumulate extensions.

use std::collections::BTreeSet;

use semver::Version;
use serde::{Deserialize, Serialize};

/// A capability an extension can request in its manifest.
///
/// These map one-to-one onto WIT interfaces in the world. The notarisation
/// pipeline reads a component's actual imports and checks them against the
/// manifest, so a component cannot quietly import more than it declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Read the contents of open documents.
    ReadDocument,
    /// Propose edits to documents. Edits are applied by the host, which means
    /// they go through the same undo history as a human's.
    EditDocument,
    /// Read files in the project.
    ReadWorkspace,
    /// Write files in the project.
    WriteWorkspace,
    /// Register commands in the palette.
    RegisterCommands,
    /// Contribute UI: status items, panels.
    ContributeUi,
    /// Provide a language server or grammar.
    ProvideLanguage,
    /// Run a process.
    ///
    /// The heaviest grant available. Extensions that need it are held to the
    /// strictest notarisation review, and it is what a language-server extension
    /// needs in order to launch its server.
    SpawnProcess,
    /// Make network requests.
    Network,
    /// Read and write the extension's own private storage.
    Storage,
}

impl Capability {
    /// Every capability.
    pub const ALL: &'static [Capability] = &[
        Capability::ReadDocument,
        Capability::EditDocument,
        Capability::ReadWorkspace,
        Capability::WriteWorkspace,
        Capability::RegisterCommands,
        Capability::ContributeUi,
        Capability::ProvideLanguage,
        Capability::SpawnProcess,
        Capability::Network,
        Capability::Storage,
    ];

    /// The manifest name.
    pub const fn name(&self) -> &'static str {
        match self {
            Capability::ReadDocument => "read-document",
            Capability::EditDocument => "edit-document",
            Capability::ReadWorkspace => "read-workspace",
            Capability::WriteWorkspace => "write-workspace",
            Capability::RegisterCommands => "register-commands",
            Capability::ContributeUi => "contribute-ui",
            Capability::ProvideLanguage => "provide-language",
            Capability::SpawnProcess => "spawn-process",
            Capability::Network => "network",
            Capability::Storage => "storage",
        }
    }

    /// The WIT interface this capability corresponds to.
    ///
    /// Used by the notarisation scanner: it reads the component's imports and
    /// maps each back to the capability it implies.
    pub const fn wit_interface(&self) -> &'static str {
        match self {
            Capability::ReadDocument => "nebula:ide/document@0.1.0",
            Capability::EditDocument => "nebula:ide/editor@0.1.0",
            Capability::ReadWorkspace => "nebula:ide/workspace-read@0.1.0",
            Capability::WriteWorkspace => "nebula:ide/workspace-write@0.1.0",
            Capability::RegisterCommands => "nebula:ide/commands@0.1.0",
            Capability::ContributeUi => "nebula:ide/ui@0.1.0",
            Capability::ProvideLanguage => "nebula:ide/language@0.1.0",
            Capability::SpawnProcess => "nebula:ide/process@0.1.0",
            Capability::Network => "nebula:ide/http@0.1.0",
            Capability::Storage => "nebula:ide/storage@0.1.0",
        }
    }

    /// Look a capability up by manifest name.
    pub fn from_name(name: &str) -> Option<Capability> {
        Self::ALL.iter().copied().find(|c| c.name() == name)
    }

    /// Look a capability up by the WIT interface a component imports.
    pub fn from_interface(interface: &str) -> Option<Capability> {
        // Match with and without the version suffix, since a component may
        // import an unversioned name.
        let base = interface.split('@').next().unwrap_or(interface);
        Self::ALL.iter().copied().find(|c| {
            let ours = c.wit_interface();
            ours == interface || ours.split('@').next() == Some(base)
        })
    }

    /// Whether this capability requires explicit user consent at install time.
    ///
    /// Anything that can reach outside the editor does. Registering a command or
    /// adding a status item cannot harm the user, and prompting for those would
    /// train them to click through the prompts that matter.
    pub const fn requires_consent(&self) -> bool {
        matches!(
            self,
            Capability::WriteWorkspace
                | Capability::SpawnProcess
                | Capability::Network
                | Capability::ReadWorkspace
        )
    }

    /// What the install prompt tells the user.
    pub const fn describe(&self) -> &'static str {
        match self {
            Capability::ReadDocument => "read the file you are editing",
            Capability::EditDocument => "propose edits to your files",
            Capability::ReadWorkspace => "read any file in your project",
            Capability::WriteWorkspace => "create and modify files in your project",
            Capability::RegisterCommands => "add commands to the palette",
            Capability::ContributeUi => "add items to the interface",
            Capability::ProvideLanguage => "provide language support",
            Capability::SpawnProcess => "run programs on your computer",
            Capability::Network => "make network requests",
            Capability::Storage => "store its own settings",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A version of the host world.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorldVersion(pub Version);

impl WorldVersion {
    /// Parse a version string.
    pub fn parse(value: &str) -> Option<WorldVersion> {
        Version::parse(value).ok().map(WorldVersion)
    }

    /// Whether a component targeting `self` can run against `host`.
    ///
    /// Standard semver compatibility, with the pre-1.0 rule that the minor
    /// version is the breaking one. A component may run against a host with a
    /// higher patch or minor version, never a higher major.
    pub fn is_compatible_with(&self, host: &WorldVersion) -> bool {
        if self.0.major != host.0.major {
            return false;
        }
        if self.0.major == 0 {
            // Pre-1.0: 0.x is the breaking version.
            return self.0.minor == host.0.minor && self.0.patch <= host.0.patch;
        }
        // The host must be at least as new as what the component expects.
        (self.0.minor, self.0.patch) <= (host.0.minor, host.0.patch)
    }
}

impl std::fmt::Display for WorldVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The set of world versions this host implements.
#[derive(Debug, Clone)]
pub struct WitWorld {
    /// Versions served concurrently, oldest first.
    versions: Vec<WorldVersion>,
}

impl Default for WitWorld {
    fn default() -> Self {
        Self::current()
    }
}

impl WitWorld {
    /// The versions this build serves.
    ///
    /// More than one deliberately: an extension built against an older world
    /// keeps working, which is the whole point of the deprecation window.
    pub fn current() -> Self {
        Self { versions: vec![WorldVersion(Version::new(0, 1, 0))] }
    }

    /// A world serving an explicit set of versions.
    pub fn with_versions(versions: Vec<WorldVersion>) -> Self {
        let mut versions = versions;
        versions.sort();
        Self { versions }
    }

    /// The newest version served.
    pub fn latest(&self) -> &WorldVersion {
        self.versions.last().expect("a world always serves at least one version")
    }

    /// Every version served.
    pub fn versions(&self) -> &[WorldVersion] {
        &self.versions
    }

    /// Whether a component targeting `wanted` can run here.
    pub fn supports(&self, wanted: &WorldVersion) -> bool {
        self.versions.iter().any(|host| wanted.is_compatible_with(host))
    }

    /// The WIT source for the world, as published to extension authors.
    ///
    /// Kept here rather than in a separate file so it cannot drift from the
    /// [`Capability`] enum: a test asserts every capability's interface appears.
    pub fn wit_source() -> &'static str {
        WORLD_WIT
    }
}

/// The published WIT world.
const WORLD_WIT: &str = r#"package nebula:ide@0.1.0;

/// Text and position primitives shared by the interfaces below.
interface types {
    /// A zero-based character offset into a document.
    type offset = u32;

    /// A half-open range of character offsets.
    record range {
        start: offset,
        end: offset,
    }

    /// A document the host has open.
    record document-info {
        id: u64,
        path: option<string>,
        language: option<string>,
        version: u64,
        length: offset,
    }

    /// A single replacement.
    record text-edit {
        range: range,
        text: string,
    }

    /// Why an operation failed.
    variant error {
        not-found(string),
        not-permitted(string),
        invalid-argument(string),
        internal(string),
    }
}

/// Read the document the user is editing.
interface document {
    use types.{offset, range, document-info, error};

    /// The document currently focused, if any.
    active: func() -> option<document-info>;

    /// The text covered by `range`.
    text: func(id: u64, r: range) -> result<string, error>;

    /// The whole document's text.
    full-text: func(id: u64) -> result<string, error>;

    /// The cursor positions, as character offsets.
    selections: func(id: u64) -> result<list<range>, error>;
}

/// Propose edits. The host applies them, so they join the user's undo history
/// and can be undone in one step like any other edit.
interface editor {
    use types.{text-edit, error};

    /// Apply a set of edits atomically.
    apply-edits: func(id: u64, edits: list<text-edit>) -> result<_, error>;

    /// Move the cursor.
    set-cursor: func(id: u64, position: u32) -> result<_, error>;
}

/// Read files in the project.
interface workspace-read {
    use types.{error};

    /// The project root, as an absolute path.
    root: func() -> option<string>;

    /// Read a project-relative file.
    read-file: func(path: string) -> result<list<u8>, error>;

    /// List project-relative paths matching a glob.
    list-files: func(glob: string) -> result<list<string>, error>;
}

/// Write files in the project.
interface workspace-write {
    use types.{error};

    /// Write a project-relative file.
    write-file: func(path: string, contents: list<u8>) -> result<_, error>;

    /// Create a directory.
    create-directory: func(path: string) -> result<_, error>;
}

/// Register commands in the palette.
interface commands {
    use types.{error};

    /// Register a command. The host calls `handle-command` when it is invoked.
    register: func(id: string, title: string) -> result<_, error>;
}

/// Contribute to the interface.
interface ui {
    use types.{error};

    /// Set the extension's status-bar text.
    set-status: func(text: string) -> result<_, error>;

    /// Show a notification.
    notify: func(level: string, message: string) -> result<_, error>;
}

/// Provide language support.
interface language {
    use types.{error};

    /// Register a language server this extension can start.
    register-server: func(language-id: string, command: string, args: list<string>)
        -> result<_, error>;
}

/// Run a process. Subject to the host's OS sandbox.
interface process {
    use types.{error};

    /// The result of running a program.
    record output {
        exit-code: s32,
        stdout: string,
        stderr: string,
    }

    /// Run a program to completion, under the host's sandbox policy.
    run: func(program: string, args: list<string>) -> result<output, error>;
}

/// Make network requests.
interface http {
    use types.{error};

    /// An HTTP response.
    record response {
        status: u16,
        body: list<u8>,
    }

    /// Send a request.
    request: func(method: string, url: string, body: option<list<u8>>)
        -> result<response, error>;
}

/// Private key/value storage scoped to this extension.
interface storage {
    use types.{error};

    get: func(key: string) -> result<option<list<u8>>, error>;
    set: func(key: string, value: list<u8>) -> result<_, error>;
    delete: func(key: string) -> result<_, error>;
}

/// What every extension must export.
world extension {
    import types;
    import document;
    import editor;
    import workspace-read;
    import workspace-write;
    import commands;
    import ui;
    import language;
    import process;
    import http;
    import storage;

    /// Called once after the extension is loaded.
    export activate: func() -> result<_, string>;

    /// Called before the extension is unloaded.
    export deactivate: func();

    /// Called when a command this extension registered is invoked.
    export handle-command: func(id: string, arguments: string) -> result<string, string>;
}
"#;

/// The capabilities a set of WIT imports implies.
///
/// This is what the notarisation pipeline runs: it reads a component's actual
/// imports and derives the capabilities, then checks them against the manifest.
/// A component that imports more than it declared is rejected.
pub fn capabilities_from_imports(imports: &[String]) -> BTreeSet<Capability> {
    imports.iter().filter_map(|import| Capability::from_interface(import)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_round_trip_through_their_names() {
        for capability in Capability::ALL {
            assert_eq!(Capability::from_name(capability.name()), Some(*capability));
        }
        assert_eq!(Capability::from_name("read-your-email"), None);
    }

    #[test]
    fn every_capability_maps_to_a_distinct_wit_interface() {
        let mut interfaces: Vec<&str> = Capability::ALL.iter().map(|c| c.wit_interface()).collect();
        let count = interfaces.len();
        interfaces.sort_unstable();
        interfaces.dedup();
        assert_eq!(interfaces.len(), count, "two capabilities share an interface");
    }

    #[test]
    fn every_capability_appears_in_the_published_world() {
        // The world text and the enum must not drift: a capability with no
        // interface is unreachable, and an interface with no capability is
        // ungoverned.
        let wit = WitWorld::wit_source();
        for capability in Capability::ALL {
            let interface = capability.wit_interface();
            let name = interface
                .split('/')
                .nth(1)
                .and_then(|s| s.split('@').next())
                .expect("interface names are package/name@version");
            assert!(
                wit.contains(&format!("interface {name}")),
                "`{name}` is missing from the published world"
            );
            assert!(
                wit.contains(&format!("import {name}")),
                "`{name}` is not imported by the world"
            );
        }
    }

    #[test]
    fn imports_map_back_to_capabilities() {
        let imports = vec![
            "nebula:ide/document@0.1.0".to_string(),
            "nebula:ide/workspace-read@0.1.0".to_string(),
        ];
        let capabilities = capabilities_from_imports(&imports);

        assert!(capabilities.contains(&Capability::ReadDocument));
        assert!(capabilities.contains(&Capability::ReadWorkspace));
        assert!(!capabilities.contains(&Capability::Network));
    }

    #[test]
    fn an_unversioned_import_still_maps() {
        assert_eq!(Capability::from_interface("nebula:ide/http"), Some(Capability::Network));
    }

    #[test]
    fn unrecognised_imports_map_to_nothing() {
        // WASI and other host-provided interfaces are not Nebula capabilities.
        let imports =
            vec!["wasi:cli/environment@0.2.0".to_string(), "wasi:io/streams@0.2.0".to_string()];
        assert!(capabilities_from_imports(&imports).is_empty());
    }

    #[test]
    fn only_capabilities_that_reach_outside_the_editor_need_consent() {
        assert!(Capability::Network.requires_consent());
        assert!(Capability::SpawnProcess.requires_consent());
        assert!(Capability::WriteWorkspace.requires_consent());

        // Prompting for these would train users to click through the prompts
        // that matter.
        assert!(!Capability::RegisterCommands.requires_consent());
        assert!(!Capability::ContributeUi.requires_consent());
    }

    #[test]
    fn every_capability_has_a_user_facing_description() {
        for capability in Capability::ALL {
            let description = capability.describe();
            assert!(!description.is_empty());
            assert!(
                !description.contains('-'),
                "`{}` reads like an identifier, not a sentence: {description}",
                capability.name()
            );
        }
    }

    #[test]
    fn pre_one_point_zero_minor_versions_are_breaking() {
        let host = WorldVersion::parse("0.2.0").unwrap();

        assert!(WorldVersion::parse("0.2.0").unwrap().is_compatible_with(&host));
        assert!(
            !WorldVersion::parse("0.1.0").unwrap().is_compatible_with(&host),
            "0.1 and 0.2 are incompatible before 1.0"
        );
        assert!(!WorldVersion::parse("0.3.0").unwrap().is_compatible_with(&host));
    }

    #[test]
    fn a_patch_older_component_runs_against_a_newer_host() {
        let host = WorldVersion::parse("0.1.5").unwrap();
        assert!(WorldVersion::parse("0.1.0").unwrap().is_compatible_with(&host));
        assert!(
            !WorldVersion::parse("0.1.9").unwrap().is_compatible_with(&host),
            "a component needing a newer patch must not silently run"
        );
    }

    #[test]
    fn major_versions_never_cross() {
        let host = WorldVersion::parse("1.0.0").unwrap();
        assert!(!WorldVersion::parse("0.9.0").unwrap().is_compatible_with(&host));
        assert!(!WorldVersion::parse("2.0.0").unwrap().is_compatible_with(&host));
    }

    #[test]
    fn a_host_can_serve_several_world_versions_at_once() {
        // This is what keeps an extension working across a host update.
        let world = WitWorld::with_versions(vec![
            WorldVersion::parse("0.1.0").unwrap(),
            WorldVersion::parse("0.2.0").unwrap(),
        ]);

        assert!(world.supports(&WorldVersion::parse("0.1.0").unwrap()));
        assert!(world.supports(&WorldVersion::parse("0.2.0").unwrap()));
        assert!(!world.supports(&WorldVersion::parse("0.3.0").unwrap()));
        assert_eq!(world.latest().to_string(), "0.2.0");
    }

    #[test]
    fn the_published_world_declares_the_required_exports() {
        let wit = WitWorld::wit_source();
        for export in ["export activate", "export deactivate", "export handle-command"] {
            assert!(wit.contains(export), "the world must declare `{export}`");
        }
    }
}
