//! `doctor`: one-shot diagnostic of warm-cache delivery on a claimed box.
//!
//! When a freshly-claimed devbox builds cold (re-downloads the pinned toolchain,
//! recompiles dependencies from scratch — see devbox-infra#13), the cause is one
//! of a few layers, and they are hard to tell apart by eye. `doctor` prints them
//! all at once so `devbox ssh <box> -- devbox-agent doctor` says which layer is
//! broken:
//!
//! - **root-disk fallback** — `/workspace` is not a separate mount (the snapshot
//!   volume did not attach), so there are no warmed caches at all.
//! - **env not applied** — `RUSTUP_HOME`/`CARGO_HOME` resolve to `/opt/rust`
//!   instead of the on-volume `/workspace/.{rustup,cargo}`, so a warmed volume is
//!   present but unused. `doctor` runs inside the claimant's SSH session, so the
//!   values it prints are exactly the ones the claimant's `cargo`/`rustup` see.
//! - **stale / cold snapshot** — the env points on-volume but the pinned
//!   toolchain or the cargo registry cache is missing, so the snapshot predates
//!   the warm build (or never warmed).
//!
//! It is read-only and best-effort: every probe degrades to a printed note rather
//! than aborting, so the report is always complete even when IMDS or EC2 is
//! unreachable.

use std::path::Path;

use anyhow::Result;
use aws_config::BehaviorVersion;
use aws_sdk_ec2::config::Region;
use aws_sdk_ec2::types::Filter;

use crate::imds;

/// Workspace mount the snapshot volume is expected at.
const WORKSPACE: &str = "/workspace";

/// Print the diagnostic report. Always returns `Ok`: a doctor that aborted on the
/// first unreachable probe would hide the very signal the operator needs.
pub(crate) async fn run() -> Result<()> {
    println!("devbox doctor");
    println!("=============");

    let imds_client = imds::client();
    let instance_id = imds::get(&imds_client, "/latest/meta-data/instance-id")
        .await
        .ok()
        .flatten();
    let owner = imds::instance_tag(&imds_client, "devbox:owner")
        .await
        .ok()
        .flatten();
    println!("instance:        {}", optional(instance_id.as_deref()));
    println!("devbox:owner:    {}", optional(owner.as_deref()));
    println!();

    report_workspace_mount();
    println!();

    let rustup_home = std::env::var("RUSTUP_HOME").unwrap_or_default();
    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_default();
    report_env_home("RUSTUP_HOME", &rustup_home);
    report_env_home("CARGO_HOME", &cargo_home);
    report_registry_cache(&cargo_home);
    println!();

    report_repos(&rustup_home);

    if let Some(id) = instance_id {
        println!();
        report_volumes(&id).await;
    }

    Ok(())
}

/// Whether `/workspace` is its own mount (the attached snapshot volume) or has
/// fallen back to the root filesystem (volume did not attach → no warm caches).
fn report_workspace_mount() {
    println!("workspace mount:");
    let mounts = match std::fs::read_to_string("/proc/mounts") {
        Ok(m) => m,
        Err(e) => {
            println!("  could not read /proc/mounts: {e}");
            return;
        }
    };

    let source_of = |mountpoint: &str| -> Option<(String, String)> {
        mounts.lines().find_map(|line| {
            let mut f = line.split_whitespace();
            let dev = f.next()?;
            let mnt = f.next()?;
            let fstype = f.next()?;
            (mnt == mountpoint).then(|| (dev.to_string(), fstype.to_string()))
        })
    };

    match source_of(WORKSPACE) {
        Some((dev, fstype)) => {
            let root_dev = source_of("/").map(|(d, _)| d);
            if root_dev.as_deref() == Some(dev.as_str()) {
                println!("  /workspace is on the ROOT filesystem ({dev}, {fstype})");
                println!("  -> the snapshot volume did NOT attach; caches are cold");
            } else {
                println!("  /workspace mounted from {dev} ({fstype}) — separate volume OK");
            }
        }
        None => println!("  /workspace is not a mount point (no separate volume)"),
    }
}

/// Report where a tool-home env var resolves and whether it is on the workspace
/// volume. A value under `/opt` means `/etc/environment` was not applied to this
/// shell, so the warmed on-volume caches are bypassed.
fn report_env_home(name: &str, value: &str) {
    if value.is_empty() {
        println!("  {name:<12} (unset) -> tools fall back to $HOME or /opt; caches bypassed");
        return;
    }
    let on_volume = Path::new(value).starts_with(WORKSPACE);
    let exists = Path::new(value).is_dir();
    let note = if on_volume {
        "on workspace volume"
    } else {
        "NOT on workspace volume -> /etc/environment not applied to this shell"
    };
    let present = if exists { "" } else { " (missing)" };
    println!("  {name:<12} {value}{present} -> {note}");
}

/// Whether the cargo registry cache is populated (warmed deps ride here).
fn report_registry_cache(cargo_home: &str) {
    if cargo_home.is_empty() {
        return;
    }
    let cache = Path::new(cargo_home).join("registry").join("cache");
    let populated = std::fs::read_dir(&cache).is_ok_and(|mut entries| entries.next().is_some());
    if populated {
        println!("  registry cache populated at {}", cache.display());
    } else {
        println!(
            "  registry cache EMPTY at {} -> deps will recompile cold",
            cache.display()
        );
    }
}

/// For each git repo seeded under `/workspace`, report whether `target/` is
/// present and, when the repo pins a toolchain (`rust-toolchain.toml`), whether
/// that exact toolchain is installed under `RUSTUP_HOME` (the #13 symptom is a
/// pinned toolchain that was never warmed onto the volume).
fn report_repos(rustup_home: &str) {
    println!("workspace repos:");
    let entries = match std::fs::read_dir(WORKSPACE) {
        Ok(e) => e,
        Err(e) => {
            println!("  could not read {WORKSPACE}: {e}");
            return;
        }
    };

    let mut found = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.join(".git").exists() {
            continue;
        }
        found = true;
        let name = entry.file_name().to_string_lossy().into_owned();
        let has_target = path.join("target").is_dir();
        let target = if has_target {
            "target/ present"
        } else {
            "target/ MISSING (cold)"
        };
        println!("  {name}: {target}");

        if let Some(channel) = pinned_toolchain(&path) {
            let installed = toolchain_installed(rustup_home, &channel);
            let state = if installed {
                "installed"
            } else {
                "NOT installed under RUSTUP_HOME -> will be downloaded on first build"
            };
            println!("    pinned toolchain {channel}: {state}");
        }
    }
    if !found {
        println!("  (no git repos under {WORKSPACE})");
    }
}

/// The `channel` pinned in a repo's `rust-toolchain.toml`, if present. A minimal
/// line parse — avoids a TOML dependency for one well-known field.
fn pinned_toolchain(repo: &Path) -> Option<String> {
    let text = std::fs::read_to_string(repo.join("rust-toolchain.toml")).ok()?;
    text.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix("channel")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim();
        Some(rest.trim_matches('"').trim_matches('\'').to_string())
    })
}

/// Whether a toolchain whose name begins with `channel-` exists under
/// `$RUSTUP_HOME/toolchains` (installed dirs are `1.96.1-aarch64-...`).
fn toolchain_installed(rustup_home: &str, channel: &str) -> bool {
    if rustup_home.is_empty() {
        return false;
    }
    let prefix = format!("{channel}-");
    std::fs::read_dir(Path::new(rustup_home).join("toolchains")).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n == channel || n.starts_with(&prefix)
        })
    })
}

/// List the EBS volumes attached to this instance with their source snapshot id,
/// so the operator can compare against `/devbox/workspace-snapshot/latest`. The
/// `/dev/sdb` (or its NVMe alias) volume is the workspace volume. Best-effort: a
/// missing `ec2:DescribeVolumes` permission or any API error just prints a note.
async fn report_volumes(instance_id: &str) {
    println!("attached volumes (compare snapshot id to /devbox/workspace-snapshot/latest):");
    let region = match imds::get(&imds::client(), "/latest/meta-data/placement/region").await {
        Ok(Some(r)) => r,
        _ => {
            println!("  region unavailable; skipping volume lookup");
            return;
        }
    };
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .load()
        .await;
    let client = aws_sdk_ec2::Client::new(&config);
    let filter = Filter::builder()
        .name("attachment.instance-id")
        .values(instance_id)
        .build();
    let resp = match client.describe_volumes().filters(filter).send().await {
        Ok(r) => r,
        Err(e) => {
            println!("  ec2:DescribeVolumes unavailable: {e}");
            return;
        }
    };
    for vol in resp.volumes() {
        let device = vol
            .attachments()
            .first()
            .and_then(|a| a.device())
            .unwrap_or("?");
        println!(
            "  {} device {} snapshot {}",
            vol.volume_id().unwrap_or("?"),
            device,
            vol.snapshot_id()
                .filter(|s| !s.is_empty())
                .unwrap_or("(none)"),
        );
    }
}

/// Render an optional value for the header lines.
fn optional(value: Option<&str>) -> &str {
    value.unwrap_or("(unavailable)")
}
