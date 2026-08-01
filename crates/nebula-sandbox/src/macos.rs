//! macOS confinement via Seatbelt.
//!
//! Seatbelt profiles are written in a TinyScheme dialect and applied with
//! `sandbox_init(3)`. The profile below is deny-by-default with explicit
//! allowances, matching the [`Policy`] model exactly.
//!
//! ## The deprecation
//!
//! `sandbox_init` is formally deprecated and Apple offers no supported
//! replacement C API for confining a non-App-Sandbox child process. It still
//! functions on current macOS, and it is what OpenAI's Codex CLI uses for the
//! same job, so it is what Nebula uses — but the situation is tracked as a
//! standing risk, and the fallback if Apple removes it is to run agent tools in
//! a local VM, as Anthropic's Claude Cowork does.
//!
//! Everything except [`apply`] and the `sandbox_init` binding is built on every
//! platform. [`build_profile`] is pure string work, and the rules it enforces —
//! deny-by-default, and no path escaping the profile's string literals — are
//! worth testing on whatever machine happens to run the tests.

use crate::Result;
use crate::policy::{NetworkAccess, Policy};
#[cfg(target_os = "macos")]
use crate::{Enforcement, SandboxError};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int sandbox_init(const char *profile, uint64_t flags, char **errorbuf);`
    fn sandbox_init(
        profile: *const libc::c_char,
        flags: u64,
        errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;

    /// Frees the error buffer `sandbox_init` allocates.
    fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

/// Apply `policy` to the calling process.
#[cfg(target_os = "macos")]
pub fn apply(policy: &Policy) -> Result<Enforcement> {
    let profile = build_profile(policy)?;
    let profile_c = std::ffi::CString::new(profile)
        .map_err(|e| SandboxError::InvalidPolicy(format!("profile contained a NUL byte: {e}")))?;

    let mut error_buf: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: `profile_c` is a valid NUL-terminated C string that outlives the
    // call, and `error_buf` is a valid out-pointer. On failure the callee
    // allocates a message that is freed below.
    let result = unsafe { sandbox_init(profile_c.as_ptr(), 0, &mut error_buf) };

    if result == 0 {
        return Ok(Enforcement::Full { mechanism: "macOS Seatbelt".to_string() });
    }

    let message = if error_buf.is_null() {
        "sandbox_init failed without a message".to_string()
    } else {
        // SAFETY: non-null on failure and NUL-terminated, per sandbox_init(3).
        let msg = unsafe { std::ffi::CStr::from_ptr(error_buf) }.to_string_lossy().into_owned();
        // SAFETY: the pointer came from sandbox_init and is freed exactly once.
        unsafe { sandbox_free_error(error_buf) };
        msg
    };
    Err(SandboxError::ApplyFailed(format!("sandbox_init: {message}")))
}

/// Build the Seatbelt profile text for a policy.
///
/// Public so it can be inspected — and tested — on any platform without
/// actually confining the calling process.
pub fn build_profile(policy: &Policy) -> Result<String> {
    policy.validate()?;

    let mut profile = String::from("(version 1)\n(deny default)\n");

    // Reading these is required for any process to start at all — the dynamic
    // linker, the locale data, the timezone database.
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow sysctl-read)\n");
    profile.push_str("(allow mach-lookup)\n");
    profile.push_str("(allow file-read-metadata)\n");

    for path in &policy.read_paths {
        let resolved = crate::resolve(path)?;
        profile.push_str(&format!(
            "(allow file-read* (subpath {}))\n",
            quote_scheme(&resolved.to_string_lossy())
        ));
    }

    for path in &policy.write_paths {
        let resolved = crate::resolve(path)?;
        let quoted = quote_scheme(&resolved.to_string_lossy());
        profile.push_str(&format!("(allow file-write* (subpath {quoted}))\n"));
        profile.push_str(&format!("(allow file-read* (subpath {quoted}))\n"));
    }

    for path in &policy.exec_paths {
        let resolved = crate::resolve(path)?;
        profile.push_str(&format!(
            "(allow process-exec (subpath {}))\n",
            quote_scheme(&resolved.to_string_lossy())
        ));
    }

    match policy.network {
        NetworkAccess::Allowed => profile.push_str("(allow network*)\n"),
        // Explicit rather than relying on `deny default`, so an audit of the
        // profile text can see the decision.
        NetworkAccess::Denied => profile.push_str("(deny network*)\n"),
    }

    Ok(profile)
}

/// Quote a path as a TinyScheme string literal.
///
/// Backslashes and quotes must be escaped, or a path containing either would
/// terminate the literal early and change the meaning of the profile — which is
/// a sandbox escape, not a cosmetic bug.
fn quote_scheme(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn profiles_deny_by_default() {
        let profile = build_profile(&Policy::deny_all()).unwrap();
        assert!(profile.starts_with("(version 1)\n(deny default)"));
    }

    #[test]
    fn granted_paths_appear_in_the_profile() {
        // Real directories, because `resolve` canonicalises: asserting on
        // `/bin` would pass on macOS and fail on a Linux box where `/bin` is a
        // symlink to `/usr/bin`.
        let dir = TempDir::new().unwrap();
        let read = dir.path().join("read");
        let write = dir.path().join("write");
        let exec = dir.path().join("exec");
        for path in [&read, &write, &exec] {
            std::fs::create_dir(path).unwrap();
        }

        let policy = Policy::builder().read(&read).write(&write).exec(&exec).build();
        let profile = build_profile(&policy).unwrap();

        let canonical = |p: &PathBuf| p.canonicalize().unwrap().display().to_string();
        assert!(
            profile.contains(&format!("(allow file-read* (subpath \"{}\"))", canonical(&read))),
            "{profile}"
        );
        assert!(
            profile.contains(&format!("(allow file-write* (subpath \"{}\"))", canonical(&write))),
            "{profile}"
        );
        assert!(
            profile.contains(&format!("(allow process-exec (subpath \"{}\"))", canonical(&exec))),
            "{profile}"
        );
    }

    #[test]
    fn network_denial_is_stated_explicitly() {
        let denied = build_profile(&Policy::deny_all()).unwrap();
        assert!(denied.contains("(deny network*)"));

        let allowed =
            build_profile(&Policy::builder().network(NetworkAccess::Allowed).build()).unwrap();
        assert!(allowed.contains("(allow network*)"));
    }

    #[test]
    fn paths_with_quotes_cannot_escape_the_string_literal() {
        // A directory literally named `evil") (allow default) ("x` would
        // rewrite the profile if its quotes reached the output unescaped.
        let hostile = PathBuf::from("/tmp/evil\") (allow default) (\"x");
        let policy = Policy { read_paths: vec![hostile], ..Policy::deny_all() };
        let profile = build_profile(&policy).unwrap();

        // The text `(allow default)` is still *present* — it is part of the
        // directory's name. What matters is that it stays inside the string
        // literal, which it does exactly when the injected quotes are escaped.
        // Asserting merely that the text is absent would pass even if nothing
        // were escaped at all, which is how this went unverified for so long.
        assert!(
            !profile.contains("\") (allow default) (\""),
            "the hostile quotes reached the profile unescaped:\n{profile}"
        );
        assert!(
            profile.contains("\\\") (allow default) (\\\""),
            "the hostile path should appear with its quotes escaped:\n{profile}"
        );
    }

    #[test]
    fn backslashes_are_escaped() {
        let policy = Policy { read_paths: vec![PathBuf::from("/tmp/a\\b")], ..Policy::deny_all() };
        let profile = build_profile(&policy).unwrap();
        assert!(profile.contains("\\\\"), "{profile}");
    }

    #[test]
    fn an_invalid_policy_is_rejected_before_reaching_the_kernel() {
        let policy = Policy { read_paths: vec![PathBuf::from("relative")], ..Policy::deny_all() };
        assert!(build_profile(&policy).is_err());
    }
}
