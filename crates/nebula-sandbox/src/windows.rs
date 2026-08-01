//! Windows confinement via Job Objects.
//!
//! Windows has no unprivileged equivalent of Landlock. Filesystem scoping needs
//! an AppContainer with capability SIDs, which requires the child to be
//! launched through `CreateProcessAsUser` with a prepared token — a launcher
//! concern rather than something a process can do to itself.
//!
//! What *is* available in-process is a Job Object: a kernel object that caps
//! memory, CPU time, and process count, and that kills the whole tree when
//! closed. That is what this module provides, and [`apply`] reports honestly
//! that filesystem scoping is not enforced so callers can decide whether to
//! proceed.

use crate::policy::Policy;
use crate::{Enforcement, Result, SandboxError};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Apply what confinement Windows offers in-process.
///
/// Always returns [`Enforcement::Partial`] at best: the filesystem is not
/// scoped, and saying otherwise would be a lie the caller would act on.
pub fn apply(policy: &Policy) -> Result<Enforcement> {
    policy.validate()?;

    let job = create_job_object(DEFAULT_MEMORY_LIMIT, DEFAULT_PROCESS_LIMIT)?;
    // The job handle is intentionally leaked: closing it would terminate the
    // process tree it governs, which is exactly what we do not want while the
    // tool is still running. The kernel reclaims it at process exit.
    std::mem::forget(job);

    let mut missing = vec!["filesystem scoping (requires an AppContainer launcher)".to_string()];
    if policy.network == crate::NetworkAccess::Denied {
        missing.push("network denial (requires a Windows Filtering Platform rule)".to_string());
    }

    Ok(Enforcement::Partial {
        mechanism: "Windows Job Object".to_string(),
        // A Job Object caps memory and processes; it scopes no paths at all.
        filesystem_scoped: false,
        missing,
    })
}

/// Default memory ceiling for a sandboxed tool: 2 GiB.
const DEFAULT_MEMORY_LIMIT: usize = 2 * 1024 * 1024 * 1024;

/// Default cap on processes in the job.
const DEFAULT_PROCESS_LIMIT: u32 = 64;

/// An owned Job Object handle that closes on drop.
pub struct JobObject(HANDLE);

impl Drop for JobObject {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from `CreateJobObjectW` and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Create a Job Object with resource limits and assign the current process.
pub fn create_job_object(memory_limit: usize, process_limit: u32) -> Result<JobObject> {
    // SAFETY: creating an unnamed job object with default security.
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(SandboxError::ApplyFailed(format!(
            "CreateJobObject failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let job = JobObject(handle);

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        // Killing the tree when the job handle closes is what stops an orphaned
        // build process outliving the editor.
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    limits.JobMemoryLimit = memory_limit;
    limits.BasicLimitInformation.ActiveProcessLimit = process_limit;

    // SAFETY: `limits` is a correctly-sized, fully-initialised structure of the
    // type the information class expects.
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(SandboxError::ApplyFailed(format!(
            "SetInformationJobObject failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    // SAFETY: a pseudo-handle to the current process is always valid.
    let assigned = unsafe { AssignProcessToJobObject(job.0, GetCurrentProcess()) };
    if assigned == 0 {
        return Err(SandboxError::ApplyFailed(format!(
            "AssignProcessToJobObject failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_never_claims_full_enforcement() {
        // The whole point of the honest-reporting design: a caller must be able
        // to tell that the filesystem is not scoped here.
        let policy = Policy::read_only("C:\\Users");
        match apply(&policy) {
            Ok(enforcement) => {
                assert!(!enforcement.is_full(), "Windows cannot scope the filesystem in-process");
                assert!(enforcement.summary().contains("filesystem scoping"));
            }
            Err(_) => {
                // Job object creation can fail under a restrictive parent job;
                // an error is still an honest outcome.
            }
        }
    }

    #[test]
    fn an_invalid_policy_is_rejected() {
        let policy =
            Policy { read_paths: vec![std::path::PathBuf::from("relative")], ..Policy::deny_all() };
        assert!(apply(&policy).is_err());
    }
}
