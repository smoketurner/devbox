//! On-box records for sub-agent sandboxes.
//!
//! Each sandbox owns one directory under the store root
//! (`$DEVBOX_SANDBOX_DIR`, default `/workspace/.sandboxes`):
//!
//! ```text
//! <root>/<name>/meta.json     this record
//! <root>/<name>/console.log   the guest serial console (VMM stdout/stderr)
//! <root>/<name>/workspace.img the guest's /workspace block device
//! ```
//!
//! The directory name is the sandbox name, so creating it is the uniqueness
//! check. Names are validated with [`is_valid_devbox_name`] before any path is
//! built from them, so a name can never escape the root.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use devbox_common::{env_non_empty, is_valid_devbox_name, naming::name_from_seed};
use serde::{Deserialize, Serialize};

/// Env var overriding the store root.
const ROOT_ENV: &str = "DEVBOX_SANDBOX_DIR";

/// Default store root: on the `/workspace` volume, which is sized for checkouts
/// and owned by the claimant.
const DEFAULT_ROOT: &str = "/workspace/.sandboxes";

/// Attempts at a free random name before giving up.
const NAME_ATTEMPTS: usize = 64;

/// The persisted record for one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SandboxMeta {
    /// `adjective-noun` handle, also the directory name.
    pub(crate) name: String,
    /// Harness the guest runs (e.g. `claude`, `codex`).
    pub(crate) harness: String,
    /// The task handed to the harness.
    pub(crate) prompt: String,
    /// The claimant's repository the sandbox was cloned from.
    pub(crate) repo: PathBuf,
    /// Commit the sandbox started from.
    pub(crate) base_commit: String,
    /// Branch the sub-agent works on, and the branch its work is collected to.
    pub(crate) branch: String,
    /// Creation time, seconds since the Unix epoch.
    pub(crate) created_unix: u64,
    /// Guest vCPUs.
    pub(crate) vcpus: u8,
    /// Guest memory.
    pub(crate) memory_mib: u32,
    /// VMM process id while launched.
    pub(crate) pid: Option<u32>,
    /// Whether the guest's commits have been fetched onto `branch`.
    pub(crate) collected: bool,
    /// Review checkout of `branch` on the host, once collected.
    #[serde(default)]
    pub(crate) worktree: Option<PathBuf>,
}

/// The directory holding every sandbox on this box.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The store at `$DEVBOX_SANDBOX_DIR`, or [`DEFAULT_ROOT`].
    pub(crate) fn from_env() -> Self {
        Self::new(
            env_non_empty(ROOT_ENV).map_or_else(|| PathBuf::from(DEFAULT_ROOT), PathBuf::from),
        )
    }

    /// The directory for `name`.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a valid sandbox name.
    pub(crate) fn dir(&self, name: &str) -> Result<PathBuf> {
        anyhow::ensure!(
            is_valid_devbox_name(name),
            "invalid sandbox name {name:?} (lowercase letters, digits, '-' and '_')"
        );
        Ok(self.root.join(name))
    }

    /// Pick a random `adjective-noun` name not used by any sandbox in the store.
    ///
    /// # Errors
    ///
    /// Returns an error if the RNG fails or no free name is found.
    pub(crate) fn fresh_name(&self) -> Result<String> {
        for _ in 0..NAME_ATTEMPTS {
            let mut seed = [0u8; 4];
            aws_lc_rs::rand::fill(&mut seed)
                .map_err(|_| anyhow::anyhow!("failed to read from the random number generator"))?;
            let name = name_from_seed(seed);
            if !self.dir(&name)?.exists() {
                return Ok(name);
            }
        }
        anyhow::bail!("could not find a free sandbox name after {NAME_ATTEMPTS} attempts")
    }

    /// Create the directory for `meta.name` and write its record.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is invalid or already taken, or on I/O failure.
    pub(crate) fn create(&self, meta: &SandboxMeta) -> Result<PathBuf> {
        let dir = self.dir(&meta.name)?;
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create sandbox root {}", self.root.display()))?;
        std::fs::create_dir(&dir).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!("a sandbox named {:?} already exists", meta.name)
            } else {
                anyhow::Error::new(e).context(format!("create {}", dir.display()))
            }
        })?;
        self.save(meta)?;
        Ok(dir)
    }

    /// Load the record for `name`.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is invalid, unknown, or its record is unreadable.
    pub(crate) fn load(&self, name: &str) -> Result<SandboxMeta> {
        let path = self.dir(name)?.join("meta.json");
        let raw = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("no sandbox named {name:?} (see `devbox-agent sandbox ls`)")
            } else {
                anyhow::Error::new(e).context(format!("read {}", path.display()))
            }
        })?;
        serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))
    }

    /// Persist `meta`, replacing the previous record atomically.
    ///
    /// # Errors
    ///
    /// Returns an error on I/O or serialization failure.
    pub(crate) fn save(&self, meta: &SandboxMeta) -> Result<()> {
        let dir = self.dir(&meta.name)?;
        let tmp = dir.join("meta.json.tmp");
        let json = serde_json::to_vec_pretty(meta).context("serialize sandbox record")?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, dir.join("meta.json"))
            .with_context(|| format!("commit {}", tmp.display()))
    }

    /// Every readable record in the store, oldest first. A missing root is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if the root exists but cannot be listed.
    pub(crate) fn list(&self) -> Result<Vec<SandboxMeta>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("list {}", self.root.display())));
            }
        };
        let mut metas: Vec<SandboxMeta> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter_map(|name| self.load(&name).ok())
            .collect();
        metas.sort_by(|a, b| {
            a.created_unix
                .cmp(&b.created_unix)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(metas)
    }
}

/// Seconds since the Unix epoch (0 if the clock is before it).
pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::{SandboxMeta, Store};
    use crate::sandbox::test_support::temp_root;

    fn meta(name: &str, created_unix: u64) -> SandboxMeta {
        SandboxMeta {
            name: name.to_owned(),
            harness: "claude".to_owned(),
            prompt: "fix it".to_owned(),
            repo: "/workspace/devbox".into(),
            base_commit: "abc".to_owned(),
            branch: format!("agent/{name}"),
            created_unix,
            vcpus: 2,
            memory_mib: 4096,
            pid: None,
            collected: false,
            worktree: None,
        }
    }

    #[test]
    fn create_load_round_trips_and_rejects_duplicates() {
        let store = Store::new(temp_root("store-roundtrip"));
        let m = meta("calm-quilt", 10);
        store.create(&m).unwrap();
        assert_eq!(store.load("calm-quilt").unwrap(), m);
        let err = store.create(&m).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    }

    #[test]
    fn list_is_oldest_first_and_missing_root_is_empty() {
        let root = temp_root("store-list");
        let store = Store::new(root.clone());
        store.create(&meta("swift-otter", 20)).unwrap();
        store.create(&meta("bold-acorn", 10)).unwrap();
        let names: Vec<String> = store.list().unwrap().into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["bold-acorn", "swift-otter"]);

        let missing = Store::new(root.join("nope"));
        assert!(missing.list().unwrap().is_empty());
    }

    #[test]
    fn names_cannot_escape_the_root() {
        let store = Store::new(temp_root("store-escape"));
        for bad in ["../etc", "a/b", "", "UPPER", "."] {
            assert!(store.dir(bad).is_err(), "{bad:?} must be rejected");
            assert!(store.load(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn unknown_name_says_how_to_list() {
        let store = Store::new(temp_root("store-unknown"));
        let err = store.load("calm-quilt").unwrap_err();
        assert!(format!("{err:#}").contains("sandbox ls"), "{err:#}");
    }

    #[test]
    fn fresh_name_is_valid_and_unused() {
        let store = Store::new(temp_root("store-fresh"));
        let name = store.fresh_name().unwrap();
        assert!(devbox_common::is_valid_devbox_name(&name));
        assert!(!store.dir(&name).unwrap().exists());
    }
}
