//! Sandboxed process execution.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nebula_sandbox::{Enforcement, Policy};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::limits::ResourceLimits;
use crate::{ExecError, Result};

/// How a process finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    /// Exited normally with this code.
    Exited(i32),
    /// Killed by this signal (Unix only).
    Signaled(i32),
    /// Killed because it exceeded its wall-clock timeout.
    TimedOut,
}

impl Status {
    /// Whether the process succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, Status::Exited(0))
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Exited(0) => write!(f, "exited successfully"),
            Status::Exited(code) => write!(f, "exited with code {code}"),
            Status::Signaled(signal) => write!(f, "killed by signal {signal}"),
            Status::TimedOut => write!(f, "timed out"),
        }
    }
}

/// What a process produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// How it finished.
    pub status: Status,
    /// Captured standard output, possibly truncated.
    pub stdout: String,
    /// Captured standard error, possibly truncated.
    pub stderr: String,
    /// Whether either stream hit the capture ceiling.
    pub truncated: bool,
    /// Wall-clock duration.
    pub duration: Duration,
    /// What confinement was in force, for the audit log.
    pub enforcement: Option<String>,
}

impl Output {
    /// Whether the process succeeded.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// stdout and stderr combined, as a tool would show them.
    pub fn combined(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

/// A process to run.
#[derive(Debug, Clone)]
pub struct Command {
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    inherit_env: bool,
    policy: Option<Policy>,
    limits: ResourceLimits,
    require_confinement: bool,
    stdin: Option<Vec<u8>>,
}

impl Command {
    /// A command that runs `program`.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            // Deny-by-default for the environment too: a tool that inherits the
            // editor's environment inherits every token in it.
            inherit_env: false,
            policy: None,
            limits: ResourceLimits::default(),
            require_confinement: false,
            stdin: None,
        }
    }

    /// Add an argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory.
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Set an environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Inherit the parent's environment.
    ///
    /// Off by default. Turning it on hands the child every secret in the
    /// editor's environment, so it is opt-in and should stay rare.
    pub fn inherit_env(mut self, inherit: bool) -> Self {
        self.inherit_env = inherit;
        self
    }

    /// Confine the child with `policy`.
    pub fn sandbox(mut self, policy: Policy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Refuse to launch if the sandbox policy cannot be fully enforced.
    ///
    /// The right setting for running model-generated code. It is off by default
    /// because a developer on an older kernel still needs their build command to
    /// run, and the enforcement level is reported either way.
    pub fn require_confinement(mut self, require: bool) -> Self {
        self.require_confinement = require;
        self
    }

    /// Set resource limits.
    pub fn limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Set the wall-clock timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.limits.timeout = timeout;
        self
    }

    /// Write `data` to the child's standard input, then close it.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// The program this command runs.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// A shell-like rendering, for logs and for the permission prompt.
    ///
    /// This is display only. Nebula never builds a command line by string
    /// concatenation and hands it to a shell — arguments are passed as an
    /// array, so there is nothing for a quoting bug to escape into.
    pub fn display(&self) -> String {
        let mut out = self.program.clone();
        for arg in &self.args {
            out.push(' ');
            if arg.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
                out.push_str(&format!("{arg:?}"));
            } else {
                out.push_str(arg);
            }
        }
        out
    }

    /// Find the executable this command names.
    ///
    /// A program with a path separator in it — `./build.sh`, `target/release/x`
    /// — is relative to the command's working directory, not to whatever
    /// directory the editor happens to be running in. Handing it straight to
    /// `which` resolves it against the wrong place and reports a perfectly
    /// present program as missing.
    fn resolve_program(&self) -> Result<PathBuf> {
        let program = PathBuf::from(&self.program);

        if program.is_absolute() {
            return program
                .is_file()
                .then_some(program)
                .ok_or_else(|| ExecError::NotFound(self.program.clone()));
        }

        if self.program.contains('/') || self.program.contains(std::path::MAIN_SEPARATOR) {
            let base = self.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
            let candidate = base.join(&program);
            return candidate
                .is_file()
                .then_some(candidate)
                .ok_or_else(|| ExecError::NotFound(self.program.clone()));
        }

        // A bare name is looked up on PATH, as a shell would.
        which::which(&self.program).map_err(|_| ExecError::NotFound(self.program.clone()))
    }

    /// Run to completion.
    pub async fn run(self) -> Result<Output> {
        let started = Instant::now();

        if let Some(cwd) = &self.cwd
            && !cwd.is_dir()
        {
            return Err(ExecError::BadWorkingDirectory(cwd.clone()));
        }

        // Resolve the program up front so a typo produces a clear error rather
        // than an opaque ENOENT from the spawn.
        let resolved = self.resolve_program()?;

        // Decide about confinement before spawning anything.
        let enforcement_note = match &self.policy {
            None => None,
            Some(policy) => {
                policy.validate()?;
                let available = nebula_sandbox::is_available();
                if self.require_confinement && !available {
                    return Err(ExecError::SandboxRefused(format!(
                        "no sandbox backend on this system ({})",
                        nebula_sandbox::backend_description()
                    )));
                }
                Some(nebula_sandbox::backend_description())
            }
        };

        // macOS confines by wrapping, not in `pre_exec`. `sandbox_init` compiles
        // a TinyScheme profile and allocates heavily, and the `pre_exec` closure
        // runs in the forked child of a multithreaded runtime, where only
        // async-signal-safe calls are legal. macOS libmalloc detects the
        // violation and aborts: every dynamically linked program in the stress
        // run died on SIGABRT before `main`, whatever the profile contained,
        // which is why widening the read paths and granting `file-map-executable`
        // both changed nothing. `sandbox-exec` applies the identical profile in
        // a fresh single-threaded process after `exec`, where building one is
        // safe. Linux is untouched: Landlock and seccomp are raw syscalls and
        // are legal exactly where they already are.
        #[cfg(target_os = "macos")]
        let (resolved, spawn_args) = match &self.policy {
            Some(policy) => {
                let profile = nebula_sandbox::macos::build_profile(policy)
                    .map_err(|e| ExecError::SandboxRefused(e.to_string()))?;
                let mut args = vec!["-p".to_string(), profile, resolved.display().to_string()];
                args.extend(self.args.iter().cloned());
                (PathBuf::from("/usr/bin/sandbox-exec"), args)
            }
            None => (resolved, self.args.clone()),
        };
        #[cfg(not(target_os = "macos"))]
        let spawn_args = self.args.clone();

        let mut command = tokio::process::Command::new(&resolved);
        command
            .args(&spawn_args)
            .stdin(if self.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Do not let a killed child leave a zombie behind.
            .kill_on_drop(true);

        if !self.inherit_env {
            command.env_clear();
            // A completely empty environment breaks almost every real tool;
            // these are the variables a process legitimately needs to function.
            command.env("PATH", default_path());
            if let Some(home) = std::env::var_os("HOME") {
                command.env("HOME", home);
            }
            command.env("LANG", "C.UTF-8");
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }

        #[cfg(unix)]
        {
            let limits = self.limits.clone();
            // Not on macOS: the wrapper above owns confinement there, and an
            // unused binding would be the only trace left of it.
            #[cfg(not(target_os = "macos"))]
            let policy = self.policy.clone();
            // SAFETY: this closure runs in the forked child between `fork` and
            // `exec`. It performs syscalls only (`setsid`, `setrlimit`,
            // `landlock_*`/`seccomp`), which is the standard way to confine a
            // child before it runs the target binary. Nothing here touches
            // shared state of the parent process.
            unsafe {
                command.pre_exec(move || {
                    // A new session and process group, so a timeout can kill the
                    // whole tree rather than just the direct child.
                    if libc::setsid() == -1 {
                        // Already a group leader is fine; anything else is not
                        // fatal either, it just weakens cleanup.
                        tracing::trace!("setsid failed in child");
                    }
                    limits.apply_to_current_process()?;

                    // macOS is deliberately absent: its policy was applied by
                    // the `sandbox-exec` wrapper chosen above, because
                    // `sandbox_init` is not async-signal-safe and aborts here.
                    #[cfg(not(target_os = "macos"))]
                    if let Some(policy) = &policy {
                        match nebula_sandbox::apply(policy) {
                            Ok(_) => {}
                            Err(err) => {
                                // The child cannot log usefully; failing the
                                // exec is the only safe response, because the
                                // alternative is running unconfined code that
                                // the caller believed was confined.
                                let _ = err;
                                return Err(std::io::Error::other(
                                    "sandbox policy could not be applied",
                                ));
                            }
                        }
                    }
                    Ok(())
                });
            }
        }

        let mut child = command
            .spawn()
            .map_err(|source| ExecError::Spawn { program: self.program.clone(), source })?;

        if let Some(data) = &self.stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            use tokio::io::AsyncWriteExt;
            // A child that exits without reading stdin gives us EPIPE; that is
            // the child's prerogative, not an error in the launch.
            let _ = pipe.write_all(data).await;
            let _ = pipe.shutdown().await;
        }

        let cap = self.limits.max_output_bytes;
        let pid = child.id();

        // The readers run as independent tasks for the whole life of the child.
        // Draining continuously is mandatory, not an optimisation: a child that
        // fills the 64 KiB pipe buffer blocks forever if nobody is reading, and
        // would then hit the timeout instead of finishing.
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdout_task = tokio::spawn(async move {
            match stdout_pipe {
                Some(mut pipe) => read_capped(&mut pipe, cap).await,
                None => Ok((Vec::new(), false)),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr_pipe {
                Some(mut pipe) => read_capped(&mut pipe, cap).await,
                None => Ok((Vec::new(), false)),
            }
        });

        let (timed_out, exit_status) =
            match tokio::time::timeout(self.limits.timeout, child.wait()).await {
                Ok(status) => (false, status),
                Err(_) => {
                    // Kill the group first so grandchildren die too, then reap.
                    kill_process_group(pid);
                    let _ = child.start_kill();
                    (true, child.wait().await)
                }
            };

        // Both pipes close when the process dies, so the readers finish
        // promptly. The bound is a backstop against a grandchild that inherited
        // the pipe and outlived the kill.
        let collect = |joined: std::result::Result<
            std::io::Result<(Vec<u8>, bool)>,
            tokio::task::JoinError,
        >| match joined {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(source)) => Err(ExecError::Io { program: self.program.clone(), source }),
            // A panicked reader task should not lose the exit status.
            Err(_) => Ok((Vec::new(), true)),
        };

        let drain = Duration::from_secs(2);
        let stdout_joined = tokio::time::timeout(drain, stdout_task).await;
        let stderr_joined = tokio::time::timeout(drain, stderr_task).await;

        let (stdout_bytes, stdout_truncated) = match stdout_joined {
            Ok(joined) => collect(joined)?,
            Err(_) => (Vec::new(), true),
        };
        let (stderr_bytes, stderr_truncated) = match stderr_joined {
            Ok(joined) => collect(joined)?,
            Err(_) => (Vec::new(), true),
        };

        let status = if timed_out {
            Status::TimedOut
        } else {
            match exit_status {
                Ok(status) => classify(status),
                Err(source) => {
                    return Err(ExecError::Io { program: self.program.clone(), source });
                }
            }
        };

        Ok(Output {
            status,
            stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
            truncated: stdout_truncated || stderr_truncated,
            duration: started.elapsed(),
            enforcement: enforcement_note,
        })
    }

    /// Run to completion on a temporary runtime.
    ///
    /// For synchronous callers such as the stress harness and the SDK CLI.
    pub fn run_blocking(self) -> Result<Output> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|source| ExecError::Io { program: self.program.clone(), source })?;
        runtime.block_on(self.run())
    }
}

/// Read from `pipe` until EOF or `cap` bytes, reporting whether it was capped.
async fn read_capped<R>(pipe: &mut R, cap: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncReadExt + Unpin,
{
    let mut out = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut truncated = false;

    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if out.len() >= cap {
            // Keep draining so the child does not block on a full pipe, but stop
            // accumulating.
            truncated = true;
            continue;
        }
        let take = read.min(cap - out.len());
        out.extend_from_slice(&buffer[..take]);
        if take < read {
            truncated = true;
        }
    }
    Ok((out, truncated))
}

fn classify(status: std::process::ExitStatus) -> Status {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Status::Signaled(signal);
        }
    }
    Status::Exited(status.code().unwrap_or(-1))
}

/// Kill an entire process group.
///
/// The child called `setsid`, so its PID is its process-group ID and a negative
/// PID reaches every descendant. Killing only the direct child would leave the
/// compiler processes a build spawned still running.
fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    {
        if let Some(pid) = pid {
            // SAFETY: `kill` with a negative PID targets the process group; an
            // invalid or already-dead group simply returns ESRCH.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// A sane `PATH` for a child that does not inherit the environment.
fn default_path() -> String {
    #[cfg(unix)]
    {
        // Include the parent's PATH so toolchains installed under the user's
        // home (rustup, nvm, pyenv) remain reachable — but a policy that denies
        // reading those directories still stops the child using them.
        match std::env::var("PATH") {
            Ok(path) if !path.is_empty() => path,
            _ => "/usr/local/bin:/usr/bin:/bin".to_string(),
        }
    }
    #[cfg(not(unix))]
    {
        std::env::var("PATH").unwrap_or_else(|_| "C:\\Windows\\System32".to_string())
    }
}

/// Which confinement a policy achieved, without running anything.
pub fn probe_enforcement(policy: &Policy) -> Result<Enforcement> {
    policy.validate()?;
    Ok(if nebula_sandbox::is_available() {
        Enforcement::Full { mechanism: nebula_sandbox::backend_description() }
    } else {
        Enforcement::Unsupported { reason: nebula_sandbox::backend_description() }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_sandbox::PolicyBuilder;

    #[tokio::test]
    async fn a_relative_program_resolves_against_the_working_directory() {
        // The regression this guards: `./thing` used to be looked up relative
        // to wherever the editor was started, so a build tool that produced a
        // binary and then ran it reported its own output as missing.
        let dir = tempfile::TempDir::new().unwrap();

        // The script has to be one the platform will actually start. Windows
        // has no shebang line: handing it a `.sh` gets `%1 is not a valid
        // Win32 application` from `CreateProcess`, which says nothing about
        // the resolution this test is here to check.
        #[cfg(unix)]
        let program = {
            use std::os::unix::fs::PermissionsExt;
            let script = dir.path().join("hello.sh");
            std::fs::write(&script, "#!/bin/sh\necho from the working directory\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            "./hello.sh"
        };
        #[cfg(windows)]
        let program = {
            let script = dir.path().join("hello.bat");
            std::fs::write(&script, "@echo off\r\necho from the working directory\r\n").unwrap();
            "./hello.bat"
        };

        let output = Command::new(program).current_dir(dir.path()).run().await.unwrap();

        assert!(output.is_success(), "{}", output.stderr);
        assert!(output.stdout.contains("from the working directory"), "{}", output.stdout);
    }

    #[tokio::test]
    async fn a_relative_program_that_is_not_there_is_still_reported_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let error =
            Command::new("./nothing-here.sh").current_dir(dir.path()).run().await.unwrap_err();
        assert!(matches!(error, ExecError::NotFound(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_absolute_program_still_runs() {
        // Naming the program by absolute path is the point, so the path has to
        // be one that exists on the machine running the test. `/bin/sh` is a
        // Unix fact, and on Windows it produced `NotFound("/bin/sh")` — the
        // resolution behaving correctly about a program that was never there.
        #[cfg(unix)]
        let output = Command::new("/bin/sh").args(["-c", "echo absolute"]).run().await.unwrap();
        #[cfg(windows)]
        let output = {
            // `COMSPEC` is where Windows itself records the command processor;
            // hardcoding `C:\Windows` assumes a system root that installs are
            // free to move.
            let shell = std::env::var("COMSPEC")
                .unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string());
            assert!(PathBuf::from(&shell).is_absolute(), "COMSPEC was not absolute: {shell}");
            Command::new(shell).args(["/C", "echo absolute"]).run().await.unwrap()
        };
        assert!(output.stdout.contains("absolute"), "{output:?}");
    }
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn runs_a_program_and_captures_stdout() {
        let output = Command::new("echo").arg("hello nebula").run().await.unwrap();
        assert!(output.is_success(), "{output:?}");
        assert_eq!(output.stdout.trim(), "hello nebula");
        assert!(output.stderr.is_empty());
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn captures_a_nonzero_exit_code() {
        let output = Command::new("sh").arg("-c").arg("exit 42").run().await.unwrap();
        assert_eq!(output.status, Status::Exited(42));
        assert!(!output.is_success());
    }

    #[tokio::test]
    async fn captures_stderr_separately() {
        let output =
            Command::new("sh").arg("-c").arg("echo out; echo err >&2").run().await.unwrap();
        assert_eq!(output.stdout.trim(), "out");
        assert_eq!(output.stderr.trim(), "err");
        assert!(output.combined().contains("out") && output.combined().contains("err"));
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_clearly() {
        let err = Command::new("nebula-definitely-not-a-real-program").run().await.unwrap_err();
        assert!(matches!(err, ExecError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn stdin_is_delivered() {
        let output = Command::new("cat").stdin("piped input").run().await.unwrap();
        assert_eq!(output.stdout, "piped input");
    }

    #[tokio::test]
    async fn the_working_directory_is_honoured() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("marker.txt"), b"found").unwrap();

        let output =
            Command::new("cat").arg("marker.txt").current_dir(dir.path()).run().await.unwrap();
        assert_eq!(output.stdout, "found");
    }

    #[tokio::test]
    async fn a_bad_working_directory_is_rejected() {
        let err = Command::new("echo")
            .current_dir("/definitely/not/a/directory")
            .run()
            .await
            .unwrap_err();
        assert!(matches!(err, ExecError::BadWorkingDirectory(_)));
    }

    #[tokio::test]
    async fn the_environment_is_not_inherited_by_default() {
        // SAFETY: single-threaded test setup before any child is spawned.
        unsafe { std::env::set_var("NEBULA_SECRET_TOKEN", "super-secret") };

        let output = Command::new("sh")
            .arg("-c")
            .arg("echo \"[${NEBULA_SECRET_TOKEN:-unset}]\"")
            .run()
            .await
            .unwrap();
        assert_eq!(
            output.stdout.trim(),
            "[unset]",
            "the child must not inherit the editor's secrets"
        );

        let inherited = Command::new("sh")
            .arg("-c")
            .arg("echo \"[${NEBULA_SECRET_TOKEN:-unset}]\"")
            .inherit_env(true)
            .run()
            .await
            .unwrap();
        assert_eq!(inherited.stdout.trim(), "[super-secret]");

        unsafe { std::env::remove_var("NEBULA_SECRET_TOKEN") };
    }

    #[tokio::test]
    async fn explicit_environment_variables_reach_the_child() {
        let output = Command::new("sh")
            .arg("-c")
            .arg("echo $MY_VAR")
            .env("MY_VAR", "explicit value")
            .run()
            .await
            .unwrap();
        assert_eq!(output.stdout.trim(), "explicit value");
    }

    #[tokio::test]
    async fn a_hanging_process_is_killed_at_the_timeout() {
        let started = Instant::now();
        let output = Command::new("sleep")
            .arg("60")
            .timeout(Duration::from_millis(300))
            .run()
            .await
            .unwrap();

        assert_eq!(output.status, Status::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout did not actually stop the process"
        );
    }

    #[tokio::test]
    async fn the_whole_process_group_dies_on_timeout() {
        // The shell spawns a grandchild and then waits. Killing only the direct
        // child would leave `sleep` running.
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("grandchild-still-running");
        let script = format!("sh -c 'sleep 30; touch {}' & wait", marker.display());

        let output = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .timeout(Duration::from_millis(300))
            .run()
            .await
            .unwrap();
        assert_eq!(output.status, Status::TimedOut);

        // If the grandchild survived, it would create the marker after 30s.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!marker.exists(), "a grandchild outlived the timeout");
    }

    #[tokio::test]
    async fn oversized_output_is_truncated_rather_than_exhausting_memory() {
        let limits = ResourceLimits::default().max_output(4096).timeout(Duration::from_secs(30));
        let output = Command::new("sh")
            .arg("-c")
            // 5 MB of output against a 4 KB ceiling.
            .arg("yes nebula | head -c 5000000")
            .limits(limits)
            .run()
            .await
            .unwrap();

        assert!(output.truncated, "the truncation flag must be set");
        assert!(
            output.stdout.len() <= 4096,
            "captured {} bytes despite a 4096 byte cap",
            output.stdout.len()
        );
    }

    #[tokio::test]
    async fn a_program_producing_no_output_is_fine() {
        let output = Command::new("true").run().await.unwrap();
        assert!(output.is_success());
        assert!(output.stdout.is_empty());
    }

    #[tokio::test]
    async fn duration_is_measured() {
        let output = Command::new("sleep").arg("0.2").run().await.unwrap();
        assert!(output.duration >= Duration::from_millis(150), "{:?}", output.duration);
    }

    #[test]
    fn the_display_form_quotes_arguments_with_spaces() {
        let command = Command::new("grep").arg("-r").arg("two words").arg("src/");
        let display = command.display();
        assert!(display.starts_with("grep -r "));
        assert!(display.contains("\"two words\""), "{display}");
    }

    #[test]
    fn blocking_execution_works_without_an_ambient_runtime() {
        let output = Command::new("echo").arg("sync").run_blocking().unwrap();
        assert_eq!(output.stdout.trim(), "sync");
    }

    // --- Sandbox enforcement, end to end ---

    /// Whether to skip a test that needs real OS confinement.
    ///
    /// Some kernels (containers, minimal VMs) have no Landlock, and a developer
    /// on such a machine should still get a green test run. But a test that can
    /// skip everywhere is a test that proves nothing, so CI sets
    /// `NEBULA_REQUIRE_SANDBOX=1` and the skip becomes a hard failure there.
    fn skip_without_sandbox() -> bool {
        if nebula_sandbox::is_available() {
            return false;
        }
        let backend = nebula_sandbox::backend_description();
        assert!(
            std::env::var("NEBULA_REQUIRE_SANDBOX").is_err(),
            "NEBULA_REQUIRE_SANDBOX is set but no sandbox backend is available: {backend}"
        );
        eprintln!("skipping: no sandbox backend ({backend})");
        true
    }

    /// The half of a test policy that exists only so the process can start:
    /// the system read list, and exec from the directories the fixtures live
    /// in. Every sandbox test here builds on this and nothing else.
    ///
    /// A helper rather than a list written out at each call site, because a
    /// hand-written `/usr`, `/lib`, `/bin`, `/etc` is a Linux inventory that
    /// omits the dyld shared cache, and a process that cannot reach the cache
    /// dies before `main`. It has now been written out three times and fixed
    /// twice: the copy in `policy.rs`, then the one here — and then the one in
    /// the write test below, which the previous fix walked straight past
    /// because it was looking for this helper rather than for the paths.
    fn startable_policy(label: &str) -> PolicyBuilder {
        let mut builder = Policy::builder().label(label);
        for dir in nebula_sandbox::system_read_directories() {
            builder = builder.read(dir);
        }
        builder.exec("/usr/bin").exec("/bin")
    }

    /// Assert that a denied run was denied *by the sandbox*, not by dying.
    ///
    /// A "denied access is refused" test that only checks for a non-zero exit
    /// proves nothing: a process the loader kills before `main` also exits
    /// non-zero, so the test stays green on a platform where the policy is so
    /// broken that nothing starts at all. That is what happened on macOS —
    /// every confined process died on `SIGABRT` and these tests reported
    /// success throughout.
    ///
    /// There are two ways to never start, and each has its own signature. The
    /// loader killing a process leaves a signal rather than an exit code. The
    /// macOS wrapper failing to hand off leaves an ordinary exit code, but
    /// `sandbox-exec` names itself when it does.
    fn assert_the_process_actually_ran(output: &Output, policy: &Policy) {
        assert!(
            matches!(output.status, Status::Exited(_)),
            "the process was killed before it ran, so this proves nothing about the policy: {}",
            diagnose(output, policy)
        );
        assert!(
            !output.stderr.contains("sandbox-exec:"),
            "the wrapper never reached the program, so this proves nothing about the policy: {}",
            diagnose(output, policy)
        );
    }

    /// A policy allowing `allowed` to be read, and deliberately nothing else
    /// beyond what any process needs to start.
    fn confining_policy(allowed: &std::path::Path) -> Policy {
        startable_policy("test-confinement").read(allowed.to_path_buf()).build()
    }

    /// Everything a sandboxed run reports, plus the profile it was given.
    ///
    /// On macOS a denied process writes nothing to either stream and dies on a
    /// signal, so `output` alone cannot say *which* rule was missing. The
    /// profile is the other half of that evidence, and printing it is the
    /// difference between a failure that names its cause and one that needs a
    /// machine nobody in CI has.
    fn diagnose(output: &Output, policy: &Policy) -> String {
        let profile = if cfg!(target_os = "macos") {
            nebula_sandbox::macos::build_profile(policy)
                .unwrap_or_else(|e| format!("<profile could not be built: {e}>"))
        } else {
            policy.describe()
        };
        format!("{output:?}\n--- policy as applied ---\n{profile}")
    }

    #[tokio::test]
    async fn a_confined_process_can_read_inside_its_policy() {
        if skip_without_sandbox() {
            return;
        }
        let allowed = TempDir::new().unwrap();
        fs::write(allowed.path().join("ok.txt"), b"readable").unwrap();
        let policy = confining_policy(allowed.path());

        let output = Command::new("cat")
            .arg(allowed.path().join("ok.txt").display().to_string())
            .sandbox(policy.clone())
            .timeout(Duration::from_secs(20))
            .run()
            .await
            .unwrap();

        assert!(output.is_success(), "granted read failed: {}", diagnose(&output, &policy));
        assert_eq!(output.stdout, "readable");
    }

    #[tokio::test]
    async fn a_confined_process_cannot_read_outside_its_policy() {
        if skip_without_sandbox() {
            return;
        }
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let secret = forbidden.path().join("secret.txt");
        fs::write(&secret, b"should never be read").unwrap();
        let policy = confining_policy(allowed.path());

        let output = Command::new("cat")
            .arg(secret.display().to_string())
            .sandbox(policy.clone())
            .timeout(Duration::from_secs(20))
            .run()
            .await
            .unwrap();

        assert!(
            !output.is_success(),
            "the sandbox let a process read outside its policy: {}",
            diagnose(&output, &policy)
        );
        assert!(
            !output.stdout.contains("should never be read"),
            "secret content leaked: {}",
            diagnose(&output, &policy)
        );
        assert_the_process_actually_ran(&output, &policy);
    }

    #[tokio::test]
    async fn a_confined_process_cannot_write_outside_its_policy() {
        if skip_without_sandbox() {
            return;
        }
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let target = forbidden.path().join("written.txt");
        let policy = confining_policy(allowed.path());

        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("echo data > {}", target.display()))
            .sandbox(policy.clone())
            .timeout(Duration::from_secs(20))
            .run()
            .await
            .unwrap();

        assert!(
            !output.is_success(),
            "a write outside the policy succeeded: {}",
            diagnose(&output, &policy)
        );
        assert!(!target.exists(), "the file was created despite the policy");
        assert_the_process_actually_ran(&output, &policy);
    }

    #[tokio::test]
    async fn a_confined_process_can_write_where_the_policy_allows() {
        if skip_without_sandbox() {
            return;
        }
        let workspace = TempDir::new().unwrap();
        let target = workspace.path().join("output.txt");

        // The system read list comes from the library, like every other policy
        // here. Spelling it out was what kept this test failing on macOS after
        // the identical list two functions up had already been fixed.
        let policy =
            startable_policy("writable-workspace").write(workspace.path().to_path_buf()).build();

        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("echo data > {}", target.display()))
            .sandbox(policy.clone())
            .timeout(Duration::from_secs(20))
            .run()
            .await
            .unwrap();

        assert!(output.is_success(), "a permitted write failed: {}", diagnose(&output, &policy));
        assert_eq!(fs::read_to_string(&target).unwrap().trim(), "data");
    }

    #[tokio::test]
    async fn the_enforcement_level_is_reported_for_the_audit_log() {
        let dir = TempDir::new().unwrap();
        let output =
            Command::new("true").sandbox(Policy::read_only(dir.path())).run().await.unwrap();
        assert!(output.enforcement.is_some(), "every sandboxed run must record what it achieved");
    }

    #[test]
    fn probing_enforcement_does_not_require_running_anything() {
        let dir = TempDir::new().unwrap();
        let enforcement = probe_enforcement(&Policy::read_only(dir.path())).unwrap();
        // `confines_filesystem`, not `is_confined`: the latter is true whenever
        // seccomp installed a network filter, even on a kernel where Landlock
        // is missing and every file on the machine stays readable.
        assert_eq!(enforcement.confines_filesystem(), nebula_sandbox::is_available());
    }
}
