//! Tamper-evident audit logging.
//!
//! Every action the agent takes is recorded before it happens and completed
//! after. The log is a hash chain: each entry carries the hash of its
//! predecessor, so removing or altering an entry breaks every hash after it.
//!
//! That does not make the log tamper-*proof* — anyone who can write the file can
//! rewrite the whole chain. It makes it tamper-*evident*, which is the property
//! that is actually achievable for a local file, and it is enough to answer the
//! question that matters after an incident: "is this record intact?"

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{AgentError, Result};

/// How an action ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AuditOutcome {
    /// The action is about to be attempted.
    Started,
    /// It succeeded.
    Succeeded {
        /// A short summary of what happened.
        summary: String,
    },
    /// It failed.
    Failed {
        /// Why.
        error: String,
    },
    /// It was refused because the capability was not granted.
    Refused {
        /// Which capability was missing.
        capability: String,
    },
    /// The user declined it.
    Declined,
}

/// One record in the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Monotonic sequence number, starting at 1.
    pub sequence: u64,
    /// RFC 3339 timestamp.
    pub timestamp: String,
    /// The task this belongs to.
    pub task_id: String,
    /// What was attempted, e.g. the tool name.
    pub action: String,
    /// Arguments, with anything sensitive already redacted by the caller.
    pub detail: serde_json::Value,
    /// How it ended.
    pub outcome: AuditOutcome,
    /// Hex hash of the previous entry, chaining the log.
    pub previous_hash: String,
    /// Hex hash of this entry's content.
    pub hash: String,
}

impl AuditEntry {
    /// Compute the hash of an entry's content.
    ///
    /// The hash covers every field except `hash` itself, including
    /// `previous_hash` — which is what links the chain.
    fn compute_hash(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.sequence.to_le_bytes().as_slice());
        hasher.update(self.timestamp.as_bytes());
        hasher.update(self.task_id.as_bytes());
        hasher.update(self.action.as_bytes());
        // Serialising through serde_json gives a canonical form for the value.
        hasher.update(self.detail.to_string().as_bytes());
        hasher.update(serde_json::to_string(&self.outcome).unwrap_or_default().as_bytes());
        hasher.update(self.previous_hash.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Whether this entry's own hash matches its content.
    pub fn is_intact(&self) -> bool {
        self.hash == self.compute_hash()
    }
}

/// The hash recorded as the predecessor of the first entry.
const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// An append-only, hash-chained log.
#[derive(Debug)]
pub struct AuditLog {
    entries: parking_lot::RwLock<Vec<AuditEntry>>,
    /// Where entries are persisted, if anywhere.
    path: Option<PathBuf>,
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl AuditLog {
    /// A log that keeps entries in memory only.
    pub fn in_memory() -> Self {
        Self { entries: parking_lot::RwLock::new(Vec::new()), path: None }
    }

    /// A log persisted to `path` as newline-delimited JSON.
    ///
    /// An existing file is loaded and its chain verified, so a corrupted log is
    /// detected at startup rather than at incident-response time.
    pub fn at_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let entries = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| AgentError::Audit(format!("reading {}: {e}", path.display())))?;
            let mut entries = Vec::new();
            for (index, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let entry: AuditEntry = serde_json::from_str(line).map_err(|e| {
                    AgentError::Audit(format!(
                        "line {} of the audit log is corrupt: {e}",
                        index + 1
                    ))
                })?;
                entries.push(entry);
            }
            entries
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    AgentError::Audit(format!("creating {}: {e}", parent.display()))
                })?;
            }
            Vec::new()
        };

        let log = Self { entries: parking_lot::RwLock::new(entries), path: Some(path) };
        if let Err(broken) = log.verify() {
            tracing::error!(
                sequence = broken,
                "the audit log's hash chain is broken; earlier entries cannot be trusted"
            );
        }
        Ok(log)
    }

    /// Append an entry, chaining it to the previous one.
    ///
    /// Returns the sequence number, which the caller passes to
    /// [`AuditLog::complete`] once the action finishes.
    pub fn record(
        &self,
        task_id: &str,
        action: &str,
        detail: serde_json::Value,
        outcome: AuditOutcome,
    ) -> Result<u64> {
        let mut entries = self.entries.write();

        let previous_hash =
            entries.last().map(|e| e.hash.clone()).unwrap_or_else(|| GENESIS_HASH.to_string());
        let sequence = entries.len() as u64 + 1;

        let mut entry = AuditEntry {
            sequence,
            timestamp: now_rfc3339(),
            task_id: task_id.to_string(),
            action: action.to_string(),
            detail,
            outcome,
            previous_hash,
            hash: String::new(),
        };
        entry.hash = entry.compute_hash();

        // Persist before returning: an action whose record is not durable must
        // not be reported as recorded.
        if let Some(path) = &self.path {
            let line = serde_json::to_string(&entry)
                .map_err(|e| AgentError::Audit(format!("serialising entry: {e}")))?;
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| AgentError::Audit(format!("opening {}: {e}", path.display())))?;
            writeln!(file, "{line}")
                .map_err(|e| AgentError::Audit(format!("writing entry: {e}")))?;
            file.sync_data()
                .map_err(|e| AgentError::Audit(format!("syncing the audit log: {e}")))?;
        }

        entries.push(entry);
        Ok(sequence)
    }

    /// Record the completion of an action started earlier.
    ///
    /// This appends a new entry rather than mutating the old one: an append-only
    /// log whose entries change is not append-only, and mutating one would break
    /// every hash after it.
    pub fn complete(&self, task_id: &str, action: &str, outcome: AuditOutcome) -> Result<u64> {
        self.record(task_id, action, serde_json::json!({}), outcome)
    }

    /// Every entry.
    pub fn entries(&self) -> Vec<AuditEntry> {
        self.entries.read().clone()
    }

    /// Entries belonging to one task.
    pub fn entries_for(&self, task_id: &str) -> Vec<AuditEntry> {
        self.entries.read().iter().filter(|e| e.task_id == task_id).cloned().collect()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// Verify the whole chain.
    ///
    /// Returns `Err(sequence)` naming the first entry that does not verify.
    pub fn verify(&self) -> std::result::Result<(), u64> {
        let entries = self.entries.read();
        let mut expected_previous = GENESIS_HASH.to_string();

        for (index, entry) in entries.iter().enumerate() {
            if entry.sequence != index as u64 + 1 {
                return Err(entry.sequence);
            }
            if entry.previous_hash != expected_previous {
                return Err(entry.sequence);
            }
            if !entry.is_intact() {
                return Err(entry.sequence);
            }
            expected_previous = entry.hash.clone();
        }
        Ok(())
    }

    /// The hash of the most recent entry, which commits to the whole log.
    ///
    /// Publishing this somewhere the agent cannot write — a co-signed receipt, a
    /// remote log — is what turns tamper-evidence into something an attacker
    /// with local write access cannot defeat.
    pub fn head_hash(&self) -> String {
        self.entries
            .read()
            .last()
            .map(|e| e.hash.clone())
            .unwrap_or_else(|| GENESIS_HASH.to_string())
    }
}

/// The current time as an RFC 3339 string.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn a_new_log_is_empty_and_verifies() {
        let log = AuditLog::in_memory();
        assert!(log.is_empty());
        assert!(log.verify().is_ok());
        assert_eq!(log.head_hash(), GENESIS_HASH);
    }

    #[test]
    fn entries_are_numbered_from_one_and_chained() {
        let log = AuditLog::in_memory();
        log.record("task-1", "read_file", json!({ "path": "a.rs" }), AuditOutcome::Started)
            .unwrap();
        log.record(
            "task-1",
            "read_file",
            json!({}),
            AuditOutcome::Succeeded { summary: "read 42 lines".into() },
        )
        .unwrap();

        let entries = log.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sequence, 1);
        assert_eq!(entries[1].sequence, 2);
        assert_eq!(entries[0].previous_hash, GENESIS_HASH);
        assert_eq!(
            entries[1].previous_hash, entries[0].hash,
            "each entry must commit to its predecessor"
        );
        assert!(log.verify().is_ok());
    }

    #[test]
    fn altering_an_entry_breaks_the_chain() {
        let log = AuditLog::in_memory();
        for i in 0..5 {
            log.record("task-1", "run_command", json!({ "index": i }), AuditOutcome::Started)
                .unwrap();
        }
        assert!(log.verify().is_ok());

        // Rewrite history: change what the third action claimed to do.
        log.entries.write()[2].detail = json!({ "index": "tampered" });

        match log.verify() {
            Err(sequence) => assert_eq!(sequence, 3, "the altered entry must be named"),
            Ok(()) => panic!("tampering went undetected"),
        }
    }

    #[test]
    fn removing_an_entry_breaks_the_chain() {
        let log = AuditLog::in_memory();
        for i in 0..5 {
            log.record("task-1", "action", json!({ "i": i }), AuditOutcome::Started).unwrap();
        }
        log.entries.write().remove(2);

        assert!(log.verify().is_err(), "a deleted entry must be detectable");
    }

    #[test]
    fn reordering_entries_breaks_the_chain() {
        let log = AuditLog::in_memory();
        for i in 0..4 {
            log.record("task-1", "action", json!({ "i": i }), AuditOutcome::Started).unwrap();
        }
        log.entries.write().swap(1, 2);

        assert!(log.verify().is_err());
    }

    #[test]
    fn the_head_hash_changes_with_every_entry() {
        let log = AuditLog::in_memory();
        let genesis = log.head_hash();

        log.record("t", "a", json!({}), AuditOutcome::Started).unwrap();
        let after_one = log.head_hash();
        assert_ne!(genesis, after_one);

        log.record("t", "b", json!({}), AuditOutcome::Started).unwrap();
        assert_ne!(after_one, log.head_hash());
    }

    #[test]
    fn every_outcome_is_recorded_faithfully() {
        let log = AuditLog::in_memory();
        log.record("t", "write_file", json!({}), AuditOutcome::Started).unwrap();
        log.record(
            "t",
            "write_file",
            json!({}),
            AuditOutcome::Refused { capability: "write-files".into() },
        )
        .unwrap();
        log.record("t", "delete_file", json!({}), AuditOutcome::Declined).unwrap();
        log.record(
            "t",
            "run_command",
            json!({}),
            AuditOutcome::Failed { error: "exit status 1".into() },
        )
        .unwrap();

        let entries = log.entries();
        assert!(matches!(entries[1].outcome, AuditOutcome::Refused { .. }));
        assert_eq!(entries[2].outcome, AuditOutcome::Declined);
        assert!(matches!(entries[3].outcome, AuditOutcome::Failed { .. }));
        assert!(log.verify().is_ok());
    }

    #[test]
    fn entries_can_be_filtered_by_task() {
        let log = AuditLog::in_memory();
        log.record("task-a", "read", json!({}), AuditOutcome::Started).unwrap();
        log.record("task-b", "read", json!({}), AuditOutcome::Started).unwrap();
        log.record("task-a", "write", json!({}), AuditOutcome::Started).unwrap();

        assert_eq!(log.entries_for("task-a").len(), 2);
        assert_eq!(log.entries_for("task-b").len(), 1);
        assert!(log.entries_for("task-c").is_empty());
    }

    #[test]
    fn a_persisted_log_survives_a_restart_with_its_chain_intact() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.jsonl");

        {
            let log = AuditLog::at_path(&path).unwrap();
            for i in 0..10 {
                log.record("task-1", "action", json!({ "i": i }), AuditOutcome::Started).unwrap();
            }
            assert!(log.verify().is_ok());
        }

        let reopened = AuditLog::at_path(&path).unwrap();
        assert_eq!(reopened.len(), 10);
        assert!(reopened.verify().is_ok(), "the chain must survive a round trip through disk");

        // Appending after a reload continues the same chain.
        reopened.record("task-1", "more", json!({}), AuditOutcome::Started).unwrap();
        assert_eq!(reopened.len(), 11);
        assert!(reopened.verify().is_ok());
    }

    #[test]
    fn tampering_with_the_file_on_disk_is_detected_on_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.jsonl");

        {
            let log = AuditLog::at_path(&path).unwrap();
            for i in 0..3 {
                log.record("t", "action", json!({ "i": i }), AuditOutcome::Started).unwrap();
            }
        }

        // Edit the middle line the way someone covering their tracks would.
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        lines[1] = lines[1].replace("\"i\":1", "\"i\":99");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let reloaded = AuditLog::at_path(&path).unwrap();
        assert!(reloaded.verify().is_err(), "an edit to the log file must not verify");
    }

    #[test]
    fn a_corrupt_line_is_reported_rather_than_silently_skipped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.jsonl");
        std::fs::write(&path, "{not valid json at all\n").unwrap();

        let err = AuditLog::at_path(&path).unwrap_err();
        assert!(matches!(err, AgentError::Audit(_)));
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let log = AuditLog::in_memory();
        log.record("t", "a", json!({}), AuditOutcome::Started).unwrap();

        let timestamp = &log.entries()[0].timestamp;
        assert!(
            time::OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
                .is_ok(),
            "timestamp {timestamp} is not RFC 3339"
        );
    }

    #[test]
    fn concurrent_appends_produce_a_valid_chain() {
        use std::sync::Arc;

        let log = Arc::new(AuditLog::in_memory());
        let mut handles = Vec::new();
        for thread in 0..8 {
            let log = Arc::clone(&log);
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    log.record(
                        &format!("task-{thread}"),
                        "action",
                        json!({ "i": i }),
                        AuditOutcome::Started,
                    )
                    .unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(log.len(), 200);
        assert!(log.verify().is_ok(), "concurrent writers must not interleave into a broken chain");
    }
}
