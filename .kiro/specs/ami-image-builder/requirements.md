# Requirements Document

## Introduction

Define an AMI build pipeline using EC2 Image Builder that produces a golden AMI for the devbox pool manager. The pipeline provisions Amazon Linux 2023 with all required development tooling, agent dependencies, pre-cloned repositories, and a warm-up health check daemon. The resulting AMI ID is published to SSM Parameter Store for consumption by the pool manager. This spec captures requirements for both the Terraform-based Image Builder infrastructure (implemented in a separate devbox-infra repository) and changes to the Rust pool manager in this workspace to consume AMI IDs dynamically.

## Glossary

- **Image_Builder_Pipeline**: An EC2 Image Builder pipeline that orchestrates the build, test, and distribution of AMIs on a defined schedule or trigger
- **Image_Recipe**: An EC2 Image Builder recipe that defines the base image (Amazon Linux 2023) and ordered list of components to apply during the AMI build
- **Component**: An EC2 Image Builder component that encapsulates a set of provisioning steps (install packages, configure services, clone repos) defined in YAML using the AWSTOE document format
- **Golden_AMI**: The output AMI produced by the Image Builder pipeline, containing all pre-baked software and configuration needed for a devbox instance to become Ready
- **SSM_Parameter**: An AWS Systems Manager Parameter Store parameter that holds the current production AMI ID, enabling decoupled consumption by the pool manager
- **Pool_Manager**: The devbox-server reconciliation loop that maintains a warm pool of EC2 instances, currently reading AMI ID from the POOL_AMI_ID environment variable
- **Warm_Up_Daemon**: A systemd service baked into the Golden AMI that performs post-boot initialization and exposes a health check endpoint to signal instance readiness
- **AMI_Rotation**: The process by which the pool manager transitions from an old AMI to a new AMI by draining old warm instances and launching replacements from the new AMI
- **Test_Component**: An Image Builder test component that validates the AMI after build by running health checks, verifying installed software, and confirming service startup

## Requirements

### Requirement 1: Image Builder Pipeline Definition

**User Story:** As a platform operator, I want an EC2 Image Builder pipeline that builds AMIs from Amazon Linux 2023, so that I can produce consistent, tested golden images for the devbox pool.

#### Acceptance Criteria

1. THE Image_Builder_Pipeline SHALL use Amazon Linux 2023 (arm64 or x86_64, configurable) as the base image for the Image_Recipe
2. THE Image_Builder_Pipeline SHALL execute the Image_Recipe components in a defined order: base OS updates, development tools, agent dependencies, repository cloning, warm-up daemon installation, SSH configuration, and security hardening
3. WHEN the Image_Builder_Pipeline completes successfully, THE Image_Builder_Pipeline SHALL produce a Golden_AMI with EBS volumes encrypted using the default AWS-managed KMS key
4. WHEN the Image_Builder_Pipeline completes successfully, THE Image_Builder_Pipeline SHALL tag the Golden_AMI with keys "devbox:pipeline-version", "devbox:build-date", "devbox:source-ami", and "devbox:component-versions"
5. THE Image_Builder_Pipeline SHALL configure a build instance type of at least m5.large (or equivalent) with a build timeout of 60 minutes maximum
6. IF the Image_Builder_Pipeline build fails at any component step, THEN THE Image_Builder_Pipeline SHALL terminate the build instance, publish a failure notification to an SNS topic, and retain build logs in CloudWatch for at least 30 days

### Requirement 2: Development Tools Component

**User Story:** As a coding agent, I want the AMI to include standard development tools pre-installed, so that I can begin coding immediately after claiming an instance without waiting for tool installation.

#### Acceptance Criteria

1. THE Component SHALL install Git version 2.40 or later from the Amazon Linux 2023 package repository
2. THE Component SHALL install build tooling including gcc, make, cmake, and pkg-config
3. THE Component SHALL install language runtimes: Node.js 20 LTS (via NodeSource or Amazon Linux extras), Python 3.11 or later, and Rust stable toolchain (via rustup)
4. THE Component SHALL install container tooling: Docker Engine and Docker Compose plugin
5. THE Component SHALL install utility tools: jq, curl, wget, unzip, tar, htop, and tree
6. THE Component SHALL configure the PATH environment variable system-wide so that all installed tools are available to any user session without additional shell configuration

### Requirement 3: Agent Dependencies Component

**User Story:** As a platform operator, I want coding agent software dependencies pre-installed in the AMI, so that agents can start executing tasks immediately upon instance claim.

#### Acceptance Criteria

1. THE Component SHALL create a dedicated non-root user account named "agent" with a home directory at /home/agent and a configurable UID
2. THE Component SHALL install the coding agent runtime and its dependencies under /opt/agent with appropriate file permissions (owned by the agent user, not world-writable)
3. THE Component SHALL pre-download and cache common package manager dependencies (npm global packages, pip packages, cargo crates) specified in a manifest file at /opt/agent/manifests/dependencies.json
4. THE Component SHALL configure systemd to enable and start the agent service on boot, with automatic restart on failure (RestartSec of 5 seconds, maximum 3 restart attempts within 60 seconds)
5. IF the agent runtime installation fails during the build, THEN THE Component SHALL exit with a non-zero status code causing the Image_Builder_Pipeline to report the build as failed

### Requirement 4: Repository Pre-Cloning Component

**User Story:** As a coding agent, I want frequently-used repositories pre-cloned in the AMI, so that common codebases are immediately available without clone wait time.

#### Acceptance Criteria

1. THE Component SHALL read a repository manifest from a configurable S3 bucket path (s3://BUCKET/manifests/repos.json) that lists repository URLs, target clone paths, and branch names
2. THE Component SHALL perform shallow clones (depth 1) of each repository listed in the manifest into /home/agent/repos/ with the specified branch checked out
3. THE Component SHALL set ownership of all cloned repositories to the agent user and group
4. IF a repository clone fails due to network error or authentication failure, THEN THE Component SHALL log the failure, skip the failed repository, and continue cloning remaining repositories without failing the overall build
5. WHEN the AMI boots as a devbox instance, THE Warm_Up_Daemon SHALL perform a git fetch on each pre-cloned repository to update to the latest commit before signaling readiness

### Requirement 5: Warm-Up Health Check Daemon

**User Story:** As a platform operator, I want each devbox instance to run a health check daemon that signals readiness to the ASG lifecycle hook, so that only fully initialized instances become available for claiming.

#### Acceptance Criteria

1. THE Warm_Up_Daemon SHALL be installed as a systemd service that starts automatically on boot with ordering after network-online.target and docker.service
2. WHEN the instance boots, THE Warm_Up_Daemon SHALL execute a configurable list of warm-up tasks including: verifying Docker daemon readiness, updating pre-cloned repositories, confirming agent service health, and validating network connectivity
3. WHEN all warm-up tasks complete successfully, THE Warm_Up_Daemon SHALL call the ASG CompleteLifecycleAction API with result "CONTINUE" using the instance's IAM role credentials and instance metadata to determine the lifecycle hook name and ASG name
4. THE Warm_Up_Daemon SHALL expose an HTTP health check endpoint on port 8642 at path /health that returns HTTP 200 with body "ready" after all warm-up tasks complete, and HTTP 503 with body "warming" before completion
5. IF any warm-up task fails after 3 retry attempts with exponential backoff (starting at 5 seconds), THEN THE Warm_Up_Daemon SHALL call CompleteLifecycleAction with result "ABANDON", log the failure reason to /var/log/devbox-warmup.log, and set the health endpoint to return HTTP 500 with the failure reason
6. THE Warm_Up_Daemon SHALL complete all warm-up tasks within 180 seconds of boot, and IF this timeout is exceeded, THEN THE Warm_Up_Daemon SHALL treat the timeout as a failed warm-up task

### Requirement 6: SSH Access Configuration

**User Story:** As a coding agent, I want SSH access configured for certificate-based auth via the Vouch CA, so that I can connect to a claimed devbox without any per-host key management.

See [`../ssh-access/`](../ssh-access/) for the full access design.

#### Acceptance Criteria

1. THE Component SHALL configure the SSH daemon to allow public key (certificate) authentication only and disable password authentication
2. THE Component SHALL bake the **Vouch SSH CA public key** into the image (e.g. `/etc/ssh/vouch_ca.pub`) and set `TrustedUserCAKeys` to it, so the host trusts Vouch-issued user certificates without any `authorized_keys` files
3. THE Component SHALL install the **`devbox-agent`** host binary (see [`../devbox-agent/`](../devbox-agent/)) and configure `AuthorizedPrincipalsCommand /usr/local/bin/devbox-agent principals %u` + `AuthorizedPrincipalsCommandUser nobody`, where the resolver reads the `devbox:owner` instance tag from IMDSv2 and prints the authorized principal (fail-closed: empty output when untagged or mismatched)
4. THE Component SHALL install a systemd unit (oneshot + bounded short-interval timer) running `devbox-agent provision`, which reads the `devbox:owner` tag and **pre-creates the claimant's UNIX account** (`useradd -m -s /bin/bash <owner>`) plus a passwordless-sudo sudoers template, so `ssh <owner>@box` resolves from `/etc/passwd`; no NSS module and no shared login account are baked
5. THE Component SHALL configure the SSH daemon to listen on port 22 with protocol version 2 only
6. THE Component SHALL set SSH idle timeout to 3600 seconds (ClientAliveInterval 300, ClientAliveCountMax 12) to prevent premature disconnection during agent work

> Note: the per-claim authorization (criteria 3–4) also requires the Launch
> Template to enable instance metadata tags (`InstanceMetadataTags=enabled`) so the
> `devbox:owner` tag is readable via IMDS. See `../ssh-access/design.md` and
> `../devbox-agent/`.

### Requirement 7: AMI ID Publication to SSM Parameter Store

**User Story:** As a platform operator, I want the pipeline to publish the new AMI ID to SSM Parameter Store after successful build and test, so that the pool manager can discover and use it without manual configuration updates.

#### Acceptance Criteria

1. WHEN the Image_Builder_Pipeline produces a Golden_AMI and all Test_Components pass, THE Image_Builder_Pipeline SHALL write the AMI ID to an SSM_Parameter at path /devbox/ami/latest with type String
2. THE Image_Builder_Pipeline SHALL also write the AMI ID to a versioned SSM_Parameter at path /devbox/ami/history/{build-date} for audit trail purposes
3. WHEN the SSM_Parameter at /devbox/ami/latest is updated, THE Image_Builder_Pipeline SHALL tag the parameter with "devbox:build-id", "devbox:pipeline-execution-id", and "devbox:base-os" metadata
4. THE Image_Builder_Pipeline SHALL retain the previous AMI ID value in an SSM_Parameter at path /devbox/ami/previous to enable rollback
5. IF the SSM_Parameter write fails, THEN THE Image_Builder_Pipeline SHALL retry 3 times with exponential backoff before publishing a failure notification to the SNS topic

### Requirement 8: Launch Template AMI Resolution (Terraform)

> **Supersedes the previous "Pool Manager AMI Discovery from SSM."** The pool manager no
> longer reads the AMI or manages Launch Template versions; the Terraform-owned Launch
> Template resolves the AMI from SSM directly. See [`../infra-boundary/`](../infra-boundary/).

**User Story:** As a platform operator, I want the Launch Template to always launch the
current AMI without anyone editing it, so new warm instances are current automatically.

#### Acceptance Criteria

1. THE Terraform-managed Launch Template SHALL set `ImageId = resolve:ssm:/devbox/ami/latest` so that any instance the ASG launches uses the AMI currently published by the pipeline.
2. THE Pool_Manager (control plane) SHALL NOT read the AMI ID, update the Launch Template, or manage Launch Template versions.
3. Because `/devbox/ami/latest` is the resolution source, new launches (warm-pool growth and refresh replacements) SHALL pick up a new AMI with no Launch Template change.

### Requirement 9: AMI Rotation via EventBridge Instance Refresh

> **Supersedes the previous "AMI Rotation in the Pool Manager."** Rotation is now event-driven
> infrastructure, not pool-manager logic.

**User Story:** As a platform operator, I want existing idle warm instances to roll onto a new
AMI automatically when one is published, without disrupting any in-use devbox.

#### Acceptance Criteria

1. WHEN the pipeline publishes a new AMI (updates `/devbox/ami/latest`), an **EventBridge rule** SHALL trigger an ASG **instance refresh** for the pool (via an SSM Automation or Lambda target).
2. THE instance refresh SHALL be configured with `ScaleInProtectedInstances = Ignore` so that **scale-in-protected (Claimed/in-use) instances are not replaced**; only unclaimed warm instances are rolled.
3. THE instance refresh SHALL set a `MinHealthyPercentage` (and honor the warm-up lifecycle hook on replacements) such that warm capacity remains available during the roll and the pool is never fully drained.
4. Claimed instances SHALL adopt the new AMI naturally when released and replaced by the ASG; the refresh SHALL NOT force-terminate them.
5. THE refresh executor SHALL require only `autoscaling:StartInstanceRefresh`, `DescribeInstanceRefreshes`, and `DescribeAutoScalingGroups`, and SHALL be provisioned by Terraform.
6. Reusing scale-in protection as the "in use" signal is intentional and conservative: protection tracks `Claimed`, and per `../ssh-access/` an unclaimed host authorizes no SSH principals, so an unprotected (Ready) host reliably has no connections.

### Requirement 10: AMI Build Triggers

**User Story:** As a platform operator, I want to control when new AMI builds are triggered, so that I can balance freshness against build costs and operational stability.

#### Acceptance Criteria

1. THE Image_Builder_Pipeline SHALL support scheduled builds via a configurable cron expression (default: weekly on Sunday at 02:00 UTC)
2. THE Image_Builder_Pipeline SHALL support manual trigger via AWS Console or CLI (StartImagePipelineExecution API)
3. THE Image_Builder_Pipeline SHALL support event-driven trigger when the repository manifest in S3 is updated (via S3 event notification to EventBridge)
4. WHEN a build is triggered while a previous build is still in progress, THE Image_Builder_Pipeline SHALL queue the new build request and execute it after the current build completes, rather than running concurrent builds
5. THE Image_Builder_Pipeline SHALL apply a rate limit of at most 4 builds per 24-hour period to control costs, and IF this limit is reached, THEN THE Image_Builder_Pipeline SHALL reject additional triggers with a log message indicating the rate limit

### Requirement 11: AMI Testing and Validation

**User Story:** As a platform operator, I want the pipeline to validate each AMI before promotion, so that broken images never reach the production pool.

#### Acceptance Criteria

1. WHEN the Golden_AMI build phase completes, THE Image_Builder_Pipeline SHALL launch a test instance from the new AMI and execute all Test_Components against it
2. THE Test_Component SHALL verify that all required packages are installed by checking binary existence and minimum version numbers for git, node, python3, rustc, docker, and jq
3. THE Test_Component SHALL verify that the Warm_Up_Daemon starts successfully and the health endpoint returns HTTP 200 within 120 seconds of instance boot
4. THE Test_Component SHALL verify that the agent user exists, has the correct home directory, and can execute sudo without a password prompt
5. THE Test_Component SHALL verify that SSH daemon is running, accepts connections on port 22, and rejects password authentication attempts
6. IF any Test_Component fails, THEN THE Image_Builder_Pipeline SHALL abort AMI distribution, retain the failed AMI for 7 days for debugging (tagged with "devbox:status" = "failed"), and publish a failure notification to the SNS topic
7. WHEN all Test_Components pass, THE Image_Builder_Pipeline SHALL tag the Golden_AMI with "devbox:status" = "production" and proceed to SSM parameter publication

### Requirement 12: Security Hardening Component

**User Story:** As a platform operator, I want the AMI to follow security best practices, so that devbox instances are hardened against common attack vectors.

#### Acceptance Criteria

1. THE Component SHALL configure IMDSv2 as required (HttpTokens = required, HttpPutResponseHopLimit = 2) at the AMI level so that instances launched from the Golden_AMI enforce IMDSv2 by default
2. THE Component SHALL disable unnecessary system services including cups, avahi-daemon, and bluetooth
3. THE Component SHALL configure automatic security updates via dnf-automatic for Amazon Linux 2023 critical security patches only (no feature updates)
4. THE Component SHALL configure audit logging (auditd) with rules for monitoring privileged command execution and file access to /etc/shadow and /etc/passwd
5. THE Component SHALL set file permissions on /home/agent to 0750 and configure umask 027 for the agent user to prevent world-readable file creation
6. THE Component SHALL remove or disable any default AWS-provided credentials or access keys, ensuring instances rely solely on their IAM instance profile for AWS API access
