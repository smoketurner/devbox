//! The Nitro Sandbox VM itself: its Firecracker configuration, stopping it, and
//! the workspace image that carries the guest's repository.
//!
//! Firecracker boots a Nitro Sandbox (a child VM created through the Nitro
//! Enclaves driver) from a config file whose paths are inside the jail's chroot
//! (see [`super::jail`]). The guest has no network device; its only channel to
//! the host is vsock (see [`super::relay`]).
//!
//! The working copy travels as an ext4 image built with `mkfs.ext4 -d` (no root
//! or loop mount needed) and attached as the guest's second drive. After the
//! guest stops, the repository is copied back out with `debugfs rdump`, again
//! without mounting anything on the host.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;

use super::api::{self, VmState};
use super::process::{cmdline_contains, terminate, wait_exit};
use super::store::SandboxMeta;

/// Guest vsock context id (2 is the host; 3 is the first guest id).
const GUEST_CID: u32 = 3;

/// Label the guest mounts its workspace drive by.
const WORKSPACE_LABEL: &str = "sbworkspace";

/// Time budget for building or reading the workspace image.
const IMAGE_TIMEOUT: Duration = Duration::from_mins(10);

/// How long `stop` waits for a guest asked to power off.
const STOP_GRACE: Duration = Duration::from_secs(15);

/// How long a VMM gets between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Files in a sandbox directory (and, for the image and config, the chroot).
pub(crate) const CONSOLE_LOG: &str = "console.log";
pub(crate) const IMAGE: &str = "workspace.img";
pub(crate) const VM_CONFIG: &str = "vm.json";
pub(crate) const VSOCK: &str = "vsock.sock";

/// How [`stop_vmm`] ended the VMM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopOutcome {
    /// It had already exited.
    NotRunning,
    /// The guest powered off when asked.
    Clean,
    /// It was signalled (the guest didn't answer, or didn't power off in time).
    Forced,
}

/// The Firecracker configuration for `meta`. Paths are inside the chroot.
pub(crate) fn vm_config(meta: &SandboxMeta) -> Value {
    json!({
        "boot-source": {
            "kernel_image_path": "/vmlinux",
            // The root drive is read-only and shared by every sandbox;
            // systemd.volatile=overlay gives each guest a writable tmpfs layer.
            "boot_args": format!(
                "console=ttyS0 reboot=k panic=1 systemd.volatile=overlay \
                 devbox.sandbox={} devbox.workspace=LABEL={WORKSPACE_LABEL}",
                meta.name
            ),
        },
        "drives": [
            {
                "drive_id": "rootfs",
                "path_on_host": "/rootfs.ext4",
                "is_root_device": true,
                "is_read_only": true,
            },
            {
                "drive_id": "workspace",
                "path_on_host": format!("/{IMAGE}"),
                "is_root_device": false,
                "is_read_only": false,
            },
        ],
        "machine-config": {
            "vcpu_count": meta.vcpus,
            "mem_size_mib": meta.memory_mib,
        },
        "vsock": {
            "guest_cid": GUEST_CID,
            "uds_path": format!("/{VSOCK}"),
        },
        // Run as a Nitro Sandbox. Direct mode: the threat is the guest, not the
        // claimant, so confidential mode's protection from the parent isn't
        // needed and direct mode's faster I/O is.
        "enclave": { "direct_mode": true },
    })
}

/// Stop the VMM `pid` (identified by `needle`) whose chroot is `vm_dir`,
/// cleanly if the guest cooperates.
///
/// A paused guest is resumed first, then asked to power off through its
/// control port (and, on x86_64, sent Ctrl-Alt-Del). If it is still running
/// after [`STOP_GRACE`], or never accepted the request, it gets SIGTERM and
/// then SIGKILL.
///
/// # Errors
///
/// Returns an error if the VMM is still running after SIGKILL.
pub(crate) async fn stop_vmm(vm_dir: &Path, pid: u32, needle: &str) -> Result<StopOutcome> {
    if !cmdline_contains(pid, needle) {
        return Ok(StopOutcome::NotRunning);
    }
    if matches!(api::vm_state(vm_dir).await, Ok(VmState::Paused)) {
        api::set_paused(vm_dir, false).await.ok();
    }
    let mut asked = api::request_guest_shutdown(vm_dir).await.is_ok();
    if cfg!(target_arch = "x86_64") {
        asked |= api::send_ctrl_alt_del(vm_dir).await.is_ok();
    }
    if asked && wait_exit(pid, needle, STOP_GRACE).await {
        return Ok(StopOutcome::Clean);
    }
    terminate(pid, needle, KILL_GRACE).await?;
    Ok(StopOutcome::Forced)
}

/// Build an ext4 image of `disk_gib` GiB at `<dir>/workspace.img`, populated
/// from `stage`.
///
/// # Errors
///
/// Returns an error if `mkfs.ext4` fails.
pub(crate) async fn build_image(stage: &Path, dir: &Path, disk_gib: u32) -> Result<()> {
    let size = format!("{disk_gib}G");
    run(
        Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", WORKSPACE_LABEL, "-d"])
            .arg(stage)
            .arg(dir.join(IMAGE))
            .arg(&size),
        "mkfs.ext4",
    )
    .await
}

/// Copy `/repo` out of `<dir>/workspace.img` into `<dest>/repo`.
///
/// The guest may have been stopped without unmounting, so the journal is
/// replayed first (`e2fsck -p`; exit 1 means "errors fixed", which is fine).
///
/// # Errors
///
/// Returns an error if the image is missing, unrepairable, or has no `/repo`.
pub(crate) async fn export_repo(dir: &Path, dest: &Path) -> Result<()> {
    let image = dir.join(IMAGE);
    anyhow::ensure!(
        image.is_file(),
        "workspace image {} is missing",
        image.display()
    );
    let status = tokio::time::timeout(
        IMAGE_TIMEOUT,
        Command::new("e2fsck")
            .args(["-p", "-f"])
            .arg(&image)
            .stdout(Stdio::null())
            .status(),
    )
    .await
    .context("e2fsck timed out")?
    .context("run e2fsck")?;
    anyhow::ensure!(
        matches!(status.code(), Some(0 | 1)),
        "e2fsck could not repair the workspace image (exit {:?})",
        status.code()
    );

    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let request = format!("rdump /repo {}", dest.display());
    run(
        Command::new("debugfs").arg("-R").arg(&request).arg(&image),
        "debugfs rdump",
    )
    .await?;
    anyhow::ensure!(
        dest.join("repo").is_dir(),
        "workspace image has no /repo; the guest removed its checkout"
    );
    Ok(())
}

/// Delete `<dir>/workspace.img` once its work has been collected.
///
/// # Errors
///
/// Returns an error on I/O failure other than the image being gone already.
pub(crate) fn discard_image(dir: &Path) -> Result<()> {
    match std::fs::remove_file(dir.join(IMAGE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context("remove workspace image")),
    }
}

/// Run `cmd` under [`IMAGE_TIMEOUT`], failing on a non-zero exit.
async fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let output = tokio::time::timeout(IMAGE_TIMEOUT, cmd.kill_on_drop(true).output())
        .await
        .with_context(|| format!("{what} timed out"))?
        .with_context(|| format!("run {what}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{what} failed (exit {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::path::PathBuf;

    use super::vm_config;
    use crate::sandbox::store::SandboxMeta;

    pub(crate) fn meta() -> SandboxMeta {
        SandboxMeta {
            name: "calm-quilt".to_owned(),
            harness: "claude".to_owned(),
            prompt: "fix it".to_owned(),
            repo: PathBuf::from("/workspace/devbox"),
            base_commit: "abc".to_owned(),
            branch: "agent/calm-quilt".to_owned(),
            created_unix: 0,
            vcpus: 2,
            memory_mib: 4096,
            collected: false,
            worktree: None,
        }
    }

    #[test]
    fn vm_config_is_a_direct_mode_sandbox_with_no_network_and_chroot_paths() {
        let config = vm_config(&meta());

        assert_eq!(config.pointer("/enclave/direct_mode").unwrap(), true);
        assert!(config.get("network-interfaces").is_none());
        assert_eq!(config.pointer("/machine-config/vcpu_count").unwrap(), 2);
        assert_eq!(
            config.pointer("/machine-config/mem_size_mib").unwrap(),
            4096
        );
        assert_eq!(config.pointer("/drives/0/is_read_only").unwrap(), true);
        assert_eq!(config.pointer("/drives/1/is_read_only").unwrap(), false);
        // Every host path is inside the jail's chroot.
        for pointer in [
            "/boot-source/kernel_image_path",
            "/drives/0/path_on_host",
            "/drives/1/path_on_host",
            "/vsock/uds_path",
        ] {
            let path = config.pointer(pointer).unwrap().as_str().unwrap();
            assert!(
                path.starts_with('/') && path.matches('/').count() == 1,
                "{pointer}={path}"
            );
        }
        let boot_args = config
            .pointer("/boot-source/boot_args")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(
            boot_args.contains("devbox.sandbox=calm-quilt")
                && boot_args.contains("devbox.workspace=LABEL=sbworkspace")
                && boot_args.contains("systemd.volatile=overlay"),
            "{boot_args}"
        );
    }

    // Needs /proc: Linux only, like the backend.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stop_signals_a_guest_that_ignores_the_shutdown_request() {
        use super::{StopOutcome, stop_vmm};
        use crate::sandbox::process::wait_started;
        use crate::sandbox::test_support::temp_root;

        let root = temp_root("fc-stop");
        // No API socket and no control port: nothing answers a clean request.
        let mut vmm = tokio::process::Command::new("sh")
            .args(["-c", "trap 'kill $!; exit 0' TERM; sleep 300 & wait", "sh"])
            .args(["--id", "calm-quilt", "x"])
            .spawn()
            .unwrap();
        let pid = vmm.id().unwrap();
        let needle = "--id\0calm-quilt\0";
        assert!(wait_started(pid, needle, std::time::Duration::from_secs(1)).await);

        let outcome = stop_vmm(&root, pid, needle).await.unwrap();
        assert_eq!(outcome, StopOutcome::Forced);
        vmm.wait().await.unwrap();
        assert_eq!(
            stop_vmm(&root, pid, needle).await.unwrap(),
            StopOutcome::NotRunning
        );
    }

    // e2fsprogs (mkfs.ext4 -d, e2fsck, debugfs) ships on the agent's Linux hosts only.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn workspace_image_round_trips_the_repo() {
        use super::{build_image, export_repo};
        use crate::sandbox::test_support::temp_root;

        let root = temp_root("fc-image");
        let stage = root.join("stage");
        std::fs::create_dir_all(stage.join("repo/.git")).unwrap();
        std::fs::write(stage.join("repo/file.txt"), "hello\n").unwrap();
        std::fs::write(stage.join("task.json"), "{}").unwrap();

        build_image(&stage, &root, 1).await.unwrap();
        let out = root.join("out");
        export_repo(&root, &out).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("repo/file.txt")).unwrap(),
            "hello\n"
        );
        assert!(out.join("repo/.git").is_dir());
    }
}
