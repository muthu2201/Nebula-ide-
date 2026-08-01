//! A confined process must still be able to open `/dev/null`.
//!
//! This is an integration test rather than a unit test because Landlock's
//! `restrict_self` confines the calling process irreversibly — running it
//! in-process would sandbox the test runner itself and break every test that
//! came after. So the work happens in a re-executed child.
//!
//! The bug this guards against was invisible in local development and only
//! appeared in CI: on a machine where Landlock is unavailable the sandbox
//! degrades to unenforced, every program runs fine, and a missing `/dev/null`
//! rule looks exactly like a working one. On a real kernel `go build` fails
//! with "error obtaining buildID for go tool compile: open /dev/null:
//! permission denied".

use std::fs::File;
use std::io::Read;
use std::process::Command;

/// Set in the child to tell it which half of the test to run.
const ROLE: &str = "NEBULA_SANDBOX_DEVICE_TEST_ROLE";

fn main_child(role: &str) -> ! {
    let policy = nebula_sandbox::Policy::deny_all();
    let enforcement = match nebula_sandbox::apply(&policy) {
        Ok(enforcement) => enforcement,
        Err(error) => {
            eprintln!("could not apply the policy: {error}");
            std::process::exit(3);
        }
    };

    // Without filesystem scoping the checks below would pass for the wrong
    // reason. Skip explicitly rather than claiming a pass.
    if !enforcement.confines_filesystem() {
        std::process::exit(64);
    }

    match role {
        "read_null" => {
            let mut buf = String::new();
            match File::open("/dev/null").and_then(|mut f| f.read_to_string(&mut buf)) {
                Ok(_) => std::process::exit(0),
                Err(error) => {
                    eprintln!("/dev/null was denied: {error}");
                    std::process::exit(1);
                }
            }
        }
        "read_secret" => {
            // The counter-check: a path the policy never granted must still be
            // refused, or the test above proves nothing about the sandbox.
            match File::open("/etc/hostname") {
                Ok(_) => std::process::exit(1),
                Err(_) => std::process::exit(0),
            }
        }
        other => {
            eprintln!("unknown role {other}");
            std::process::exit(3);
        }
    }
}

/// If this process *is* the child, do the child's work and never return.
///
/// Called from both tests: whichever runs first does the work, and the role in
/// the environment — not the caller — decides what that work is.
fn dispatch_if_child() {
    if let Ok(role) = std::env::var(ROLE) {
        main_child(&role);
    }
}

fn run(role: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .env(ROLE, role)
        .output()
        .expect("could not re-execute the test binary")
}

#[test]
fn a_confined_process_can_still_open_dev_null() {
    dispatch_if_child();

    let output = run("read_null");
    match output.status.code() {
        Some(64) => eprintln!("skipped: this kernel does not enforce Landlock"),
        Some(0) => {}
        _ => panic!(
            "a confined process could not read /dev/null:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

#[test]
fn the_sandbox_still_denies_what_it_should() {
    dispatch_if_child();

    let output = run("read_secret");
    match output.status.code() {
        Some(64) => eprintln!("skipped: this kernel does not enforce Landlock"),
        Some(0) => {}
        _ => panic!(
            "an ungranted path was readable, so the sandbox is not enforcing:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}
