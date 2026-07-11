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

use std::time::Instant;

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_ec2::config::Region;
use aws_sdk_ec2::types::Tag;
use devbox_common::WarmupReportRequest;

use crate::freshen::{self, FreshenOutcome, millis_u64};
use crate::imds;
use crate::server_client::ServerClient;

/// Run warm-up and self-tag the instance `devbox:ready=true`.
///
/// `devbox:ready` means the box is fully warmed — Docker running, IMDS
/// reachable, tag applied. A box where any step fails is NOT tagged, so the
/// reconciler's reaper terminates it after `ready_timeout` and the ASG
/// relaunches a replacement.
///
/// # Errors
///
/// Returns an error if Docker fails to start, instance identity cannot be read
/// from IMDS, or the `ec2:CreateTags` call fails.
pub(crate) async fn run() -> Result<()> {
    let warmup_start = Instant::now();

    let docker_start = Instant::now();
    ensure_docker_running()?;
    let docker_start_ms = millis_u64(docker_start.elapsed());

    let imds_client = imds::client();
    let instance_id = imds::get(&imds_client, "/latest/meta-data/instance-id")
        .await?
        .context("instance-id unavailable from IMDS")?;
    // Self-tagging must target this instance's placement region, not an AWS_REGION
    // override — a mismatched region makes ec2:CreateTags fail and the box never goes
    // Ready. (read_key uses the AWS_REGION-honoring imds::region for SSM access.)
    let region = imds::get(&imds_client, "/latest/meta-data/placement/region")
        .await?
        .context("region unavailable from IMDS")?;

    // One client for both the freshen token minting and the warm-up report, so
    // the cached web-identity JWT is minted once.
    let mut client = crate::git::build_server_client().await;
    let freshen = freshen::freshen_workspace(client.as_mut()).await;

    let ec2_client = ec2_client(region).await;
    tag_ready(&ec2_client, &instance_id).await?;

    tracing::info!(instance_id, "warm-up complete; tagged devbox:ready=true");

    // Readiness is already tagged; the report is strictly best-effort (a lost
    // report must never fail warm-up — degrade, don't reap).
    report_warmup(client.as_mut(), docker_start_ms, &freshen, warmup_start).await;
    Ok(())
}

/// POST the warm-up report best-effort. Never returns an error: readiness is
/// already tagged, and a lost report must not fail warm-up. A 404 from a server
/// that predates the endpoint is logged at info and tolerated (the agent binary
/// reaches infra via a new AMI, so it may briefly run ahead of the server).
async fn report_warmup(
    client: Option<&mut ServerClient>,
    docker_start_ms: u64,
    freshen: &FreshenOutcome,
    warmup_start: Instant,
) {
    let Some(client) = client else {
        tracing::debug!("DEVBOX_SERVER_URL not set; skipping warmup report");
        return;
    };
    let report = WarmupReportRequest {
        docker_start_ms,
        freshen_total_ms: millis_u64(freshen.total),
        total_ms: millis_u64(warmup_start.elapsed()),
        workspace_present: freshen.workspace_present,
        repos: freshen.repos.clone(),
    };
    match client.report_warmup(&report).await {
        Ok(true) => tracing::info!(
            total_ms = report.total_ms,
            freshen_total_ms = report.freshen_total_ms,
            "reported warmup metrics"
        ),
        Ok(false) => tracing::info!("server predates warmup-report; skipping"),
        Err(e) => tracing::warn!(
            error = %format!("{e:#}"),
            "could not report warmup metrics; continuing"
        ),
    }
}

/// Build an EC2 client bound to `region` — this instance's IMDS placement region.
/// Self-tagging uses the placement region directly rather than the
/// `AWS_REGION`-honoring `imds::region`, since `ec2:CreateTags` must target the
/// region where this instance actually runs.
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
