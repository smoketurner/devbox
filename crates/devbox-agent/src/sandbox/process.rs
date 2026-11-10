//! Tracking and stopping the detached processes a sandbox owns (the VMM and
//! its relay) by pid, without trusting a pid alone.

use std::time::Duration;

use anyhow::Result;
use tokio::process::Command;

/// Whether `pid` is alive and its command line contains `needle`.
///
/// Matching on the command line too means a recycled pid belonging to some
/// other process is never mistaken for ours. Command-line arguments are
/// NUL-separated in `/proc`, so `needle` may span several arguments.
pub(crate) fn cmdline_contains(pid: u32, needle: &str) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(needle))
}

/// Wait up to `timeout` for the process `needle` identifies to exit. Returns
/// whether it did.
pub(crate) async fn wait_exit(pid: u32, needle: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(tokio::time::Instant::now);
    while cmdline_contains(pid, needle) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}

/// Stop `pid` (if it is still the process `needle` identifies): SIGTERM, wait up
/// to `grace`, then SIGKILL.
///
/// # Errors
///
/// Returns an error if the process is still running after SIGKILL.
pub(crate) async fn terminate(pid: u32, needle: &str, grace: Duration) -> Result<()> {
    for (signal, wait) in [("-TERM", grace), ("-KILL", Duration::from_secs(5))] {
        if !cmdline_contains(pid, needle) {
            return Ok(());
        }
        // Best effort: the process may exit between the check and the signal.
        let _status = Command::new("kill")
            .arg(signal)
            .arg(pid.to_string())
            .status()
            .await;
        wait_exit(pid, needle, wait).await;
    }
    anyhow::ensure!(!cmdline_contains(pid, needle), "process {pid} did not stop");
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::time::Duration;

    use super::{cmdline_contains, terminate};

    #[tokio::test]
    async fn terminate_stops_only_the_matching_process() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("37.5")
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        // Until the child has exec'd, /proc shows the test binary's command line.
        for _ in 0..100 {
            if cmdline_contains(pid, "sleep\u{0}37.5") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(cmdline_contains(pid, "sleep\u{0}37.5"));
        assert!(!cmdline_contains(pid, "something-else"));

        // A non-matching needle leaves the process alone.
        terminate(pid, "something-else", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(cmdline_contains(pid, "37.5"));

        terminate(pid, "sleep\u{0}37.5", Duration::from_secs(5))
            .await
            .unwrap();
        // Reap the zombie so /proc no longer lists it, then check it's gone.
        child.wait().await.unwrap();
        assert!(!cmdline_contains(pid, "37.5"));
    }
}
