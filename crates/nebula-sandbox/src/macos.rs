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
//! Everything except `apply` and the `sandbox_init` binding is built on every
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

    // Mapping a file's pages as executable is a *separate* Seatbelt operation
    // from reading it, and dyld does both: it opens the shared cache and the
    // linked dylibs, then maps them executable before `main` runs. A profile
    // that grants `file-read*` alone lets the open succeed and refuses the
    // mapping, so the grant belongs here.
    //
    // It is *not* why the macOS stress run aborted, though this comment used
    // to say so. Every program there died on `SIGABRT` before `main` because
    // `sandbox_init` was being called from a `pre_exec` closure, where
    // allocation is illegal and libmalloc aborts the child; the profile was
    // never reached, so nothing it contained could have been the cause. The
    // grant stays on its own merits and the diagnosis does not.
    //
    // Unqualified, and it grants no file access on its own: a mapping still
    // requires the file to be open, which the read rules above govern. What it
    // permits is executing pages of a file the policy already allows reading.
    profile.push_str("(allow file-map-executable)\n");

    // See `ALWAYS_ALLOWED_DEVICES`: denying these breaks toolchains without
    // withholding anything worth withholding.
    for device in crate::ALWAYS_ALLOWED_DEVICES {
        profile.push_str(&format!(
            "(allow file-read* file-write* (literal {}))\n",
            quote_scheme(device)
        ));
    }

    for path in &policy.read_paths {
        for quoted in spellings_of(&crate::resolve(path)?) {
            profile.push_str(&format!("(allow file-read* (subpath {quoted}))\n"));
        }
    }

    for path in &policy.write_paths {
        for quoted in spellings_of(&crate::resolve(path)?) {
            profile.push_str(&format!("(allow file-write* (subpath {quoted}))\n"));
            profile.push_str(&format!("(allow file-read* (subpath {quoted}))\n"));
        }
    }

    for path in &policy.exec_paths {
        for quoted in spellings_of(&crate::resolve(path)?) {
            profile.push_str(&format!("(allow process-exec (subpath {quoted}))\n"));
            // Starting a binary means reading it, and on macOS the dynamic loader
            // reads it again along with anything it links against. `process-exec`
            // alone leaves every launch failing at the loader.
            profile.push_str(&format!("(allow file-read* (subpath {quoted}))\n"));
        }
    }

    match policy.network {
        NetworkAccess::Allowed => profile.push_str("(allow network*)\n"),
        // Explicit rather than relying on `deny default`, so an audit of the
        // profile text can see the decision.
        NetworkAccess::Denied => profile.push_str("(deny network*)\n"),
    }

    Ok(profile)
}

/// Where the data volume is mounted on a macOS system with a sealed system
/// volume — every release since Catalina.
const DATA_VOLUME_ROOT: &str = "/System/Volumes/Data";

/// Every absolute path that names `resolved`, quoted for the profile.
///
/// A macOS boot disk is two volumes, and the directories a user actually lives
/// in — `/Users`, `/Applications`, `/opt`, `/private`, `/usr/local` — are on the
/// data one, grafted into the system volume's namespace by *firmlinks*. Each of
/// them therefore has two equally valid absolute paths: `/Users/x` and
/// `/System/Volumes/Data/Users/x` are the same directory.
///
/// `canonicalize` returns the first spelling; Seatbelt was evidently given the
/// second. Granting `process-exec` on `~/.cargo` produced, from `sandbox-exec`
/// itself:
///
/// ```text
/// execvp() of '/Users/runner/.cargo/bin/rustc' failed: Operation not permitted
/// ```
///
/// — an exec denied on a path the profile named verbatim, which can only happen
/// if the rule and the check were talking about different strings. Binaries in
/// `/usr/bin`, on the system volume and not firmlinked, execed fine in the same
/// run.
///
/// So a rule is emitted under both spellings. This widens nothing: the twin
/// names the same inode, and where a path is not firmlinked the twin simply
/// does not exist and the rule grants nothing. What it removes is the
/// requirement to guess which spelling the kernel will present.
fn spellings_of(resolved: &std::path::Path) -> Vec<String> {
    let primary = resolved.to_string_lossy().into_owned();

    // The root already covers both volumes, and its twin would carry a
    // trailing slash the profile has no use for.
    if primary == "/" || primary == DATA_VOLUME_ROOT {
        return vec![quote_scheme(&primary)];
    }

    // Which way the twin runs depends on which spelling `canonicalize` handed
    // back, and that is exactly the thing not worth betting on: `realpath`
    // resolves firmlinks on some macOS releases and leaves them alone on
    // others. Both directions are covered so neither has to be predicted.
    let twin = match primary.strip_prefix(DATA_VOLUME_ROOT) {
        Some(shorter) => shorter.to_string(),
        None => format!("{DATA_VOLUME_ROOT}{primary}"),
    };
    vec![quote_scheme(&primary), quote_scheme(&twin)]
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
    fn the_standard_devices_are_always_permitted() {
        // A deny-all policy still has to let a compiler open /dev/null.
        let profile = build_profile(&Policy::deny_all()).unwrap();
        for device in crate::ALWAYS_ALLOWED_DEVICES {
            assert!(
                profile.contains(&format!("(literal \"{device}\")")),
                "{device} is missing from a deny-all profile:\n{profile}"
            );
        }
        assert!(!profile.contains("/dev/tty"), "the terminal is not a build dependency");
    }

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
    fn every_rule_names_its_path_on_both_volumes() {
        // `/Users`, `/opt`, `/private` and the rest are firmlinked onto the
        // data volume and so have two absolute paths. `sandbox-exec` refused
        // `execvp()` of a binary under `~/.cargo` while the profile named
        // `~/.cargo` verbatim, which is only possible if the kernel presented
        // the other spelling. Both are emitted.
        let dir = TempDir::new().unwrap();
        let policy = Policy::builder().read(dir.path()).write(dir.path()).exec(dir.path()).build();
        let profile = build_profile(&policy).unwrap();

        let canonical = dir.path().canonicalize().unwrap().display().to_string();
        let twin = format!("/System/Volumes/Data{canonical}");
        for operation in ["file-read*", "file-write*"] {
            assert!(
                profile.contains(&format!("(allow {operation} (subpath \"{twin}\"))")),
                "{operation} was granted on only one of the two paths naming the directory:\n{profile}"
            );
        }
        assert!(
            profile.contains(&format!("(allow process-exec (subpath \"{twin}\"))")),
            "{profile}"
        );
    }

    #[test]
    fn a_data_volume_path_gains_its_short_spelling_rather_than_a_second_prefix() {
        // `realpath` resolves firmlinks on some macOS releases, so the
        // canonical form of `/Users/x` can arrive already on the data volume.
        // The twin then has to run the other way.
        let policy = Policy {
            read_paths: vec![PathBuf::from("/System/Volumes/Data/Users/x")],
            ..Policy::deny_all()
        };
        let profile = build_profile(&policy).unwrap();
        assert!(
            !profile.contains("/System/Volumes/Data/System/Volumes/Data"),
            "a path already on the data volume was prefixed again:\n{profile}"
        );
        assert!(
            profile.contains("(allow file-read* (subpath \"/Users/x\"))"),
            "the short spelling of a data-volume path was not granted:\n{profile}"
        );
    }

    #[test]
    fn the_root_is_not_given_a_trailing_slash_twin() {
        let policy = Policy { read_paths: vec![PathBuf::from("/")], ..Policy::deny_all() };
        let profile = build_profile(&policy).unwrap();
        assert!(!profile.contains("\"/System/Volumes/Data/\""), "{profile}");
    }

    #[test]
    fn the_system_read_list_never_grants_the_whole_data_volume() {
        // `/System/Volumes/Data` is the mount point of every user file on the
        // machine, and `(subpath "/System")` reaches it. A read list that
        // contains either spelling confines nothing on macOS while every
        // "denied access is refused" test carries on passing, because those
        // tests only require a non-zero exit and a process that never starts
        // gives them one.
        for dir in crate::system_read_directories() {
            let path = dir.display().to_string();
            assert!(
                path != "/" && path != "/System" && path != "/System/Volumes",
                "`{path}` in the system read list grants the entire data volume"
            );
        }
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
