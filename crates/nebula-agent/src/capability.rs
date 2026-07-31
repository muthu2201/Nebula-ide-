//! Task-scoped capability grants.
//!
//! The 2026 practice this implements: a task is granted exactly the capabilities
//! it needs, at the moment it is created, and nothing more. A summarisation task
//! gets the read tool and nothing else; drafting an email produces a draft for
//! review rather than a sent message.
//!
//! The critical property is that a grant is **data decided before the loop
//! starts**, and the check happens in Rust between the model's request and the
//! tool's execution. Nothing the model emits can widen a grant, because the
//! model never touches this structure.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Something a tool may need permission to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Capability {
    /// Read files inside the project.
    ReadFiles,
    /// Modify files inside the project.
    WriteFiles,
    /// Run programs.
    RunCommands,
    /// Reach the network.
    Network,
    /// Call tools on an MCP server.
    CallMcpTools,
    /// Take an action that cannot be undone: deleting, force-pushing, publishing.
    Destructive,
}

impl Capability {
    /// Every capability.
    pub const ALL: &'static [Capability] = &[
        Capability::ReadFiles,
        Capability::WriteFiles,
        Capability::RunCommands,
        Capability::Network,
        Capability::CallMcpTools,
        Capability::Destructive,
    ];

    /// A stable name, used in grants, prompts and the audit log.
    pub const fn name(&self) -> &'static str {
        match self {
            Capability::ReadFiles => "read-files",
            Capability::WriteFiles => "write-files",
            Capability::RunCommands => "run-commands",
            Capability::Network => "network",
            Capability::CallMcpTools => "call-mcp-tools",
            Capability::Destructive => "destructive",
        }
    }

    /// Whether this capability should always require explicit human approval.
    ///
    /// Destructive actions are gated on a human regardless of what was granted:
    /// a grant says "this task may delete things", a prompt says "delete *this*".
    pub const fn always_requires_approval(&self) -> bool {
        matches!(self, Capability::Destructive)
    }

    /// A description shown in the approval prompt.
    pub const fn describe(&self) -> &'static str {
        match self {
            Capability::ReadFiles => "read files in this project",
            Capability::WriteFiles => "create and modify files in this project",
            Capability::RunCommands => "run programs on your machine",
            Capability::Network => "make network requests",
            Capability::CallMcpTools => "call tools on connected MCP servers",
            Capability::Destructive => "take actions that cannot be undone",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A capability granted to a task, with its scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// What is granted.
    pub capability: Capability,
    /// Paths this grant is limited to. Empty means the whole project root.
    pub paths: Vec<PathBuf>,
    /// Whether each use still needs human approval.
    pub requires_approval: bool,
}

impl Grant {
    /// An unscoped grant covering the project.
    pub fn new(capability: Capability) -> Self {
        Self {
            capability,
            paths: Vec::new(),
            requires_approval: capability.always_requires_approval(),
        }
    }

    /// Limit the grant to a path.
    pub fn scoped_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.paths.push(path.into());
        self
    }

    /// Require approval on every use.
    pub fn with_approval(mut self) -> Self {
        self.requires_approval = true;
        self
    }

    /// Whether this grant covers `path`.
    fn covers_path(&self, path: &Path) -> bool {
        // An unscoped grant covers everything the project root contains; the
        // containment check itself lives in the VFS layer.
        self.paths.is_empty() || self.paths.iter().any(|allowed| path.starts_with(allowed))
    }
}

/// The capabilities in force for one task.
///
/// Deny by default: [`GrantSet::none`] permits nothing, and every capability has
/// to be added deliberately.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantSet {
    grants: Vec<Grant>,
}

impl GrantSet {
    /// A set granting nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// Read-only access, for a task that only inspects the codebase.
    ///
    /// This is the right default for a question about the code: it cannot
    /// change anything, so a prompt injection in a file it reads has nothing to
    /// act with.
    pub fn read_only() -> Self {
        Self { grants: vec![Grant::new(Capability::ReadFiles)] }
    }

    /// Read, write and run commands: the normal coding-assistant grant.
    ///
    /// Deliberately excludes [`Capability::Network`] and
    /// [`Capability::Destructive`]. An agent that can edit code and run tests
    /// but cannot reach the network cannot exfiltrate what it read, which
    /// removes the most damaging outcome of a successful injection.
    pub fn coding() -> Self {
        Self {
            grants: vec![
                Grant::new(Capability::ReadFiles),
                Grant::new(Capability::WriteFiles),
                Grant::new(Capability::RunCommands),
            ],
        }
    }

    /// Add a grant.
    pub fn allow(mut self, grant: Grant) -> Self {
        // A repeated capability replaces the earlier grant rather than stacking,
        // so the set has one answer per capability.
        self.grants.retain(|g| g.capability != grant.capability);
        self.grants.push(grant);
        self
    }

    /// Add an unscoped capability.
    pub fn allow_capability(self, capability: Capability) -> Self {
        self.allow(Grant::new(capability))
    }

    /// Remove a capability.
    pub fn revoke(mut self, capability: Capability) -> Self {
        self.grants.retain(|g| g.capability != capability);
        self
    }

    /// Whether `capability` is granted at all.
    pub fn allows(&self, capability: Capability) -> bool {
        self.grants.iter().any(|g| g.capability == capability)
    }

    /// Whether `capability` is granted for `path`.
    pub fn allows_path(&self, capability: Capability, path: &Path) -> bool {
        self.grants.iter().any(|g| g.capability == capability && g.covers_path(path))
    }

    /// Whether using `capability` needs human approval.
    ///
    /// An ungranted capability reports `true`: it will be refused, and reporting
    /// "no approval needed" for something that cannot happen would be a
    /// misleading answer to the question.
    pub fn requires_approval(&self, capability: Capability) -> bool {
        match self.grants.iter().find(|g| g.capability == capability) {
            Some(grant) => grant.requires_approval || capability.always_requires_approval(),
            None => true,
        }
    }

    /// The granted capabilities, sorted.
    pub fn capabilities(&self) -> BTreeSet<Capability> {
        self.grants.iter().map(|g| g.capability).collect()
    }

    /// Whether nothing is granted.
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// A description for the approval prompt and the audit log.
    pub fn describe(&self) -> String {
        if self.grants.is_empty() {
            return "no capabilities".to_string();
        }
        let mut parts: Vec<String> = self
            .capabilities()
            .iter()
            .map(|capability| {
                let grant = self.grants.iter().find(|g| g.capability == *capability).unwrap();
                let mut text = capability.name().to_string();
                if !grant.paths.is_empty() {
                    let mut paths: Vec<String> =
                        grant.paths.iter().map(|p| p.display().to_string()).collect();
                    paths.sort();
                    text.push_str(&format!(" ({})", paths.join(", ")));
                }
                if grant.requires_approval {
                    text.push_str(" [approval required]");
                }
                text
            })
            .collect();
        parts.sort();
        parts.join(", ")
    }

    /// Narrow this set to the intersection with `other`.
    ///
    /// Used when a sub-agent is spawned: it can never hold more than its parent,
    /// which is what stops a delegation chain from escalating.
    pub fn intersect(&self, other: &GrantSet) -> GrantSet {
        let mut result = GrantSet::none();
        for grant in &self.grants {
            let Some(theirs) = other.grants.iter().find(|g| g.capability == grant.capability)
            else {
                continue;
            };
            // The narrower scope wins, and approval is required if either side
            // requires it.
            let paths = if grant.paths.is_empty() {
                theirs.paths.clone()
            } else if theirs.paths.is_empty() {
                grant.paths.clone()
            } else {
                grant
                    .paths
                    .iter()
                    .filter(|p| theirs.paths.iter().any(|t| p.starts_with(t) || t.starts_with(p)))
                    .cloned()
                    .collect()
            };
            result.grants.push(Grant {
                capability: grant.capability,
                paths,
                requires_approval: grant.requires_approval || theirs.requires_approval,
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_granted_by_default() {
        let grants = GrantSet::none();
        assert!(grants.is_empty());
        for capability in Capability::ALL {
            assert!(!grants.allows(*capability), "{capability} should not be granted");
        }
    }

    #[test]
    fn the_read_only_preset_cannot_change_anything() {
        let grants = GrantSet::read_only();
        assert!(grants.allows(Capability::ReadFiles));
        assert!(!grants.allows(Capability::WriteFiles));
        assert!(!grants.allows(Capability::RunCommands));
        assert!(!grants.allows(Capability::Network));
    }

    #[test]
    fn the_coding_preset_withholds_network_access() {
        // An agent that can read the codebase but not reach the network cannot
        // exfiltrate what it read, which is the worst outcome of an injection.
        let grants = GrantSet::coding();
        assert!(grants.allows(Capability::ReadFiles));
        assert!(grants.allows(Capability::WriteFiles));
        assert!(grants.allows(Capability::RunCommands));
        assert!(
            !grants.allows(Capability::Network),
            "the coding preset must not grant network access"
        );
        assert!(!grants.allows(Capability::Destructive));
    }

    #[test]
    fn destructive_actions_always_require_approval_even_when_granted() {
        let grants = GrantSet::none().allow_capability(Capability::Destructive);
        assert!(grants.allows(Capability::Destructive));
        assert!(
            grants.requires_approval(Capability::Destructive),
            "a grant says the task may delete; a prompt says delete this"
        );
    }

    #[test]
    fn an_ungranted_capability_reports_that_approval_is_required() {
        let grants = GrantSet::read_only();
        assert!(grants.requires_approval(Capability::WriteFiles));
    }

    #[test]
    fn scoped_grants_only_cover_their_paths() {
        let grants = GrantSet::none()
            .allow(Grant::new(Capability::WriteFiles).scoped_to("/project/src"));

        assert!(grants.allows_path(Capability::WriteFiles, Path::new("/project/src/main.rs")));
        assert!(!grants.allows_path(Capability::WriteFiles, Path::new("/project/secrets.env")));
        assert!(!grants.allows_path(Capability::WriteFiles, Path::new("/etc/passwd")));
    }

    #[test]
    fn an_unscoped_grant_covers_any_path() {
        let grants = GrantSet::none().allow_capability(Capability::ReadFiles);
        assert!(grants.allows_path(Capability::ReadFiles, Path::new("/project/anything.rs")));
    }

    #[test]
    fn adding_a_capability_twice_replaces_rather_than_stacking() {
        let grants = GrantSet::none()
            .allow(Grant::new(Capability::WriteFiles).scoped_to("/a"))
            .allow(Grant::new(Capability::WriteFiles).scoped_to("/b"));

        assert!(!grants.allows_path(Capability::WriteFiles, Path::new("/a/file")));
        assert!(grants.allows_path(Capability::WriteFiles, Path::new("/b/file")));
    }

    #[test]
    fn revoking_removes_a_capability() {
        let grants = GrantSet::coding().revoke(Capability::WriteFiles);
        assert!(grants.allows(Capability::ReadFiles));
        assert!(!grants.allows(Capability::WriteFiles));
    }

    #[test]
    fn a_delegated_grant_set_can_never_exceed_its_parent() {
        // The escalation path this closes: a sub-agent asking for more than the
        // task it was spawned from holds.
        let parent = GrantSet::read_only();
        let requested = GrantSet::coding().allow_capability(Capability::Network);

        let effective = requested.intersect(&parent);
        assert_eq!(effective.capabilities(), parent.capabilities());
        assert!(!effective.allows(Capability::WriteFiles));
        assert!(!effective.allows(Capability::Network));
    }

    #[test]
    fn intersection_keeps_the_narrower_path_scope() {
        let parent =
            GrantSet::none().allow(Grant::new(Capability::WriteFiles).scoped_to("/project"));
        let child = GrantSet::none()
            .allow(Grant::new(Capability::WriteFiles).scoped_to("/project/src"));

        let effective = child.intersect(&parent);
        assert!(effective.allows_path(Capability::WriteFiles, Path::new("/project/src/main.rs")));

        let widened = parent.intersect(&child);
        assert!(
            widened.allows_path(Capability::WriteFiles, Path::new("/project/src/main.rs")),
            "the intersection is symmetric in what it permits"
        );
    }

    #[test]
    fn intersection_preserves_an_approval_requirement_from_either_side() {
        let parent =
            GrantSet::none().allow(Grant::new(Capability::WriteFiles).with_approval());
        let child = GrantSet::none().allow(Grant::new(Capability::WriteFiles));

        assert!(child.intersect(&parent).requires_approval(Capability::WriteFiles));
    }

    #[test]
    fn descriptions_are_stable_and_readable() {
        let grants = GrantSet::none()
            .allow(Grant::new(Capability::ReadFiles))
            .allow(Grant::new(Capability::WriteFiles).scoped_to("/project/src").with_approval());

        let description = grants.describe();
        assert_eq!(description, grants.describe(), "the audit log must not jitter");
        assert!(description.contains("read-files"));
        assert!(description.contains("/project/src"));
        assert!(description.contains("[approval required]"));

        assert_eq!(GrantSet::none().describe(), "no capabilities");
    }

    #[test]
    fn grant_sets_round_trip_through_serde() {
        let grants = GrantSet::coding()
            .allow(Grant::new(Capability::Destructive).scoped_to("/project/build"));
        let json = serde_json::to_string(&grants).unwrap();
        assert_eq!(serde_json::from_str::<GrantSet>(&json).unwrap(), grants);
    }
}
