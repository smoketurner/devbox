//! Host warm-up: prepare the box and self-tag `devbox:ready=true`.
//!
//! Once the box is warmed, the agent sets `devbox:ready=true` on its own EC2
//! instance via `ec2:CreateTags`. The reconciler observes that tag and flips
//! the `DevboxDoc` from `Warming` to `Ready`; only Ready boxes can be claimed.
//!
//! Warm-up starts Docker, freshens the snapshot-seeded repos under `/workspace`
//! to near-HEAD (see [`crate::freshen`]), then tags the box ready.
//!
//! If warm-up fails the agent exits with a non-zero status. The reconciler's
//! reaper terminates boxes that never become ready within `ready_timeout` and
//! the ASG relaunches a replacement — no lifecycle-hook `ABANDON` signal needed.

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_ec2::config::Region;
use aws_sdk_ec2::types::Tag;

use crate::freshen::{self, ReadyDecision};
use crate::imds;

/// Run warm-up and self-tag the instance `devbox:ready=true`.
///
/// `devbox:ready` means the box is fully warmed — Docker running, IMDS
/// reachable, tag applied. A box where any step fails is NOT tagged, so the
/// reconciler's reaper terminates it after `ready_timeout` and the ASG
/// relaunches a replacement.
///
/// # Errors
///
/// Returns an error if Docker fails to start, a required workspace is absent
/// (snapshot failed to attach), instance identity cannot be read from IMDS, or
/// the `ec2:CreateTags` call fails.
pub(crate) async fn run() -> Result<()> {
    ensure_docker_running()?;

    if freshen::freshen_workspace().await == ReadyDecision::FailAndReap {
        anyhow::bail!(
            "workspace required but empty; snapshot likely failed to attach — leaving box \
             un-tagged so the reconciler reaps it"
        );
    }

    let imds_client = imds::client();
    let instance_id = imds::get(&imds_client, "/latest/meta-data/instance-id")
        .await?
        .context("instance-id unavailable from IMDS")?;
    let region = imds::get(&imds_client, "/latest/meta-data/placement/region")
        .await?
        .context("region unavailable from IMDS")?;

    let ec2_client = ec2_client(region).await;
    tag_ready(&ec2_client, &instance_id).await?;

    tracing::info!(instance_id, "warm-up complete; tagged devbox:ready=true");
    Ok(())
}

/// Build an EC2 client bound to the instance's region, using the host
/// instance-profile credentials.
async fn ec2_client(region: String) -> aws_sdk_ec2::Client {
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .load()
        .await;
    aws_sdk_ec2::Client::new(&config)
}

/// Set `devbox:ready=true` on this instance.
async fn tag_ready(client: &aws_sdk_ec2::Client, instance_id: &str) -> Result<()> {
    let tag = Tag::builder().key("devbox:ready").value("true").build();

    client
        .create_tags()
        .resources(instance_id)
        .tags(tag)
        .send()
        .await
        .with_context(|| format!("ec2:CreateTags devbox:ready=true on {instance_id}"))?;

    Ok(())
}

/// Start the Docker daemon and return an error if it fails.
///
/// `devbox:ready` means the box is fully warmed, which includes Docker running.
/// A box where this fails is left un-tagged and is reaped by the control plane.
fn ensure_docker_running() -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["start", "docker"])
        .status()
        .context("failed to invoke `systemctl start docker`")?;

    if status.success() {
        tracing::info!("docker daemon started");
        Ok(())
    } else {
        anyhow::bail!(
            "`systemctl start docker` exited with code {:?}",
            status.code()
        )
    }
}
