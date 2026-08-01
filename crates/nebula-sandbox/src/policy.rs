//! Sandbox policies.
//!
//! A policy is deny-by-default: it starts with no filesystem access and no
//! network, and every capability has to be granted explicitly. That ordering is
//! deliberate — an allowlist that someone forgets to extend produces a tool
//! that fails loudly, while a denylist that someone forgets to extend produces
//! a silent hole.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Result, SandboxError};

/// Whether a sandboxed process may use the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NetworkAccess {
    /// No sockets at all.
    #[default]
    Denied,
    /// Full network access.
    ///
    /// There is deliberately no "allowlist of hosts" variant: enforcing that at
    /// the syscall layer requires a proxy or a network namespace, and offering
    /// a variant this crate cannot actually enforce would be worse than not
    /// offering it. Host allowlisting belongs at the agent's HTTP client.
    Allowed,
}

/// What a sandboxed process may do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Directories and files readable by the process.
    pub read_paths: Vec<PathBuf>,
    /// Directories and files writable by the process. Write implies read.
    pub write_paths: Vec<PathBuf>,
    /// Paths from which the process may execute binaries.
    pub exec_paths: Vec<PathBuf>,
    /// Network access.
    pub network: NetworkAccess,
    /// A label used in audit logs.
    pub label: String,
}

/// The system locations a toolchain reads from, for this platform.
///
/// Every `PATH` directory is included on all platforms — a binary that can be
/// executed has to be readable — plus the platform's own system roots.
fn system_read_directories() -> Vec<PathBuf> {
    let mut dirs = path_directories();

    #[cfg(unix)]
    dirs.extend(["/usr", "/lib", "/lib64", "/bin", "/etc", "/opt"].map(PathBuf::from));

    // macOS keeps the dyld shared cache and the system frameworks under
    // `/System`, and every dynamically linked binary reads them before `main`
    // runs. Without them nothing starts at all — which is why the macOS stress
    // run failed all six programs identically, including `/bin/sh`, whose own
    // directory was granted. `/private` carries the real `/tmp` and `/var`,
    // which are symlinks into it and so resolve there.
    #[cfg(target_os = "macos")]
    dirs.extend(["/System", "/Library", "/private"].map(PathBuf::from));

    #[cfg(windows)]
    for var in ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramData", "LOCALAPPDATA"] {
        if let Some(value) = std::env::var_os(var) {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                dirs.push(path);
            }
        }
    }

    dirs
}

/// The directories on `PATH`, in order, skipping empty and relative entries.
///
/// Relative entries are dropped because a sandbox rule has to name a fixed
/// place: a relative `PATH` entry means "wherever the process happens to be",
/// which is not something a policy can grant.
fn path_directories() -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&path).filter(|p| p.is_absolute()).collect()
}

/// Character devices every sandbox permits, whatever the policy says.
///
/// Denying `/dev/null` does not make a sandbox stronger — reads give EOF and
/// writes are discarded, so there is nothing to leak and nothing to persist —
/// but it does break almost every toolchain that exists. `go build` opens
/// `/dev/null` to obtain a build ID and fails outright without it; linkers,
/// shells and test harnesses do the same.
///
/// `/dev/tty` is deliberately *not* here. It is a real capability — a handle on
/// the user's terminal — and no build tool needs it to succeed.
pub const ALWAYS_ALLOWED_DEVICES: &[&str] =
    &["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom"];

impl Policy {
    /// Start building a deny-everything policy.
    pub fn builder() -> PolicyBuilder {
        PolicyBuilder::default()
    }

    /// A policy that denies everything.
    pub fn deny_all() -> Policy {
        Policy { label: "deny-all".to_string(), ..Default::default() }
    }

    /// The policy Nebula uses for an agent tool operating on one project.
    ///
    /// Read and write inside the project; read the toolchain directories a
    /// build needs; execute from the standard binary directories; no network.
    /// This is the profile that runs `cargo test` on the user's behalf.
    pub fn project_tool(project_root: impl AsRef<Path>) -> Policy {
        let root = project_root.as_ref().to_path_buf();
        let mut builder =
            Policy::builder().label("project-tool").write(&root).network(NetworkAccess::Denied);

        // Toolchains read their own installation; denying that makes every
        // build fail. Where "their own installation" *is* differs by platform,
        // and a Unix path on Windows is not merely useless — it is not
        // absolute, so `validate` rejects the whole policy and nothing runs at
        // all. That is exactly how the Windows stress run failed every program
        // before starting one, with "policy paths must be absolute, got /usr".
        for dir in system_read_directories() {
            builder = builder.read(dir);
        }

        // Execution is granted for every directory on `PATH`, rather than a
        // fixed list of standard ones. A hardcoded list is a guess about where
        // toolchains live, and it is only ever right about the platform it was
        // written on: on macOS `rustc` is in `~/.cargo/bin` and `python3` and
        // `go` come from a version manager's directory, none of which appear in
        // any list of "standard" binary directories.
        //
        // This is deliberate rather than a weakening. What contains a build
        // tool is that it cannot write outside the project and its caches, and
        // cannot reach the network. Which binaries it may *start* is not the
        // control doing the work — a build compiles and runs new code by
        // definition, so exec breadth was never the boundary.
        for dir in path_directories() {
            builder = builder.exec(dir);
        }

        if let Some(home) = dirs_home() {
            // Toolchain caches. Granting the whole home directory would defeat
            // the point, so only the specific caches a build needs are added.
            for cache in [".cargo", ".rustup", ".cache", ".npm", ".pyenv", "go"] {
                let path = home.join(cache);
                // Exec as well as write: on macOS `~/.cargo/bin/rustc` is a
                // symlink into `~/.rustup/toolchains/…`, and Seatbelt evaluates
                // the path it resolves to. Granting exec on the `PATH`
                // directory alone left `rustc` unable to start.
                builder = builder.exec(&path).write(path);
            }
        }
        // `temp_dir` reads TMPDIR, TMP and TEMP as the platform expects, and
        // returns a real absolute path on all of them.
        builder = builder.write(std::env::temp_dir());
        builder.build()
    }

    /// A read-only policy over one directory, for tools that only inspect.
    pub fn read_only(root: impl AsRef<Path>) -> Policy {
        let mut builder = Policy::builder()
            .label("read-only")
            .read(root.as_ref().to_path_buf())
            .network(NetworkAccess::Denied);

        // A policy for running an inspection tool still has to let the tool
        // start. Without this the policy cannot execute anything at all — not
        // even `/usr/bin/true` — which makes it useless for the one thing it
        // exists to do. The read-only part is about the *project*: the tool
        // cannot write to it, and cannot reach the network.
        for dir in system_read_directories() {
            builder = builder.read(dir);
        }
        for dir in path_directories() {
            builder = builder.exec(dir);
        }

        builder.build()
    }

    /// Whether the policy grants any filesystem access at all.
    pub fn grants_filesystem_access(&self) -> bool {
        !self.read_paths.is_empty() || !self.write_paths.is_empty()
    }

    /// Whether `path` is readable under this policy.
    ///
    /// This mirrors the kernel's decision for auditing and for pre-flight
    /// checks; it is not itself an enforcement mechanism.
    pub fn allows_read(&self, path: &Path) -> bool {
        let path = crate::resolve(path).unwrap_or_else(|_| path.to_path_buf());
        self.read_paths
            .iter()
            .chain(self.write_paths.iter())
            .any(|allowed| path.starts_with(allowed))
    }

    /// Whether `path` is writable under this policy.
    pub fn allows_write(&self, path: &Path) -> bool {
        let path = crate::resolve(path).unwrap_or_else(|_| path.to_path_buf());
        self.write_paths.iter().any(|allowed| path.starts_with(allowed))
    }

    /// Check the policy is coherent.
    pub fn validate(&self) -> Result<()> {
        for path in self.read_paths.iter().chain(&self.write_paths).chain(&self.exec_paths) {
            if !path.is_absolute() {
                return Err(SandboxError::InvalidPolicy(format!(
                    "policy paths must be absolute, got {}",
                    path.display()
                )));
            }
            // `..` in a policy path would be resolved by the kernel against the
            // process's cwd, which is not what the author meant.
            if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return Err(SandboxError::InvalidPolicy(format!(
                    "policy paths must not contain `..`, got {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// A stable, human-readable rendering, used in audit logs and tests.
    pub fn describe(&self) -> String {
        let mut out = format!("policy `{}`:\n", self.label);
        out.push_str(&format!("  network: {:?}\n", self.network));
        for (name, paths) in
            [("read", &self.read_paths), ("write", &self.write_paths), ("exec", &self.exec_paths)]
        {
            if paths.is_empty() {
                continue;
            }
            out.push_str(&format!("  {name}:\n"));
            let mut sorted: Vec<&PathBuf> = paths.iter().collect();
            sorted.sort();
            for path in sorted {
                out.push_str(&format!("    {}\n", path.display()));
            }
        }
        out
    }
}

/// Builder for [`Policy`].
#[derive(Debug, Default, Clone)]
pub struct PolicyBuilder {
    policy: Policy,
}

impl PolicyBuilder {
    /// Set the audit label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.policy.label = label.into();
        self
    }

    /// Grant read access to a path.
    pub fn read(mut self, path: impl Into<PathBuf>) -> Self {
        push_unique(&mut self.policy.read_paths, path.into());
        self
    }

    /// Grant read and write access to a path.
    pub fn write(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        push_unique(&mut self.policy.write_paths, path.clone());
        // Write without read is not a coherent grant for any real tool.
        push_unique(&mut self.policy.read_paths, path);
        self
    }

    /// Grant execute access to a path.
    pub fn exec(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        push_unique(&mut self.policy.exec_paths, path.clone());
        push_unique(&mut self.policy.read_paths, path);
        self
    }

    /// Set network access.
    pub fn network(mut self, network: NetworkAccess) -> Self {
        self.policy.network = network;
        self
    }

    /// Finish building.
    pub fn build(self) -> Policy {
        self.policy
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// The user's home directory, if it can be determined.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_project_tool_policy_is_valid_on_the_platform_it_was_built_for() {
        // The Windows stress run failed every program with "policy paths must
        // be absolute, got /usr", because the read paths were written as Unix
        // literals. This runs on every platform CI covers and fails there.
        // A platform-appropriate root, so a failure here is the library's
        // fault and not the test's choice of argument.
        Policy::project_tool(std::env::temp_dir().join("project"))
            .validate()
            .expect("a project-tool policy must be valid on its own platform");
    }

    #[test]
    fn a_project_tool_may_execute_the_programs_on_its_path() {
        // The bug this pins: exec was granted for a hardcoded `/usr/bin`,
        // `/usr/local/bin`, `/bin`. That list is right on Linux and wrong on
        // macOS, where the stress run failed to spawn rustc, python3, node and
        // go — every one of them lives outside it.
        let policy = Policy::project_tool(std::env::temp_dir().join("project"));

        for tool in ["rustc", "cargo", "python3", "node", "go", "sh"] {
            let Some(binary) = which_on_path(tool) else {
                continue; // Not installed here; nothing to assert.
            };
            let dir = binary.parent().unwrap();
            assert!(
                policy.exec_paths.iter().any(|granted| dir.starts_with(granted)),
                "{tool} lives in {} which the policy never grants exec on",
                dir.display()
            );
        }
    }

    /// Find a program on `PATH`, so the test asserts about this machine rather
    /// than about the machine it was written on.
    fn which_on_path(program: &str) -> Option<PathBuf> {
        path_directories().into_iter().map(|dir| dir.join(program)).find(|p| p.is_file())
    }

    #[test]
    fn a_new_policy_grants_nothing() {
        let policy = Policy::deny_all();
        assert!(!policy.grants_filesystem_access());
        assert_eq!(policy.network, NetworkAccess::Denied);
        assert!(!policy.allows_read(Path::new("/etc/passwd")));
        assert!(!policy.allows_write(Path::new("/tmp/anything")));
    }

    #[test]
    fn write_access_implies_read_access() {
        let dir = TempDir::new().unwrap();
        let policy = Policy::builder().write(dir.path()).build();
        assert!(policy.allows_write(dir.path()));
        assert!(policy.allows_read(dir.path()), "a writable path must also be readable");
    }

    #[test]
    fn read_access_does_not_imply_write_access() {
        let dir = TempDir::new().unwrap();
        let policy = Policy::builder().read(dir.path()).build();
        assert!(policy.allows_read(dir.path()));
        assert!(!policy.allows_write(dir.path()));
    }

    #[test]
    fn access_checks_cover_paths_beneath_a_granted_directory() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), b"").unwrap();

        let policy = Policy::builder().write(dir.path()).build();
        assert!(policy.allows_write(&dir.path().join("src/main.rs")));
        assert!(!policy.allows_write(Path::new("/etc/passwd")));
    }

    #[test]
    fn relative_policy_paths_are_rejected() {
        let policy = Policy::builder().read("relative/path").build();
        let err = policy.validate().unwrap_err();
        assert!(matches!(err, SandboxError::InvalidPolicy(_)));
    }

    #[test]
    fn policy_paths_containing_dot_dot_are_rejected() {
        // These would be resolved against the child's cwd, not the author's
        // intent.
        let policy = Policy::builder().read("/home/user/../../etc").build();
        assert!(policy.validate().is_err());
    }

    #[test]
    fn a_valid_policy_passes_validation() {
        let dir = TempDir::new().unwrap();
        Policy::builder().write(dir.path()).read("/usr").build().validate().unwrap();
    }

    #[test]
    fn duplicate_grants_are_collapsed() {
        let policy =
            Policy::builder().read("/usr").read("/usr").write("/tmp").write("/tmp").build();
        assert_eq!(
            policy.read_paths.iter().filter(|p| p.as_path() == Path::new("/usr")).count(),
            1
        );
        assert_eq!(policy.write_paths.len(), 1);
    }

    #[test]
    fn the_project_tool_policy_confines_writes_to_the_project_and_caches() {
        let dir = TempDir::new().unwrap();
        let policy = Policy::project_tool(dir.path());
        policy.validate().unwrap();

        assert!(policy.allows_write(dir.path()), "the project must be writable");
        assert!(policy.allows_read(Path::new("/usr")), "the toolchain must be readable");
        assert!(!policy.allows_write(Path::new("/usr")), "the toolchain must not be writable");
        assert!(!policy.allows_write(Path::new("/etc")), "system config must not be writable");
        assert_eq!(policy.network, NetworkAccess::Denied);
    }

    #[test]
    fn the_read_only_policy_denies_every_write() {
        let dir = TempDir::new().unwrap();
        let policy = Policy::read_only(dir.path());
        assert!(policy.allows_read(dir.path()));
        assert!(!policy.allows_write(dir.path()));
        assert!(policy.write_paths.is_empty());
    }

    #[test]
    fn descriptions_are_stable_for_the_audit_log() {
        let policy = Policy::builder()
            .label("test")
            .read("/b")
            .read("/a")
            .write("/w")
            .network(NetworkAccess::Denied)
            .build();
        let first = policy.describe();
        assert_eq!(first, policy.describe(), "the audit log must not jitter between runs");
        assert!(first.contains("policy `test`"));
        // Paths are sorted, so /a precedes /b regardless of insertion order.
        assert!(first.find("/a").unwrap() < first.find("/b").unwrap());
    }

    #[test]
    fn policies_round_trip_through_serde() {
        let dir = TempDir::new().unwrap();
        let policy = Policy::project_tool(dir.path());
        let json = serde_json::to_string(&policy).unwrap();
        let restored: Policy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, restored);
    }
}
