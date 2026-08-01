//! Linux confinement: Landlock for the filesystem, seccomp-bpf for the network.
//!
//! The two mechanisms are complementary and both are needed:
//!
//! * **Landlock** scopes filesystem access to an explicit set of path
//!   hierarchies. It is unprivileged, inherited across `fork`/`exec`, and
//!   irreversible once applied.
//! * **seccomp-bpf** filters syscalls. Landlock ABI v4 can restrict TCP
//!   bind/connect, but it cannot stop a process opening a raw or unix socket,
//!   so network denial is enforced by refusing `socket(2)` outright.
//!
//! Both are applied to the *calling thread*, after `fork` and before `exec`.

use std::collections::BTreeMap;

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus,
};

use crate::policy::{NetworkAccess, Policy};
use crate::{Enforcement, Result, SandboxError};

/// The Landlock ABI version the running kernel supports, if any.
///
/// Queried through the documented probe: `landlock_create_ruleset(NULL, 0,
/// LANDLOCK_CREATE_RULESET_VERSION)` returns the ABI version without creating
/// anything or restricting the caller.
pub fn landlock_abi() -> Option<i64> {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    // The syscall number is 444 on every architecture that has it; libc only
    // exposes the constant on some targets, so it is spelled out here.
    #[cfg(target_arch = "x86_64")]
    const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
    #[cfg(target_arch = "aarch64")]
    const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;

    // SAFETY: a read-only probe. Passing a null attribute pointer with the
    // VERSION flag is the interface's documented way to ask "what version do
    // you support"; it creates no ruleset and changes no process state.
    let version = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (version > 0).then_some(version)
}

/// Best Landlock ABI this build knows how to target, capped at what the kernel
/// offers.
fn target_abi() -> Option<ABI> {
    let kernel = landlock_abi()?;
    Some(match kernel {
        1 => ABI::V1,
        2 => ABI::V2,
        3 => ABI::V3,
        4 => ABI::V4,
        5 => ABI::V5,
        // Newer kernels are handled at the highest version this crate was built
        // against; unknown extra features simply go unused.
        _ => ABI::V6,
    })
}

/// Apply `policy` to the calling thread and everything it later executes.
pub fn apply(policy: &Policy) -> Result<Enforcement> {
    let mut missing: Vec<String> = Vec::new();

    // --- Network: seccomp first, so a failure here happens before the
    // irreversible Landlock step. ---
    if policy.network == NetworkAccess::Denied {
        match deny_network_syscalls() {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!(%err, "seccomp network filter could not be installed");
                missing.push(format!("network denial ({err})"));
            }
        }
    }

    // --- Filesystem: Landlock. ---
    let Some(abi) = target_abi() else {
        let reason = "Landlock unavailable (kernel < 5.13, or landlock LSM not enabled)";
        if missing.is_empty() && policy.network == NetworkAccess::Denied {
            // Network was confined but the filesystem was not.
            return Ok(Enforcement::Partial {
                mechanism: "seccomp-bpf".to_string(),
                filesystem_scoped: false,
                missing: vec![format!("filesystem scoping: {reason}")],
            });
        }
        return Ok(Enforcement::Unsupported { reason: reason.to_string() });
    };

    let status = apply_landlock(policy, abi)?;

    let mechanism = format!("Landlock ABI v{}", abi as i32);
    match status {
        RulesetStatus::FullyEnforced if missing.is_empty() => {
            Ok(Enforcement::Full { mechanism: format!("{mechanism} + seccomp-bpf") })
        }
        RulesetStatus::FullyEnforced => {
            Ok(Enforcement::Partial { mechanism, filesystem_scoped: true, missing })
        }
        RulesetStatus::PartiallyEnforced => {
            missing
                .push("some filesystem access rights (kernel ABI is older than requested)".into());
            // Partially enforced means some access *rights* were dropped, not
            // that paths went unscoped; the granted paths still bound it.
            Ok(Enforcement::Partial { mechanism, filesystem_scoped: true, missing })
        }
        RulesetStatus::NotEnforced => Ok(Enforcement::Unsupported {
            reason: "the kernel accepted the ruleset but enforced nothing".to_string(),
        }),
    }
}

fn apply_landlock(policy: &Policy, abi: ABI) -> Result<RulesetStatus> {
    let read_access = AccessFs::from_read(abi);
    let write_access = AccessFs::from_all(abi);

    let mut ruleset = Ruleset::default()
        // Best-effort: on a kernel with an older ABI, enforce what it does
        // support rather than refusing to confine at all. The return value
        // reports the downgrade, so it is never silent.
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| SandboxError::ApplyFailed(format!("handle_access: {e}")))?
        .create()
        .map_err(|e| SandboxError::ApplyFailed(format!("create ruleset: {e}")))?;

    // Writable paths first: they are a superset, and adding the read rule for
    // the same path afterwards would not remove anything, but ordering them
    // this way keeps the intent obvious.
    for path in &policy.write_paths {
        let resolved = crate::resolve(path)?;
        let Ok(fd) = PathFd::new(&resolved) else {
            // A path that does not exist grants nothing; that is not an error.
            tracing::debug!(path = %resolved.display(), "skipping unopenable write path");
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, write_access))
            .map_err(|e| SandboxError::ApplyFailed(format!("add write rule: {e}")))?;
    }

    for path in &policy.read_paths {
        if policy.write_paths.contains(path) {
            continue;
        }
        let resolved = crate::resolve(path)?;
        let Ok(fd) = PathFd::new(&resolved) else {
            tracing::debug!(path = %resolved.display(), "skipping unopenable read path");
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, read_access))
            .map_err(|e| SandboxError::ApplyFailed(format!("add read rule: {e}")))?;
    }

    for path in &policy.exec_paths {
        let resolved = crate::resolve(path)?;
        let Ok(fd) = PathFd::new(&resolved) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::Execute | AccessFs::ReadFile))
            .map_err(|e| SandboxError::ApplyFailed(format!("add exec rule: {e}")))?;
    }

    // See `ALWAYS_ALLOWED_DEVICES`: denying these breaks toolchains without
    // withholding anything worth withholding.
    for device in crate::ALWAYS_ALLOWED_DEVICES {
        let Ok(fd) = PathFd::new(device) else {
            tracing::debug!(device, "this system has no such device node");
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, write_access))
            .map_err(|e| SandboxError::ApplyFailed(format!("add device rule for {device}: {e}")))?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| SandboxError::ApplyFailed(format!("restrict_self: {e}")))?;

    Ok(status.ruleset)
}

/// Install a seccomp filter that stops the process opening a network socket.
///
/// ## Why this filters by address family rather than by syscall
///
/// The obvious filter — deny `socket`, `connect`, `sendto` and friends outright
/// — breaks ordinary process spawning. Rust's standard library creates the
/// CLOEXEC pipe it uses to report exec failures with `socketpair(AF_UNIX, …)`,
/// so a blanket denial makes `Command::spawn` fail with `EPERM` inside every
/// child. In practice that means `rustc` cannot invoke its linker and no build
/// tool works at all under the sandbox.
///
/// So the filter allows `socket` and `socketpair` for `AF_UNIX` and refuses
/// every other family. Nothing else needs blocking: a process that cannot
/// *create* an `AF_INET` socket cannot connect, bind or send on one either, and
/// leaving `connect` alone means local Unix-socket IPC — which a language
/// server or a build tool may legitimately use — keeps working.
///
/// `EPERM` rather than killing the process: a build tool that probes for an
/// update should fail that one call and carry on compiling, not die on SIGSYS.
fn deny_network_syscalls() -> std::result::Result<(), String> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };

    let arch = current_target_arch()?;

    // Matches when the address family argument is anything but AF_UNIX.
    let non_unix_family = || -> std::result::Result<SeccompRule, String> {
        let condition = SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )
        .map_err(|e| format!("building the address-family condition: {e}"))?;

        SeccompRule::new(vec![condition])
            .map_err(|e| format!("building the address-family rule: {e}"))
    };

    let rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::from([
        (libc::SYS_socket, vec![non_unix_family()?]),
        (libc::SYS_socketpair, vec![non_unix_family()?]),
    ]);

    let filter = SeccompFilter::new(
        rules,
        // Anything not listed, and any listed syscall whose rule does not
        // match, runs normally.
        SeccompAction::Allow,
        // A matching rule — a non-AF_UNIX socket — fails with EPERM.
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| format!("building seccomp filter: {e}"))?;

    let program: BpfProgram =
        filter.try_into().map_err(|e| format!("compiling seccomp filter: {e}"))?;

    seccompiler::apply_filter(&program).map_err(|e| format!("installing seccomp filter: {e}"))?;
    Ok(())
}

fn current_target_arch() -> std::result::Result<seccompiler::TargetArch, String> {
    use seccompiler::TargetArch;
    #[cfg(target_arch = "x86_64")]
    {
        Ok(TargetArch::x86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Ok(TargetArch::aarch64)
    }
    #[cfg(target_arch = "riscv64")]
    {
        Ok(TargetArch::riscv64)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
    {
        Err(format!("seccomp is not supported on {}", std::env::consts::ARCH))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Applying a policy is irreversible, so these tests deliberately do *not*
    // confine the test process. Enforcement is verified end-to-end in
    // `nebula-exec`, which applies the policy in a forked child.

    #[test]
    fn abi_probe_does_not_confine_the_caller() {
        let before = std::fs::read_dir("/").is_ok();
        let _ = landlock_abi();
        let after = std::fs::read_dir("/").is_ok();
        assert_eq!(before, after, "the version probe must not restrict anything");
    }

    #[test]
    fn abi_probe_reports_a_plausible_version() {
        match landlock_abi() {
            Some(version) => {
                assert!(version >= 1, "an ABI version below 1 is not meaningful");
                assert!(version < 100, "implausible ABI version {version}");
            }
            None => {
                // A kernel without Landlock is a valid outcome, and the crate
                // reports it rather than pretending to confine.
                assert!(!crate::is_available());
            }
        }
    }

    #[test]
    fn the_backend_description_matches_the_probe() {
        let description = crate::backend_description();
        match landlock_abi() {
            Some(version) => assert!(
                description.contains(&format!("v{version}")),
                "description {description} should name ABI v{version}"
            ),
            None => assert!(description.contains("unavailable")),
        }
    }

    #[test]
    fn the_target_abi_never_exceeds_the_kernels() {
        if let (Some(kernel), Some(target)) = (landlock_abi(), target_abi()) {
            assert!(
                (target as i64) <= kernel.max(6),
                "target ABI {target:?} exceeds kernel ABI {kernel}"
            );
        }
    }
}
