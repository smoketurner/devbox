//! Host warm-up: prepare the box, then release the ASG launch lifecycle hook.
//!
//! The pool ASG holds each new instance in `Pending:Wait` behind an
//! `EC2_INSTANCE_LAUNCHING` lifecycle hook whose default result is `ABANDON`.
//! Only when the host signals readiness (`CONTINUE`) does the instance reach
//! `InService`; the reconciler then marks the `DevboxDoc` Ready. If warm-up
//! fails we signal `ABANDON` so the ASG recycles the half-baked box.
//!
//! The ASG name is discovered from the `aws:autoscaling:groupName` instance tag
//! (propagated by the ASG and readable via IMDS), and the hook name from
//! `DescribeLifecycleHooks`, so the host needs no static pool configuration.

use anyhow::{Context, Result, bail};
use aws_config::BehaviorVersion;
use aws_sdk_autoscaling::config::Region;

use crate::imds;

/// The launch transition whose hook this host is responsible for releasing.
const LAUNCHING_TRANSITION: &str = "autoscaling:EC2_INSTANCE_LAUNCHING";

/// Run warm-up and release (or abandon) the launch lifecycle hook.
///
/// # Errors
///
/// Returns an error if instance identity cannot be read from IMDS, no launching
/// hook is found, or the `CONTINUE` lifecycle action fails.
pub(crate) async fn run() -> Result<()> {
    ensure_docker_running();

    let imds_client = imds::client();
    let instance_id = imds::get(&imds_client, "/latest/meta-data/instance-id")
        .await?
        .context("instance-id unavailable from IMDS")?;
    let region = imds::get(&imds_client, "/latest/meta-data/placement/region")
        .await?
        .context("region unavailable from IMDS")?;
    let asg_name = imds::instance_tag(&imds_client, "aws:autoscaling:groupName")
        .await?
        .context("instance is not part of an ASG (aws:autoscaling:groupName tag missing)")?;

    let client = autoscaling_client(region).await;
    let hook = launch_hook_name(&client, &asg_name).await?;

    match complete(&client, &asg_name, &hook, &instance_id, "CONTINUE").await {
        Ok(()) => {
            tracing::info!(
                instance_id,
                asg = asg_name,
                hook,
                "warm-up complete; signalled CONTINUE"
            );
            Ok(())
        }
        Err(e) => {
            if let Err(abandon_err) =
                complete(&client, &asg_name, &hook, &instance_id, "ABANDON").await
            {
                tracing::warn!(
                    error = %abandon_err,
                    "failed to signal ABANDON after warm-up failure"
                );
            }
            Err(e)
        }
    }
}

/// Build an Auto Scaling client bound to the instance's region, using the host
/// instance-profile credentials.
async fn autoscaling_client(region: String) -> aws_sdk_autoscaling::Client {
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .load()
        .await;
    aws_sdk_autoscaling::Client::new(&config)
}

/// Find the launch-transition lifecycle hook on `asg`.
async fn launch_hook_name(client: &aws_sdk_autoscaling::Client, asg: &str) -> Result<String> {
    let resp = client
        .describe_lifecycle_hooks()
        .auto_scaling_group_name(asg)
        .send()
        .await
        .with_context(|| format!("describe lifecycle hooks for {asg}"))?;

    for hook in resp.lifecycle_hooks() {
        if hook.lifecycle_transition() == Some(LAUNCHING_TRANSITION)
            && let Some(name) = hook.lifecycle_hook_name()
        {
            return Ok(name.to_string());
        }
    }
    bail!("no {LAUNCHING_TRANSITION} lifecycle hook found on ASG {asg}")
}

/// Complete the lifecycle action with the given result (`CONTINUE` or `ABANDON`).
async fn complete(
    client: &aws_sdk_autoscaling::Client,
    asg: &str,
    hook: &str,
    instance_id: &str,
    result: &str,
) -> Result<()> {
    client
        .complete_lifecycle_action()
        .auto_scaling_group_name(asg)
        .lifecycle_hook_name(hook)
        .instance_id(instance_id)
        .lifecycle_action_result(result)
        .send()
        .await
        .with_context(|| format!("complete lifecycle action ({result})"))?;
    Ok(())
}

/// Best-effort: make sure the Docker daemon is up before signalling readiness.
fn ensure_docker_running() {
    match std::process::Command::new("systemctl")
        .args(["start", "docker"])
        .status()
    {
        Ok(status) if status.success() => tracing::info!("docker daemon started"),
        Ok(status) => tracing::warn!(code = ?status.code(), "`systemctl start docker` non-zero"),
        Err(e) => tracing::warn!(error = %e, "failed to start docker daemon"),
    }
}
