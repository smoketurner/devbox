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

**User Story:** As a coding agent, I want SSH access properly configured on the AMI, so that I can connect to the devbox instance securely after claiming it.

#### Acceptance Criteria

1. THE Component SHALL configure the SSH daemon to allow public key authentication only and disable password authentication
2. THE Component SHALL configure the SSH daemon to use EC2 Instance Connect for public key delivery, allowing key injection at connection time without baking static keys into the AMI
3. THE Component SHALL configure the SSH daemon to listen on port 22 with protocol version 2 only
4. THE Component SHALL set SSH idle timeout to 3600 seconds (ClientAliveInterval 300, ClientAliveCountMax 12) to prevent premature disconnection during agent work
5. THE Component SHALL configure the agent user account to accept SSH connections with a login shell of /bin/bash and appropriate sudoers entry (passwordless sudo for the agent user)

### Requirement 7: AMI ID Publication to SSM Parameter Store

**User Story:** As a platform operator, I want the pipeline to publish the new AMI ID to SSM Parameter Store after successful build and test, so that the pool manager can discover and use it without manual configuration updates.

#### Acceptance Criteria

1. WHEN the Image_Builder_Pipeline produces a Golden_AMI and all Test_Components pass, THE Image_Builder_Pipeline SHALL write the AMI ID to an SSM_Parameter at path /devbox/ami/latest with type String
2. THE Image_Builder_Pipeline SHALL also write the AMI ID to a versioned SSM_Parameter at path /devbox/ami/history/{build-date} for audit trail purposes
3. WHEN the SSM_Parameter at /devbox/ami/latest is updated, THE Image_Builder_Pipeline SHALL tag the parameter with "devbox:build-id", "devbox:pipeline-execution-id", and "devbox:base-os" metadata
4. THE Image_Builder_Pipeline SHALL retain the previous AMI ID value in an SSM_Parameter at path /devbox/ami/previous to enable rollback
5. IF the SSM_Parameter write fails, THEN THE Image_Builder_Pipeline SHALL retry 3 times with exponential backoff before publishing a failure notification to the SNS topic

### Requirement 8: Pool Manager AMI Discovery from SSM

**User Story:** As a platform operator, I want the pool manager to read the AMI ID from SSM Parameter Store instead of a static environment variable, so that new AMIs are automatically picked up without redeploying the pool manager.

#### Acceptance Criteria

1. THE Pool_Manager SHALL support reading the AMI ID from an SSM_Parameter path specified by the POOL_AMI_SSM_PARAMETER environment variable (e.g., /devbox/ami/latest)
2. WHEN POOL_AMI_SSM_PARAMETER is set, THE Pool_Manager SHALL fetch the AMI ID from SSM Parameter Store on startup and on each reconciliation tick (or at a configurable polling interval no more frequent than once per 60 seconds)
3. WHEN POOL_AMI_SSM_PARAMETER is not set, THE Pool_Manager SHALL fall back to reading POOL_AMI_ID from the environment variable for backward compatibility
4. IF the SSM Parameter Store fetch fails, THEN THE Pool_Manager SHALL log the error, retain the last known valid AMI ID, and retry on the next polling interval
5. WHEN the AMI ID fetched from SSM differs from the currently configured AMI ID, THE Pool_Manager SHALL log the change and trigger AMI rotation on the next reconciliation tick

### Requirement 9: AMI Rotation in the Pool Manager

**User Story:** As a platform operator, I want the pool manager to gracefully rotate instances to a new AMI when one becomes available, so that the warm pool always uses the latest tested image without disrupting claimed instances.

#### Acceptance Criteria

1. WHEN a new AMI ID is detected, THE Pool_Manager SHALL update the Launch Template with the new AMI ID and create a new Launch Template version
2. WHEN a new Launch Template version is created for AMI rotation, THE Pool_Manager SHALL update the ASG to reference the new Launch Template version
3. THE Pool_Manager SHALL NOT terminate or disturb instances that are in Claimed state during AMI rotation
4. WHEN AMI rotation is triggered, THE Pool_Manager SHALL terminate unclaimed Ready instances running the old AMI in batches (configurable batch size, default 1 at a time) on successive reconciliation ticks, allowing the ASG to replace them with instances using the new AMI
5. IF the pool has zero Ready instances available during rotation, THEN THE Pool_Manager SHALL pause old-instance termination until at least one new-AMI instance reaches Ready state, ensuring the pool is never fully drained
6. THE Pool_Manager SHALL log AMI rotation progress including: rotation start, each batch of old instances terminated, each new instance reaching Ready state, and rotation completion
7. WHEN all unclaimed instances are running the new AMI, THE Pool_Manager SHALL log rotation completion and resume normal reconciliation behavior

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
