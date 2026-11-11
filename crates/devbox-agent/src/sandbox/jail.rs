//! Running a sandbox's VMM under Firecracker's `jailer`.
//!
//! The VMM is the code that parses device I/O from a hostile guest, so it runs
//! as a dedicated unprivileged uid inside a chroot that holds nothing but what
//! it needs: the guest kernel and initramfs, the read-only root filesystem, the workspace
//! image, the Nitro Enclaves device node, and its sockets. A guest that escapes
//! into the VMM lands there, not in the claimant's session or the daemon.
//!
//! Layout, under the claimant's sandbox directory:
//!
//! ```text
//! <dir>/j/<vmm>/<name>/root/        the chroot (jailer's --chroot-base-dir is <dir>/j)
//!     vmlinuz  initrd.img           linked from the AMI if on the same filesystem, else copied
//!     rootfs.ext4                   likewise
//!     workspace.img                 hard link to <dir>/workspace.img
//!     dev/nitro_enclaves            device node, for the VMM's uid
//!     vm.json  api.sock  vsock.sock  vsock.sock_<port> (the relay's)
//! ```
//!
//! Everything here runs in the root daemon ([`super::daemon`]). Each running
//! sandbox gets its own uid from a reserved range, so sandboxes can't reach
//! each other's files either.

use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use devbox_common::env_non_empty;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::api::API_SOCKET;
use super::firecracker::{CONSOLE_LOG, IMAGE, INITRD, KERNEL, VM_CONFIG, vm_config};
use super::process::cmdline_contains;
use super::store::SandboxMeta;

/// The Nitro Enclaves device the VMM drives.
const ENCLAVE_DEVICE: &str = "/dev/nitro_enclaves";

/// Default locations in the AMI.
const DEFAULT_JAILER: &str = "jailer";
const DEFAULT_FIRECRACKER: &str = "firecracker";
/// The host's own AL2023 kernel (an EFI-stub image), copied in by the AMI.
const DEFAULT_KERNEL: &str = "/opt/devbox/sandbox/vmlinuz";
/// Loads the virtio-PCI, block, vsock and NSM modules the root drive needs;
/// the AL2023 kernel builds them as modules.
const DEFAULT_INITRD: &str = "/opt/devbox/sandbox/initrd.img";
const DEFAULT_ROOTFS: &str = "/opt/devbox/sandbox/rootfs.ext4";

/// Reserved uid range for jailed VMMs (one uid per running sandbox).
const DEFAULT_UID_BASE: u32 = 300_000;
const DEFAULT_UID_COUNT: u32 = 1_024;

/// Directory under a sandbox directory that jailer builds its chroot in. Short,
/// because the chroot path is part of every vsock socket path.
const JAIL_DIR: &str = "j";

/// Daemon-owned record of a launched sandbox, next to the claimant's `meta.json`.
const RUNTIME: &str = "runtime.json";

/// What the daemon knows about a launched sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Runtime {
    /// The VMM's pid (jailer execs the VMM in place).
    pub(crate) pid: u32,
    /// The uid (and gid) the VMM runs as.
    pub(crate) jail_uid: u32,
}

/// Where the jailer, VMM and guest image live, and the uids jailed VMMs use.
#[derive(Debug, Clone)]
pub(crate) struct Jail {
    jailer: PathBuf,
    firecracker: PathBuf,
    kernel: PathBuf,
    initrd: PathBuf,
    rootfs: PathBuf,
    device: PathBuf,
    uid_base: u32,
    uid_count: u32,
    /// The gid jailed VMMs run as; `None` means "the same number as the uid".
    gid: Option<u32>,
}

impl Jail {
    /// Paths and uid range from the `DEVBOX_SANDBOX_*` environment, or the AMI defaults.
    pub(crate) fn from_env() -> Self {
        let path = |key: &str, default: &str| {
            env_non_empty(key).map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        let number = |key: &str, default: u32| {
            env_non_empty(key)
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        Self {
            jailer: path("DEVBOX_SANDBOX_JAILER", DEFAULT_JAILER),
            firecracker: path("DEVBOX_SANDBOX_FIRECRACKER", DEFAULT_FIRECRACKER),
            kernel: path("DEVBOX_SANDBOX_KERNEL", DEFAULT_KERNEL),
            initrd: path("DEVBOX_SANDBOX_INITRD", DEFAULT_INITRD),
            rootfs: path("DEVBOX_SANDBOX_ROOTFS", DEFAULT_ROOTFS),
            device: PathBuf::from(ENCLAVE_DEVICE),
            uid_base: number("DEVBOX_SANDBOX_JAIL_UID_BASE", DEFAULT_UID_BASE),
            uid_count: number("DEVBOX_SANDBOX_JAIL_UID_COUNT", DEFAULT_UID_COUNT),
            gid: None,
        }
    }

    /// A jail whose binaries and images sit in `root` (`jailer`, `firecracker`,
    /// `vmlinuz`, `initrd.img`, `rootfs.ext4`, `nitro_enclaves`), using only `uid` and `gid`:
    /// for tests standing in for the host.
    #[cfg(test)]
    pub(crate) fn rooted_at(root: &Path, uid: u32, gid: u32) -> Self {
        Self {
            jailer: root.join("jailer"),
            firecracker: root.join("firecracker"),
            kernel: root.join("vmlinuz"),
            initrd: root.join("initrd.img"),
            rootfs: root.join("rootfs.ext4"),
            device: root.join("nitro_enclaves"),
            uid_base: uid,
            uid_count: 1,
            gid: Some(gid),
        }
    }

    /// Check this host can run a jailed sandbox, naming the first thing missing.
    ///
    /// # Errors
    ///
    /// Returns an error describing the missing requirement and its fix.
    pub(crate) fn preflight(&self) -> Result<()> {
        anyhow::ensure!(
            self.device.exists(),
            "Nitro Sandboxes are not available on this host: {} is missing.\n\
             The pool must launch hosts with Nitro Enclaves enabled on an enclave-capable \
             instance type (nitro_sandboxes_enabled in the devbox-infra pool module).",
            self.device.display()
        );
        for (what, program, var) in [
            ("VMM", &self.firecracker, "DEVBOX_SANDBOX_FIRECRACKER"),
            ("jailer", &self.jailer, "DEVBOX_SANDBOX_JAILER"),
        ] {
            anyhow::ensure!(
                resolve_program(program).is_some(),
                "the sandbox {what} ({}) is not installed; this AMI predates sandbox support \
                 (set {var} to override)",
                program.display()
            );
        }
        for (what, path, var) in [
            ("guest kernel", &self.kernel, "DEVBOX_SANDBOX_KERNEL"),
            ("guest initramfs", &self.initrd, "DEVBOX_SANDBOX_INITRD"),
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

    /// The chroot the VMM for sandbox `name` in `dir` runs in. Firecracker's API
    /// socket, its vsock socket and the relay's sockets all live here.
    pub(crate) fn vm_dir(&self, dir: &Path, name: &str) -> PathBuf {
        vm_dir_for(dir, &self.vmm_name(), name)
    }

    /// The gid a VMM running as `uid` gets.
    pub(crate) fn gid_for(&self, uid: u32) -> u32 {
        self.gid.unwrap_or(uid)
    }

    /// The VMM binary's file name, which jailer uses in the chroot path.
    fn vmm_name(&self) -> String {
        self.firecracker.file_name().map_or_else(
            || DEFAULT_FIRECRACKER.to_owned(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// The lowest uid in the reserved range that no live sandbox in `taken` uses.
    ///
    /// # Errors
    ///
    /// Returns an error when every uid in the range is in use.
    pub(crate) fn free_uid(&self, taken: &[u32]) -> Result<u32> {
        (0..self.uid_count)
            .filter_map(|offset| self.uid_base.checked_add(offset))
            .find(|uid| !taken.contains(uid))
            .with_context(|| {
                format!(
                    "all {} sandbox uids are in use; stop a sandbox first",
                    self.uid_count
                )
            })
    }

    /// Build the chroot for `meta` in `dir`, owned by `jail_uid`. Returns the
    /// chroot path.
    ///
    /// # Errors
    ///
    /// Returns an error if a file can't be placed or the device node created.
    pub(crate) async fn prepare(
        &self,
        dir: &Path,
        meta: &SandboxMeta,
        jail_uid: u32,
    ) -> Result<PathBuf> {
        let base = dir.join(JAIL_DIR);
        remove_jail_dir(&base)?;
        let root = self.vm_dir(dir, &meta.name);
        std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;

        link_or_copy(&self.kernel, &root.join(KERNEL)).await?;
        link_or_copy(&self.initrd, &root.join(INITRD)).await?;
        link_or_copy(&self.rootfs, &root.join("rootfs.ext4")).await?;
        std::fs::hard_link(dir.join(IMAGE), root.join(IMAGE))
            .context("link the workspace image into the jail")?;
        self.make_device_node(&root).await?;

        let config = serde_json::to_vec_pretty(&vm_config(meta)).context("serialize VM config")?;
        std::fs::write(root.join(VM_CONFIG), config).context("write vm.json")?;

        // The VMM opens all of these as its own uid. Nothing above `root` is
        // given to it; jailer chroots into `root` before dropping privileges.
        chown_tree(&root, jail_uid, self.gid_for(jail_uid))?;
        Ok(root)
    }

    /// Recreate the host's Nitro Enclaves device inside the chroot. Skipped
    /// when the configured device isn't a character device (tests).
    async fn make_device_node(&self, root: &Path) -> Result<()> {
        use std::os::unix::fs::FileTypeExt;
        let meta = std::fs::metadata(&self.device)
            .with_context(|| format!("stat {}", self.device.display()))?;
        if !meta.file_type().is_char_device() {
            return Ok(());
        }
        let (major, minor) = dev_numbers(meta.rdev());
        let dev = root.join("dev");
        std::fs::create_dir_all(&dev).context("create the jail's /dev")?;
        let node = dev.join("nitro_enclaves");
        let status = Command::new("mknod")
            .arg(&node)
            .args(["c", &major.to_string(), &minor.to_string()])
            .status()
            .await
            .context("run mknod")?;
        anyhow::ensure!(status.success(), "mknod {} failed", node.display());
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o600))
            .context("restrict the jail's device node")
    }

    /// Start the jailed VMM for sandbox `name` in `dir`, logging its console to
    /// `<dir>/console.log`. Returns its pid; a task reaps it when it exits.
    ///
    /// # Errors
    ///
    /// Returns an error if jailer can't be started.
    pub(crate) fn spawn(&self, dir: &Path, name: &str, jail_uid: u32) -> Result<u32> {
        let log_path = dir.join(CONSOLE_LOG);
        let console = std::fs::File::create(&log_path)
            .with_context(|| format!("create {}", log_path.display()))?;
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o644))
            .context("make the console log readable")?;
        let console_err = console
            .try_clone()
            .context("duplicate console log handle")?;
        let uid = jail_uid.to_string();
        let gid = self.gid_for(jail_uid).to_string();
        let mut child = Command::new(&self.jailer)
            .args(["--id", name, "--exec-file"])
            .arg(&self.firecracker)
            .args(["--uid", &uid, "--gid", &gid, "--chroot-base-dir"])
            .arg(dir.join(JAIL_DIR))
            // Arguments for the VMM; paths are inside the chroot.
            .args(["--", "--api-sock", &format!("/{API_SOCKET}")])
            .args(["--config-file", &format!("/{VM_CONFIG}")])
            .stdin(Stdio::null())
            .stdout(console)
            .stderr(console_err)
            // Its own process group, so a daemon restart doesn't take it down.
            .process_group(0)
            .spawn()
            .with_context(|| format!("start {}", self.jailer.display()))?;
        let pid = child.id().context("jailer exited at once")?;
        tokio::spawn(async move {
            // Reap it, so a finished VMM doesn't linger as a zombie.
            let _status = child.wait().await;
        });
        Ok(pid)
    }

    /// Whether the VMM recorded in `runtime` for `name` is still running.
    pub(crate) fn is_running(name: &str, runtime: &Runtime) -> bool {
        cmdline_contains(runtime.pid, &needle(name))
    }
}

/// The chroot path for sandbox `name` in `dir` when the VMM binary is called
/// `vmm`. The CLI uses the default name to check socket path lengths early.
pub(crate) fn vm_dir_for(dir: &Path, vmm: &str, name: &str) -> PathBuf {
    dir.join(JAIL_DIR).join(vmm).join(name).join("root")
}

/// The chroot path for sandbox `name` in `dir` with the default VMM binary.
pub(crate) fn default_vm_dir(dir: &Path, name: &str) -> PathBuf {
    vm_dir_for(dir, DEFAULT_FIRECRACKER, name)
}

/// The command-line fragment identifying sandbox `name`'s jailer or VMM
/// (jailer passes `--id <name>` on to the VMM it execs).
pub(crate) fn needle(name: &str) -> String {
    format!("--id\0{name}\0")
}

/// Load the daemon's record for the sandbox in `dir`, if it was launched.
pub(crate) fn load_runtime(dir: &Path) -> Option<Runtime> {
    let raw = std::fs::read(dir.join(RUNTIME)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Persist the daemon's record for the sandbox in `dir`.
///
/// # Errors
///
/// Returns an error on I/O failure.
pub(crate) fn save_runtime(dir: &Path, runtime: &Runtime) -> Result<()> {
    let json = serde_json::to_vec(runtime).context("serialize runtime record")?;
    std::fs::write(dir.join(RUNTIME), json).context("write runtime.json")
}

/// Undo [`Jail::prepare`]: give the workspace image back to `owner` (uid, gid),
/// delete the chroot, and forget the runtime record. Safe to repeat.
///
/// # Errors
///
/// Returns an error if the image can't be handed back or the chroot removed.
pub(crate) fn teardown(dir: &Path, owner: (u32, u32)) -> Result<()> {
    let image = dir.join(IMAGE);
    if image.exists() {
        chown(&image, Some(owner.0), Some(owner.1))
            .context("give the workspace image back to its owner")?;
    }
    remove_jail_dir(&dir.join(JAIL_DIR))?;
    match std::fs::remove_file(dir.join(RUNTIME)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context("remove runtime.json")),
    }
}

/// Remove a jail directory (never following a symlink in its place).
fn remove_jail_dir(base: &Path) -> Result<()> {
    match std::fs::symlink_metadata(base) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("{} is a symlink; refusing to use it", base.display())
        }
        Ok(_) => {
            std::fs::remove_dir_all(base).with_context(|| format!("remove {}", base.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("stat {}", base.display()))),
    }
}

/// Hard-link `from` to `to`, or copy it (keeping holes) when they are on
/// different filesystems.
async fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    if std::fs::hard_link(from, to).is_ok() {
        return Ok(());
    }
    let status = Command::new("cp")
        .args(["--sparse=always", "--"])
        .arg(from)
        .arg(to)
        .status()
        .await
        .context("run cp")?;
    anyhow::ensure!(
        status.success(),
        "copy {} into the jail failed",
        from.display()
    );
    Ok(())
}

/// Give `root` and everything in it to `uid`:`gid`.
fn chown_tree(root: &Path, uid: u32, gid: u32) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        std::os::unix::fs::lchown(&path, Some(uid), Some(gid))
            .with_context(|| format!("chown {}", path.display()))?;
        if std::fs::symlink_metadata(&path)?.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}

/// Linux `major`/`minor` of a device number (glibc's `gnu_dev_major`/`minor`).
fn dev_numbers(rdev: u64) -> (u64, u64) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    (major, minor)
}

/// `program` itself if it is a path, else its first match on `PATH`.
pub(crate) fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// The uid and gid that own `path`.
///
/// # Errors
///
/// Returns an error if `path` can't be read.
pub(crate) fn owner_of(path: &Path) -> Result<(u32, u32)> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    Ok((meta.uid(), meta.gid()))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::{Jail, default_vm_dir, dev_numbers, needle, resolve_program};
    use crate::sandbox::test_support::temp_root;

    #[test]
    fn preflight_explains_each_missing_requirement_in_order() {
        let root = temp_root("jail-preflight");
        let jail = Jail::rooted_at(&root, 1000, 1000);
        let msg = |jail: &Jail| format!("{:#}", jail.preflight().unwrap_err());

        assert!(
            msg(&jail).contains("nitro_sandboxes_enabled"),
            "{}",
            msg(&jail)
        );
        std::fs::write(root.join("nitro_enclaves"), "").unwrap();
        assert!(msg(&jail).contains("VMM"), "{}", msg(&jail));
        std::fs::write(root.join("firecracker"), "").unwrap();
        assert!(msg(&jail).contains("jailer"), "{}", msg(&jail));
        std::fs::write(root.join("jailer"), "").unwrap();
        assert!(msg(&jail).contains("guest kernel"), "{}", msg(&jail));
        std::fs::write(root.join("vmlinuz"), "").unwrap();
        assert!(msg(&jail).contains("guest initramfs"), "{}", msg(&jail));
        std::fs::write(root.join("initrd.img"), "").unwrap();
        assert!(
            msg(&jail).contains("guest root filesystem"),
            "{}",
            msg(&jail)
        );
        std::fs::write(root.join("rootfs.ext4"), "").unwrap();
        jail.preflight().unwrap();
    }

    #[test]
    fn uids_come_from_the_reserved_range() {
        let root = temp_root("jail-uids");
        let mut jail = Jail::rooted_at(&root, 300_000, 300_000);
        jail.uid_count = 3;
        assert_eq!(jail.free_uid(&[]).unwrap(), 300_000);
        assert_eq!(jail.free_uid(&[300_000, 300_002]).unwrap(), 300_001);
        assert!(jail.free_uid(&[300_000, 300_001, 300_002]).is_err());
    }

    #[test]
    fn chroot_path_and_needle_follow_jailer_conventions() {
        let dir = std::path::Path::new("/workspace/.sandboxes/calm-quilt");
        assert_eq!(
            default_vm_dir(dir, "calm-quilt"),
            std::path::Path::new("/workspace/.sandboxes/calm-quilt/j/firecracker/calm-quilt/root")
        );
        assert_eq!(needle("calm-quilt"), "--id\0calm-quilt\0");
    }

    #[test]
    fn device_numbers_decode_like_glibc() {
        // makedev(10, 232) (/dev/kvm) and makedev(259, 70000).
        assert_eq!(dev_numbers(0x0a_e8), (10, 232));
        let big = (259u64 << 8) | (70_000 & 0xff) | ((70_000u64 & !0xff) << 12);
        assert_eq!(dev_numbers(big), (259, 70_000));
    }

    #[test]
    fn resolve_program_handles_paths_and_path_lookup() {
        assert!(resolve_program(std::path::Path::new("sh")).is_some());
        assert!(resolve_program(std::path::Path::new("/bin/sh")).is_some());
        assert!(resolve_program(std::path::Path::new("no-such-devbox-binary")).is_none());
    }
}
