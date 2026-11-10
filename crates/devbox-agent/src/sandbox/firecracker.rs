//! Nitro Sandbox backend: Firecracker booting a child VM through the Nitro
//! Enclaves driver.
//!
//! The sandbox's working copy travels as an ext4 image built with
//! `mkfs.ext4 -d` (no root or loop mount needed) and attached as the guest's
//! second drive. After the guest stops, the repository is copied back out with
//! `debugfs rdump`, again without mounting anything on the host.
//!
//! The guest has no network device. Its only channel to the host is a vsock
//! device whose host side is a Unix socket in the sandbox directory, so every
//! byte the guest sends out has to pass through something the host runs.
//!
//! Host requirements, checked by [`Firecracker::preflight`]:
//!
//! - `/dev/nitro_enclaves`, present only when the pool launches hosts with
//!   enclave options (`nitro_sandboxes_enabled` in the devbox-infra pool module);
//! - write access to it (the `ne` group);
//! - a sandbox-capable `firecracker`, the guest kernel and the guest root
//!   filesystem (`DEVBOX_SANDBOX_FIRECRACKER`, `DEVBOX_SANDBOX_KERNEL`,
//!   `DEVBOX_SANDBOX_ROOTFS`).

use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use devbox_common::env_non_empty;
use serde_json::{Value, json};
use tokio::process::Command;

use super::store::SandboxMeta;

/// The Nitro Enclaves device the VMM drives.
const ENCLAVE_DEVICE: &str = "/dev/nitro_enclaves";

/// Default locations of the VMM and guest image in the AMI.
const DEFAULT_FIRECRACKER: &str = "firecracker";
const DEFAULT_KERNEL: &str = "/opt/devbox/sandbox/vmlinux";
const DEFAULT_ROOTFS: &str = "/opt/devbox/sandbox/rootfs.ext4";

/// Guest vsock context id (2 is the host; 3 is the first guest id).
const GUEST_CID: u32 = 3;

/// Label the guest mounts its workspace drive by.
const WORKSPACE_LABEL: &str = "sbworkspace";

/// Time budget for building or reading the workspace image.
const IMAGE_TIMEOUT: Duration = Duration::from_mins(10);

/// How long `stop` waits after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(15);

/// Files inside a sandbox directory.
pub(crate) const CONSOLE_LOG: &str = "console.log";
const IMAGE: &str = "workspace.img";
const VM_CONFIG: &str = "vm.json";
const VSOCK: &str = "vsock.sock";

/// Where the VMM and guest image live on this host.
#[derive(Debug, Clone)]
pub(crate) struct Firecracker {
    device: PathBuf,
    firecracker: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
}

impl Firecracker {
    /// Paths from the `DEVBOX_SANDBOX_*` environment, or the AMI defaults.
    pub(crate) fn from_env() -> Self {
        let path = |key: &str, default: &str| {
            env_non_empty(key).map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        Self {
            device: PathBuf::from(ENCLAVE_DEVICE),
            firecracker: path("DEVBOX_SANDBOX_FIRECRACKER", DEFAULT_FIRECRACKER),
            kernel: path("DEVBOX_SANDBOX_KERNEL", DEFAULT_KERNEL),
            rootfs: path("DEVBOX_SANDBOX_ROOTFS", DEFAULT_ROOTFS),
        }
    }

    /// A backend rooted at `root` (`nitro_enclaves`, `firecracker`, `vmlinux`,
    /// `rootfs.ext4` inside it), for tests that stand in for the real host.
    #[cfg(test)]
    pub(crate) fn rooted_at(root: &Path) -> Self {
        Self {
            device: root.join("nitro_enclaves"),
            firecracker: root.join("firecracker"),
            kernel: root.join("vmlinux"),
            rootfs: root.join("rootfs.ext4"),
        }
    }

    /// Check this host can run a sandbox, explaining what to fix when it can't.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first missing requirement.
    pub(crate) fn preflight(&self) -> Result<()> {
        anyhow::ensure!(
            self.device.exists(),
            "Nitro Sandboxes are not available on this host: {} is missing.\n\
             The pool must launch hosts with Nitro Enclaves enabled on an enclave-capable \
             instance type (nitro_sandboxes_enabled in the devbox-infra pool module).",
            self.device.display()
        );
        if let Err(e) = OpenOptions::new().read(true).write(true).open(&self.device) {
            anyhow::bail!(
                "cannot open {} ({e}).\n\
                 Add your account to the `ne` group (sudo usermod -aG ne \"$USER\"), \
                 then log in again.",
                self.device.display()
            );
        }
        anyhow::ensure!(
            resolve_program(&self.firecracker).is_some(),
            "the sandbox VMM ({}) is not installed; this AMI predates sandbox support \
             (set DEVBOX_SANDBOX_FIRECRACKER to override)",
            self.firecracker.display()
        );
        for (what, path, var) in [
            ("guest kernel", &self.kernel, "DEVBOX_SANDBOX_KERNEL"),
            (
                "guest root filesystem",
                &self.rootfs,
                "DEVBOX_SANDBOX_ROOTFS",
            ),
        ] {
            anyhow::ensure!(
                path.is_file(),
                "the sandbox {what} is missing at {}; this AMI predates sandbox support \
                 (set {var} to override)",
                path.display()
            );
        }
        Ok(())
    }

    /// The Firecracker configuration for the sandbox in `dir`.
    pub(crate) fn vm_config(&self, dir: &Path, meta: &SandboxMeta) -> Value {
        json!({
            "boot-source": {
                "kernel_image_path": self.kernel,
                "boot_args": format!(
                    "console=ttyS0 reboot=k panic=1 devbox.sandbox={} devbox.workspace=LABEL={WORKSPACE_LABEL}",
                    meta.name
                ),
            },
            "drives": [
                {
                    "drive_id": "rootfs",
                    "path_on_host": self.rootfs,
                    "is_root_device": true,
                    "is_read_only": true,
                },
                {
                    "drive_id": "workspace",
                    "path_on_host": dir.join(IMAGE),
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
                "uds_path": dir.join(VSOCK),
            },
            // Run as a Nitro Sandbox. Direct mode: the threat is the guest, not the
            // claimant, so confidential mode's protection from the parent isn't
            // needed and direct mode's faster I/O is.
            "enclave": { "direct_mode": true },
        })
    }

    /// Build the workspace image from `stage`, write the VM config, and start the
    /// VMM detached. Returns the VMM's process id.
    ///
    /// # Errors
    ///
    /// Returns an error if the image, config or VMM process can't be created.
    pub(crate) async fn launch(
        &self,
        dir: &Path,
        meta: &SandboxMeta,
        stage: &Path,
        disk_gib: u32,
    ) -> Result<u32> {
        build_image(stage, &dir.join(IMAGE), disk_gib).await?;

        let config_path = dir.join(VM_CONFIG);
        let config =
            serde_json::to_vec_pretty(&self.vm_config(dir, meta)).context("serialize VM config")?;
        std::fs::write(&config_path, config)
            .with_context(|| format!("write {}", config_path.display()))?;

        let console = std::fs::File::create(dir.join(CONSOLE_LOG))
            .with_context(|| format!("create {}", dir.join(CONSOLE_LOG).display()))?;
        let console_err = console
            .try_clone()
            .context("duplicate console log handle")?;
        // Its own process group, so the VMM outlives this command and a Ctrl-C in
        // the claimant's terminal doesn't reach it.
        let child = std::process::Command::new(&self.firecracker)
            .arg("--no-api")
            .arg("--config-file")
            .arg(&config_path)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(console)
            .stderr(console_err)
            .process_group(0)
            .spawn()
            .with_context(|| format!("start {}", self.firecracker.display()))?;
        Ok(child.id())
    }

    /// Whether the VMM for `meta` is still running.
    pub(crate) fn is_running(dir: &Path, meta: &SandboxMeta) -> bool {
        let Some(pid) = meta.pid else {
            return false;
        };
        // Match on the config path too, so a recycled pid isn't mistaken for ours.
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
            let needle = dir.join(VM_CONFIG);
            let needle = needle.to_string_lossy();
            String::from_utf8_lossy(&cmdline).contains(needle.as_ref())
        })
    }

    /// Stop the VMM: SIGTERM, then SIGKILL after [`STOP_GRACE`].
    ///
    /// # Errors
    ///
    /// Returns an error if the VMM is still running after SIGKILL.
    pub(crate) async fn stop(dir: &Path, meta: &SandboxMeta) -> Result<()> {
        let Some(pid) = meta.pid else {
            return Ok(());
        };
        for (signal, wait) in [("-TERM", STOP_GRACE), ("-KILL", Duration::from_secs(5))] {
            if !Self::is_running(dir, meta) {
                return Ok(());
            }
            // Best effort: the process may exit between the check and the signal.
            let _status = Command::new("kill")
                .arg(signal)
                .arg(pid.to_string())
                .status()
                .await;
            let deadline = tokio::time::Instant::now()
                .checked_add(wait)
                .unwrap_or_else(tokio::time::Instant::now);
            while Self::is_running(dir, meta) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        anyhow::ensure!(
            !Self::is_running(dir, meta),
            "sandbox {} (pid {pid}) did not stop",
            meta.name
        );
        Ok(())
    }

    /// Copy the guest's repository out of the workspace image into `dest`.
    ///
    /// # Errors
    ///
    /// Returns an error if the image is missing or can't be read.
    pub(crate) async fn export_repo(dir: &Path, dest: &Path) -> Result<()> {
        export_image(&dir.join(IMAGE), dest).await
    }

    /// Delete the workspace image once its work has been collected.
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
}

/// Build an ext4 image of `disk_gib` GiB at `image`, populated from `stage`.
async fn build_image(stage: &Path, image: &Path, disk_gib: u32) -> Result<()> {
    let size = format!("{disk_gib}G");
    run(
        Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", WORKSPACE_LABEL, "-d"])
            .arg(stage)
            .arg(image)
            .arg(&size),
        "mkfs.ext4",
    )
    .await
}

/// Copy `/repo` out of the ext4 image at `image` into `dest` (created).
///
/// The guest may have been stopped without unmounting, so the journal is
/// replayed first (`e2fsck -p`; exit 1 means "errors fixed", which is fine).
async fn export_image(image: &Path, dest: &Path) -> Result<()> {
    anyhow::ensure!(
        image.is_file(),
        "workspace image {} is missing",
        image.display()
    );
    let status = tokio::time::timeout(
        IMAGE_TIMEOUT,
        Command::new("e2fsck")
            .args(["-p", "-f"])
            .arg(image)
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
        Command::new("debugfs").arg("-R").arg(&request).arg(image),
        "debugfs rdump",
    )
    .await?;
    let repo = dest.join("repo");
    anyhow::ensure!(
        repo.is_dir(),
        "workspace image has no /repo; the guest removed its checkout"
    );
    Ok(())
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

/// `program` itself if it is a path, else its first match on `PATH`.
fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use std::path::PathBuf;

    use super::{Firecracker, resolve_program};
    #[cfg(target_os = "linux")]
    use super::{build_image, export_image};
    use crate::sandbox::store::SandboxMeta;
    use crate::sandbox::test_support::temp_root;

    fn backend(root: &std::path::Path) -> Firecracker {
        Firecracker::rooted_at(root)
    }

    fn meta() -> SandboxMeta {
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
            pid: None,
            collected: false,
            worktree: None,
        }
    }

    #[test]
    fn preflight_explains_each_missing_requirement_in_order() {
        let root = temp_root("fc-preflight");
        let fc = backend(&root);
        let msg = |fc: &Firecracker| format!("{:#}", fc.preflight().unwrap_err());

        assert!(msg(&fc).contains("nitro_sandboxes_enabled"), "{}", msg(&fc));
        std::fs::write(root.join("nitro_enclaves"), "").unwrap();
        assert!(msg(&fc).contains("VMM"), "{}", msg(&fc));
        std::fs::write(root.join("firecracker"), "").unwrap();
        assert!(msg(&fc).contains("guest kernel"), "{}", msg(&fc));
        std::fs::write(root.join("vmlinux"), "").unwrap();
        assert!(msg(&fc).contains("guest root filesystem"), "{}", msg(&fc));
        std::fs::write(root.join("rootfs.ext4"), "").unwrap();
        fc.preflight().unwrap();
    }

    #[test]
    fn vm_config_is_a_direct_mode_sandbox_with_no_network() {
        let root = temp_root("fc-config");
        let config = backend(&root).vm_config(&root, &meta());

        assert_eq!(config.pointer("/enclave/direct_mode").unwrap(), true);
        assert!(config.get("network-interfaces").is_none());
        assert_eq!(config.pointer("/machine-config/vcpu_count").unwrap(), 2);
        assert_eq!(
            config.pointer("/machine-config/mem_size_mib").unwrap(),
            4096
        );
        assert_eq!(config.pointer("/drives/0/is_read_only").unwrap(), true);
        assert_eq!(config.pointer("/drives/1/is_read_only").unwrap(), false);
        let boot_args = config
            .pointer("/boot-source/boot_args")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(
            boot_args.contains("devbox.sandbox=calm-quilt"),
            "{boot_args}"
        );
    }

    #[test]
    fn resolve_program_handles_paths_and_path_lookup() {
        assert!(resolve_program(std::path::Path::new("sh")).is_some());
        assert!(resolve_program(std::path::Path::new("/bin/sh")).is_some());
        assert!(resolve_program(std::path::Path::new("no-such-devbox-binary")).is_none());
    }

    // e2fsprogs (mkfs.ext4 -d, e2fsck, debugfs) ships on the agent's Linux hosts only.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn workspace_image_round_trips_the_repo() {
        let root = temp_root("fc-image");
        let stage = root.join("stage");
        std::fs::create_dir_all(stage.join("repo/.git")).unwrap();
        std::fs::write(stage.join("repo/file.txt"), "hello\n").unwrap();
        std::fs::write(stage.join("task.json"), "{}").unwrap();

        let image = root.join("workspace.img");
        build_image(&stage, &image, 1).await.unwrap();
        let out = root.join("out");
        export_image(&image, &out).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("repo/file.txt")).unwrap(),
            "hello\n"
        );
        assert!(out.join("repo/.git").is_dir());
    }
}
