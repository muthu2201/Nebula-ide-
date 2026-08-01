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
//! The deprecation warning is silenced at the single call site rather than
//! crate-wide, so a future removal surfaces as a compile error here.

use crate::policy::{NetworkAccess, Policy};
use crate::{Enforcement, Result, SandboxError};

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
/// Exposed (crate-internally) so it can be tested on any platform without
/// actually confining the test process.
pub(crate) fn build_profile(policy: &Policy) -> Result<String> {
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

    #[test]
    fn profiles_deny_by_default() {
        let profile = build_profile(&Policy::deny_all()).unwrap();
        assert!(profile.starts_with("(version 1)\n(deny default)"));
    }

    #[test]
    fn granted_paths_appear_in_the_profile() {
        let policy = Policy::builder().read("/usr").write("/tmp/work").exec("/bin").build();
        let profile = build_profile(&policy).unwrap();

        assert!(profile.contains("(allow file-read* (subpath \"/usr\"))"), "{profile}");
        assert!(profile.contains("file-write* (subpath \"/tmp/work\")"), "{profile}");
        assert!(profile.contains("process-exec (subpath \"/bin\")"), "{profile}");
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
        // A directory literally named `evil") (allow default) ("` would
        // otherwise rewrite the profile.
        let hostile = PathBuf::from("/tmp/evil\") (allow default) (\"x");
        let policy = Policy { read_paths: vec![hostile], ..Policy::deny_all() };
        let profile = build_profile(&policy).unwrap();

        assert!(!profile.contains("(allow default)"), "profile injection succeeded:\n{profile}");
        assert!(profile.contains("\\\""), "the quote should have been escaped:\n{profile}");
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
