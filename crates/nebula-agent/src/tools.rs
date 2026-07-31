//! The tool registry, and the enforcement point.
//!
//! [`ToolRegistry::invoke`] is the single choke point every tool call passes
//! through. The order of operations there is the security design:
//!
//! 1. resolve the tool (an unknown name is refused, not guessed at);
//! 2. check the capability **in Rust**, against a grant set the model never
//!    touched;
//! 3. ask the human if approval is required;
//! 4. record the attempt in the audit log;
//! 5. only then run the tool, under the OS sandbox.
//!
//! Steps 2 and 5 enforce the same boundary twice, deliberately. A bug in this
//! layer should not become an escape, and a kernel without Landlock should not
//! become an unrestricted agent.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::audit::{AuditLog, AuditOutcome};
use crate::capability::{Capability, GrantSet};
use crate::injection::InjectionScanner;
use crate::{AgentError, Result};

/// What a tool produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcome {
    /// Text returned to the model.
    pub content: String,
    /// Whether the tool failed. The call still succeeded; the tool is reporting
    /// that what it was asked to do did not work.
    pub is_error: bool,
    /// Anything the scanner flagged in content this tool read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injection_findings: Vec<crate::injection::InjectionFinding>,
}

impl ToolOutcome {
    /// A successful result.
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, injection_findings: Vec::new() }
    }

    /// A failure the model should see and react to.
    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, injection_findings: Vec::new() }
    }

    /// Attach injection findings.
    pub fn with_findings(mut self, findings: Vec<crate::injection::InjectionFinding>) -> Self {
        self.injection_findings = findings;
        self
    }
}

/// Everything a tool needs to do its job.
pub struct ToolContext {
    /// The project root. Every path is resolved against it and confined to it.
    pub project_root: PathBuf,
    /// The capabilities in force.
    pub grants: GrantSet,
    /// The task this call belongs to.
    pub task_id: String,
    /// Scanner applied to any untrusted content the tool reads.
    pub scanner: InjectionScanner,
}

/// Something the model can call.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The name the model uses.
    fn name(&self) -> &str;

    /// What it does. This text is part of the prompt.
    fn description(&self) -> &str;

    /// JSON Schema for the arguments.
    fn input_schema(&self) -> serde_json::Value;

    /// The capability this tool requires.
    fn capability(&self) -> Capability;

    /// Whether this particular call is destructive.
    ///
    /// Takes the arguments, because destructiveness is usually a property of the
    /// call rather than the tool: `run_command` is fine for `cargo test` and not
    /// for `rm -rf`.
    fn is_destructive(&self, _arguments: &serde_json::Value) -> bool {
        false
    }

    /// A one-line summary for the approval prompt and the audit log.
    fn summarize(&self, arguments: &serde_json::Value) -> String {
        format!("{}({arguments})", self.name())
    }

    /// Run the tool.
    async fn execute(
        &self,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutcome>;
}

/// Decides whether an action the user must approve may proceed.
pub trait ApprovalPolicy: Send + Sync {
    /// Approve or decline `summary`, which describes what is about to happen.
    fn approve(&self, tool: &str, summary: &str) -> bool;
}

/// Declines everything that needs approval.
///
/// The correct default for a headless run: nothing that needs a human should
/// proceed when there is no human.
pub struct DenyAll;

impl ApprovalPolicy for DenyAll {
    fn approve(&self, _tool: &str, _summary: &str) -> bool {
        false
    }
}

/// Approves everything.
///
/// For tests and for an explicitly unattended run the user has opted into. It is
/// named to be obvious in a code review.
pub struct ApproveAll;

impl ApprovalPolicy for ApproveAll {
    fn approve(&self, _tool: &str, _summary: &str) -> bool {
        true
    }
}

/// The tools available to an agent, and the enforcement point for calling them.
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    audit: Arc<AuditLog>,
    approval: Arc<dyn ApprovalPolicy>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new(audit: Arc<AuditLog>) -> Self {
        Self { tools: BTreeMap::new(), audit, approval: Arc::new(DenyAll) }
    }

    /// Set the approval policy.
    pub fn with_approval(mut self, approval: Arc<dyn ApprovalPolicy>) -> Self {
        self.approval = approval;
        self
    }

    /// Register a tool.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// The registered tool names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The tool definitions to advertise to the model.
    ///
    /// Only tools whose capability is granted are advertised. A model that is
    /// never told about a tool does not try to call it, which turns a refusal
    /// into a non-event rather than a wasted turn.
    pub fn definitions_for(&self, grants: &GrantSet) -> Vec<nebula_ai::provider::ToolDefinition> {
        self.tools
            .values()
            .filter(|tool| grants.allows(tool.capability()))
            .map(|tool| nebula_ai::provider::ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    /// Call a tool, enforcing capabilities and recording the attempt.
    pub async fn invoke(
        &self,
        name: &str,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutcome> {
        // 1. Resolve. An unknown tool is an error, never a guess.
        let Some(tool) = self.tools.get(name).cloned() else {
            self.audit.record(
                &context.task_id,
                name,
                serde_json::json!({ "reason": "unknown tool" }),
                AuditOutcome::Failed { error: "no such tool".to_string() },
            )?;
            return Err(AgentError::UnknownTool(name.to_string()));
        };

        let summary = tool.summarize(&arguments);
        let capability = tool.capability();

        // 2. Capability check, in Rust, against a grant set the model never saw.
        if !context.grants.allows(capability) {
            self.audit.record(
                &context.task_id,
                name,
                serde_json::json!({ "summary": summary }),
                AuditOutcome::Refused { capability: capability.name().to_string() },
            )?;
            return Err(AgentError::NotGranted {
                tool: name.to_string(),
                capability: capability.name().to_string(),
            });
        }

        // A destructive call needs the destructive capability as well as the
        // tool's own — `run_command` being granted does not imply `rm -rf` is.
        let destructive = tool.is_destructive(&arguments);
        if destructive && !context.grants.allows(Capability::Destructive) {
            self.audit.record(
                &context.task_id,
                name,
                serde_json::json!({ "summary": summary }),
                AuditOutcome::Refused {
                    capability: Capability::Destructive.name().to_string(),
                },
            )?;
            return Err(AgentError::NotGranted {
                tool: name.to_string(),
                capability: Capability::Destructive.name().to_string(),
            });
        }

        // 3. Human approval, where required.
        let needs_approval = destructive || context.grants.requires_approval(capability);
        if needs_approval && !self.approval.approve(name, &summary) {
            self.audit.record(
                &context.task_id,
                name,
                serde_json::json!({ "summary": summary }),
                AuditOutcome::Declined,
            )?;
            return Err(AgentError::Declined(summary));
        }

        // 4. Record the attempt *before* it happens, so a crash mid-action still
        // leaves evidence that it was started.
        self.audit.record(
            &context.task_id,
            name,
            serde_json::json!({ "summary": summary, "arguments": arguments }),
            AuditOutcome::Started,
        )?;

        // 5. Execute.
        let result = tool.execute(arguments, context).await;

        match &result {
            Ok(outcome) => {
                self.audit.complete(
                    &context.task_id,
                    name,
                    AuditOutcome::Succeeded {
                        summary: format!(
                            "{} bytes returned{}",
                            outcome.content.len(),
                            if outcome.is_error { " (tool reported an error)" } else { "" }
                        ),
                    },
                )?;
            }
            Err(error) => {
                self.audit.complete(
                    &context.task_id,
                    name,
                    AuditOutcome::Failed { error: error.to_string() },
                )?;
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// A tool that records whether it ran.
    struct Spy {
        name: &'static str,
        capability: Capability,
        destructive: bool,
        ran: Arc<parking_lot::Mutex<usize>>,
    }

    impl Spy {
        fn new(name: &'static str, capability: Capability) -> (Arc<Self>, Arc<parking_lot::Mutex<usize>>) {
            let ran = Arc::new(parking_lot::Mutex::new(0));
            let tool =
                Arc::new(Self { name, capability, destructive: false, ran: Arc::clone(&ran) });
            (tool, ran)
        }

        fn destructive(name: &'static str, capability: Capability) -> (Arc<Self>, Arc<parking_lot::Mutex<usize>>) {
            let ran = Arc::new(parking_lot::Mutex::new(0));
            let tool =
                Arc::new(Self { name, capability, destructive: true, ran: Arc::clone(&ran) });
            (tool, ran)
        }
    }

    #[async_trait]
    impl Tool for Spy {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "a tool used in tests"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }
        fn capability(&self) -> Capability {
            self.capability
        }
        fn is_destructive(&self, _arguments: &serde_json::Value) -> bool {
            self.destructive
        }
        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _context: &ToolContext,
        ) -> Result<ToolOutcome> {
            *self.ran.lock() += 1;
            Ok(ToolOutcome::ok("did the thing"))
        }
    }

    fn context(grants: GrantSet) -> ToolContext {
        ToolContext {
            project_root: PathBuf::from("/project"),
            grants,
            task_id: "task-1".to_string(),
            scanner: InjectionScanner::new(),
        }
    }

    fn registry(audit: Arc<AuditLog>) -> ToolRegistry {
        ToolRegistry::new(audit).with_approval(Arc::new(ApproveAll))
    }

    #[tokio::test]
    async fn a_granted_tool_runs() {
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = registry(Arc::clone(&audit));
        let (tool, ran) = Spy::new("read_file", Capability::ReadFiles);
        registry.register(tool);

        let outcome = registry
            .invoke("read_file", json!({ "path": "a.rs" }), &context(GrantSet::read_only()))
            .await
            .unwrap();

        assert_eq!(outcome.content, "did the thing");
        assert_eq!(*ran.lock(), 1);
    }

    #[tokio::test]
    async fn an_ungranted_tool_is_refused_before_it_runs() {
        // The property the whole design rests on: the model asking is not
        // enough.
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = registry(Arc::clone(&audit));
        let (tool, ran) = Spy::new("write_file", Capability::WriteFiles);
        registry.register(tool);

        let err = registry
            .invoke("write_file", json!({}), &context(GrantSet::read_only()))
            .await
            .unwrap_err();

        assert!(matches!(err, AgentError::NotGranted { .. }), "{err:?}");
        assert_eq!(*ran.lock(), 0, "the tool must not have executed");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_refused_rather_than_guessed_at() {
        let audit = Arc::new(AuditLog::in_memory());
        let registry = registry(Arc::clone(&audit));

        let err = registry
            .invoke("read_fil", json!({}), &context(GrantSet::coding()))
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::UnknownTool(_)));
    }

    #[tokio::test]
    async fn a_destructive_call_needs_the_destructive_capability_too() {
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = registry(Arc::clone(&audit));
        let (tool, ran) = Spy::destructive("delete_all", Capability::WriteFiles);
        registry.register(tool);

        // WriteFiles alone is not enough.
        let err = registry
            .invoke("delete_all", json!({}), &context(GrantSet::coding()))
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::NotGranted { capability, .. } if capability == "destructive"));
        assert_eq!(*ran.lock(), 0);

        // With it granted, and approval given, it runs.
        let grants = GrantSet::coding().allow_capability(Capability::Destructive);
        registry.invoke("delete_all", json!({}), &context(grants)).await.unwrap();
        assert_eq!(*ran.lock(), 1);
    }

    #[tokio::test]
    async fn a_declined_approval_stops_the_call() {
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = ToolRegistry::new(Arc::clone(&audit)).with_approval(Arc::new(DenyAll));
        let (tool, ran) = Spy::destructive("delete_all", Capability::WriteFiles);
        registry.register(tool);

        let grants = GrantSet::coding().allow_capability(Capability::Destructive);
        let err = registry.invoke("delete_all", json!({}), &context(grants)).await.unwrap_err();

        assert!(matches!(err, AgentError::Declined(_)));
        assert_eq!(*ran.lock(), 0);
    }

    #[tokio::test]
    async fn a_headless_run_declines_by_default() {
        // No approval policy set: nothing needing a human may proceed.
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = ToolRegistry::new(Arc::clone(&audit));
        let (tool, ran) = Spy::destructive("publish", Capability::Network);
        registry.register(tool);

        let grants = GrantSet::none()
            .allow_capability(Capability::Network)
            .allow_capability(Capability::Destructive);
        assert!(registry.invoke("publish", json!({}), &context(grants)).await.is_err());
        assert_eq!(*ran.lock(), 0);
    }

    #[tokio::test]
    async fn only_granted_tools_are_advertised_to_the_model() {
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = registry(audit);
        registry.register(Spy::new("read_file", Capability::ReadFiles).0);
        registry.register(Spy::new("write_file", Capability::WriteFiles).0);
        registry.register(Spy::new("fetch_url", Capability::Network).0);

        let advertised: Vec<String> = registry
            .definitions_for(&GrantSet::read_only())
            .into_iter()
            .map(|d| d.name)
            .collect();

        assert_eq!(advertised, vec!["read_file"]);
        assert_eq!(registry.definitions_for(&GrantSet::coding()).len(), 2);
        assert!(registry.definitions_for(&GrantSet::none()).is_empty());
    }

    #[tokio::test]
    async fn every_outcome_reaches_the_audit_log() {
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = registry(Arc::clone(&audit));
        registry.register(Spy::new("read_file", Capability::ReadFiles).0);
        registry.register(Spy::new("write_file", Capability::WriteFiles).0);

        // One success, one refusal, one unknown tool.
        registry
            .invoke("read_file", json!({}), &context(GrantSet::read_only()))
            .await
            .unwrap();
        let _ = registry.invoke("write_file", json!({}), &context(GrantSet::read_only())).await;
        let _ = registry.invoke("nonexistent", json!({}), &context(GrantSet::read_only())).await;

        let entries = audit.entries();
        assert!(entries.iter().any(|e| matches!(e.outcome, AuditOutcome::Started)));
        assert!(entries.iter().any(|e| matches!(e.outcome, AuditOutcome::Succeeded { .. })));
        assert!(entries.iter().any(|e| matches!(e.outcome, AuditOutcome::Refused { .. })));
        assert!(
            audit.verify().is_ok(),
            "the log written by the registry must itself verify"
        );
    }

    #[tokio::test]
    async fn a_refusal_is_recorded_even_though_nothing_ran() {
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = registry(Arc::clone(&audit));
        registry.register(Spy::new("write_file", Capability::WriteFiles).0);

        let _ = registry.invoke("write_file", json!({}), &context(GrantSet::read_only())).await;

        let refusals: Vec<_> = audit
            .entries()
            .into_iter()
            .filter(|e| matches!(e.outcome, AuditOutcome::Refused { .. }))
            .collect();
        assert_eq!(refusals.len(), 1, "an attempt that was blocked is still worth recording");
        assert_eq!(refusals[0].action, "write_file");
    }

    #[tokio::test]
    async fn the_audit_log_survives_to_disk_across_a_session() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.jsonl");

        {
            let audit = Arc::new(AuditLog::at_path(&path).unwrap());
            let mut registry = registry(Arc::clone(&audit));
            registry.register(Spy::new("read_file", Capability::ReadFiles).0);
            registry
                .invoke("read_file", json!({ "path": "a.rs" }), &context(GrantSet::read_only()))
                .await
                .unwrap();
        }

        let reloaded = AuditLog::at_path(&path).unwrap();
        assert!(reloaded.len() >= 2, "a start and a completion entry");
        assert!(reloaded.verify().is_ok());
    }

    #[test]
    fn registry_names_are_sorted_for_stable_prompts() {
        let audit = Arc::new(AuditLog::in_memory());
        let mut registry = registry(audit);
        registry.register(Spy::new("zeta", Capability::ReadFiles).0);
        registry.register(Spy::new("alpha", Capability::ReadFiles).0);
        registry.register(Spy::new("mid", Capability::ReadFiles).0);

        assert_eq!(registry.names(), vec!["alpha", "mid", "zeta"]);
        assert_eq!(registry.len(), 3);
    }
}
