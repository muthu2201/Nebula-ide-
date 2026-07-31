//! Hardware fingerprinting.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// One stable hardware signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareId {
    /// What kind of signal this is.
    pub kind: String,
    /// The HMAC of its value. The raw value never leaves this module.
    pub digest: String,
}

/// A machine fingerprint: several independent signals, each hashed separately.
///
/// Separately, rather than as one combined hash, so that a partial match is
/// expressible. A single hash is all-or-nothing: change one component and the
/// licence stops working, which punishes a user for upgrading their RAM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// The signals, sorted by kind.
    pub signals: Vec<HardwareId>,
}

/// How many signals must match for a fingerprint to be accepted.
///
/// Two out of the collected signals: enough that a different machine will not
/// match by chance, loose enough that replacing a disk or upgrading memory does
/// not invalidate a licence the user paid for.
pub const REQUIRED_MATCHES: usize = 2;

/// The key the HMAC is taken under.
///
/// Not a secret — it is in the binary — but it does mean a fingerprint from
/// Nebula cannot be correlated with one from another product using the same raw
/// identifiers, which is a real privacy property even though it is not a
/// security one.
const FINGERPRINT_DOMAIN: &[u8] = b"nebula-ide-fingerprint-v1";

impl Fingerprint {
    /// Collect a fingerprint for this machine.
    pub fn collect() -> Fingerprint {
        let mut signals = Vec::new();

        // The hostname: stable, and cheap to read.
        if let Ok(hostname) = hostname() {
            signals.push(HardwareId::new("hostname", hostname.as_bytes()));
        }

        // The CPU model and core count. Stable unless the machine changes.
        let system = sysinfo::System::new_all();
        if let Some(cpu) = system.cpus().first() {
            signals.push(HardwareId::new("cpu", cpu.brand().as_bytes()));
        }
        signals.push(HardwareId::new(
            "cpu-count",
            sysinfo::System::physical_core_count().unwrap_or(0).to_string().as_bytes(),
        ));

        // Total memory, rounded to the nearest gigabyte so that a reading which
        // fluctuates by a few megabytes does not change the fingerprint.
        let gigabytes = system.total_memory() / (1024 * 1024 * 1024);
        signals.push(HardwareId::new("memory-gb", gigabytes.to_string().as_bytes()));

        // A machine identifier from the OS, where one is available.
        if let Some(machine_id) = machine_id() {
            signals.push(HardwareId::new("machine-id", machine_id.as_bytes()));
        }

        signals.sort_by(|a, b| a.kind.cmp(&b.kind));
        Fingerprint { signals }
    }

    /// How many signals this fingerprint shares with `other`.
    pub fn matching_signals(&self, other: &Fingerprint) -> usize {
        self.signals
            .iter()
            .filter(|signal| {
                other
                    .signals
                    .iter()
                    .any(|theirs| theirs.kind == signal.kind && theirs.digest == signal.digest)
            })
            .count()
    }

    /// Whether `other` is close enough to be the same machine.
    pub fn matches(&self, other: &Fingerprint) -> bool {
        // A fingerprint with too few signals to reach the threshold must match
        // exactly, rather than being accepted on a single coincidence.
        let required = REQUIRED_MATCHES.min(self.signals.len()).min(other.signals.len());
        if required == 0 {
            return false;
        }
        self.matching_signals(other) >= required
    }

    /// A short, stable rendering for display and support.
    pub fn short_id(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for signal in &self.signals {
            hasher.update(signal.kind.as_bytes());
            hasher.update(signal.digest.as_bytes());
        }
        hasher.finalize().to_hex()[..12].to_string()
    }

    /// Number of signals collected.
    pub fn len(&self) -> usize {
        self.signals.len()
    }

    /// Whether no signals were collected.
    pub fn is_empty(&self) -> bool {
        self.signals.is_empty()
    }
}

impl HardwareId {
    /// Hash a raw identifier under the fingerprint domain key.
    pub fn new(kind: &str, value: &[u8]) -> HardwareId {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(FINGERPRINT_DOMAIN)
            .expect("HMAC accepts a key of any length");
        mac.update(kind.as_bytes());
        mac.update(b":");
        mac.update(value);

        HardwareId {
            kind: kind.to_string(),
            // 128 bits is far beyond what is needed to avoid collisions between
            // machines, and keeps the licence file readable.
            digest: hex(&mac.finalize().into_bytes()[..16]),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hostname() -> std::io::Result<String> {
    #[cfg(unix)]
    {
        // `gethostname` would need libc; the environment and /etc/hostname cover
        // every realistic case without adding an unsafe call.
        if let Ok(name) = std::env::var("HOSTNAME")
            && !name.is_empty()
        {
            return Ok(name);
        }
        std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string())
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME")
            .map_err(|_| std::io::Error::other("COMPUTERNAME is not set"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::other("no hostname source on this platform"))
    }
}

/// A machine identifier provided by the operating system, if there is one.
fn machine_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // systemd's machine-id, or the D-Bus one on systems without systemd.
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(id) = std::fs::read_to_string(path) {
                let id = id.trim().to_string();
                if !id.is_empty() {
                    return Some(id);
                }
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        // The hardware UUID from IOKit, read through ioreg rather than linking
        // the framework.
        let output = std::process::Command::new("/usr/sbin/ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .find(|line| line.contains("IOPlatformUUID"))
            .and_then(|line| line.split('"').nth(3))
            .map(str::to_string)
    }
    #[cfg(windows)]
    {
        std::env::var("PROCESSOR_IDENTIFIER").ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint_from(signals: &[(&str, &str)]) -> Fingerprint {
        let mut signals: Vec<HardwareId> = signals
            .iter()
            .map(|(kind, value)| HardwareId::new(kind, value.as_bytes()))
            .collect();
        signals.sort_by(|a, b| a.kind.cmp(&b.kind));
        Fingerprint { signals }
    }

    #[test]
    fn collecting_produces_several_signals() {
        let fingerprint = Fingerprint::collect();
        assert!(
            fingerprint.len() >= 2,
            "too few signals to distinguish machines: {fingerprint:?}"
        );
    }

    #[test]
    fn collection_is_stable_within_a_run() {
        assert_eq!(Fingerprint::collect(), Fingerprint::collect());
    }

    #[test]
    fn a_fingerprint_matches_itself() {
        let fingerprint = Fingerprint::collect();
        assert!(fingerprint.matches(&fingerprint));
    }

    #[test]
    fn raw_identifiers_never_appear_in_the_fingerprint() {
        // A licence file must not disclose anything about the machine.
        let signal = HardwareId::new("hostname", b"very-distinctive-machine-name");
        let rendered = serde_json::to_string(&signal).unwrap();

        assert!(
            !rendered.contains("very-distinctive"),
            "the raw identifier leaked: {rendered}"
        );
        assert_eq!(signal.digest.len(), 32, "expected a 128-bit digest in hex");
    }

    #[test]
    fn different_values_produce_different_digests() {
        assert_ne!(
            HardwareId::new("cpu", b"Intel Core i7").digest,
            HardwareId::new("cpu", b"Apple M3").digest
        );
    }

    #[test]
    fn the_same_value_under_a_different_kind_hashes_differently() {
        // Domain separation: a hostname of "abc" must not collide with a machine
        // id of "abc".
        assert_ne!(
            HardwareId::new("hostname", b"abc").digest,
            HardwareId::new("machine-id", b"abc").digest
        );
    }

    #[test]
    fn an_unrelated_machine_does_not_match() {
        let mine = fingerprint_from(&[
            ("hostname", "my-laptop"),
            ("cpu", "Apple M3 Pro"),
            ("machine-id", "aaaa"),
            ("memory-gb", "32"),
        ]);
        let theirs = fingerprint_from(&[
            ("hostname", "their-desktop"),
            ("cpu", "Intel Core i9"),
            ("machine-id", "bbbb"),
            ("memory-gb", "64"),
        ]);

        assert_eq!(mine.matching_signals(&theirs), 0);
        assert!(!mine.matches(&theirs));
    }

    #[test]
    fn a_memory_upgrade_does_not_invalidate_a_licence() {
        // The case that makes an exact-match fingerprint user-hostile.
        let before = fingerprint_from(&[
            ("hostname", "my-laptop"),
            ("cpu", "Apple M3 Pro"),
            ("machine-id", "aaaa"),
            ("memory-gb", "16"),
        ]);
        let after = fingerprint_from(&[
            ("hostname", "my-laptop"),
            ("cpu", "Apple M3 Pro"),
            ("machine-id", "aaaa"),
            ("memory-gb", "64"),
        ]);

        assert_eq!(before.matching_signals(&after), 3);
        assert!(before.matches(&after), "upgrading RAM must not lock the user out");
    }

    #[test]
    fn a_rename_and_a_disk_swap_together_still_match() {
        let before = fingerprint_from(&[
            ("hostname", "old-name"),
            ("cpu", "Apple M3 Pro"),
            ("machine-id", "aaaa"),
            ("memory-gb", "32"),
        ]);
        let after = fingerprint_from(&[
            ("hostname", "new-name"),
            ("cpu", "Apple M3 Pro"),
            ("machine-id", "bbbb"),
            ("memory-gb", "32"),
        ]);

        assert_eq!(before.matching_signals(&after), 2);
        assert!(before.matches(&after));
    }

    #[test]
    fn one_coincidental_signal_is_not_enough() {
        // Two machines with the same amount of RAM are not the same machine.
        let mine = fingerprint_from(&[
            ("hostname", "my-laptop"),
            ("cpu", "Apple M3 Pro"),
            ("machine-id", "aaaa"),
            ("memory-gb", "32"),
        ]);
        let theirs = fingerprint_from(&[
            ("hostname", "other-laptop"),
            ("cpu", "Intel Core i9"),
            ("machine-id", "cccc"),
            ("memory-gb", "32"),
        ]);

        assert_eq!(mine.matching_signals(&theirs), 1);
        assert!(!mine.matches(&theirs), "one shared signal must not be enough");
    }

    #[test]
    fn an_empty_fingerprint_never_matches() {
        let empty = Fingerprint { signals: Vec::new() };
        assert!(!empty.matches(&Fingerprint::collect()));
        assert!(!Fingerprint::collect().matches(&empty));
        assert!(!empty.matches(&empty), "nothing must match on no evidence");
    }

    #[test]
    fn short_ids_are_stable_and_distinct() {
        let first = fingerprint_from(&[("hostname", "a"), ("cpu", "x")]);
        let second = fingerprint_from(&[("hostname", "b"), ("cpu", "y")]);

        assert_eq!(first.short_id(), first.short_id());
        assert_ne!(first.short_id(), second.short_id());
        assert_eq!(first.short_id().len(), 12);
    }

    #[test]
    fn signal_order_does_not_affect_matching() {
        let ordered = fingerprint_from(&[("a", "1"), ("b", "2"), ("c", "3")]);
        let mut shuffled = ordered.clone();
        shuffled.signals.reverse();

        assert!(ordered.matches(&shuffled));
        assert_eq!(ordered.matching_signals(&shuffled), 3);
    }

    #[test]
    fn fingerprints_round_trip_through_serde() {
        let fingerprint = Fingerprint::collect();
        let json = serde_json::to_string(&fingerprint).unwrap();
        assert_eq!(serde_json::from_str::<Fingerprint>(&json).unwrap(), fingerprint);
    }
}
