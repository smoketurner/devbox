//! Devbox server binary entry point.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use devbox_common::env_non_empty;
use devbox_server::auth::{
    AgentAuthConfig, AuthConfig, AuthError, Authenticator, OidcConfig, OidcEndpoints, discover,
};
use devbox_server::compute::ec2::Ec2;
use devbox_server::db::{DocumentStore, Pool, PoolConfig};
use devbox_server::reconcile::{ReconcilerConfig, spawn_reconciliation_loop};
use devbox_server::routes::{AppState, build_router};
use devbox_server::sessions::SessionArchives;
use secrecy::SecretString;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load configuration
    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite::memory:".to_string());
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .context("invalid PORT")?;

    tracing::info!(database_url = %redact_url(&database_url), port, "starting devbox server");

    // Connect to database
    let pool = Pool::connect(&database_url, &PoolConfig::default()).await?;

    // Run migrations
    match &pool {
        Pool::Sqlite(p) => {
            devbox_server::db::migrations::run_sqlite_migrations(p).await?;
        }
        Pool::Postgres(p) => {
            devbox_server::db::migrations::run_dsql_migrations(p).await?;
        }
    }

    let store = Arc::new(DocumentStore::new(pool));

    // Load reconciler config from environment. Instance type, AMI, subnets,
    // security groups, and pool max are owned by Terraform on the Launch Template
    // and ASG; the control plane only adopts the ASG and maintains warm capacity.
    let reconciler_config = ReconcilerConfig {
        pool_id: std::env::var("POOL_ID").unwrap_or_else(|_| "default".to_string()),
        server_id: uuid::Uuid::now_v7().to_string(),
        polling_interval: Duration::from_secs(
            std::env::var("POOL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        ),
        lock_ttl: Duration::from_secs(
            std::env::var("POOL_LOCK_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        ),
        ready_timeout: Duration::from_secs(
            std::env::var("POOL_READY_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        ),
        archive_timeout: Duration::from_secs(
            std::env::var("SESSION_ARCHIVE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        ),
    };

    reconciler_config
        .validate()
        .context("invalid reconciler configuration")?;

    // Load AWS config and create EC2 client. Retries are configured here, at the
    // SDK layer, so every compute call absorbs transient throttling/brownouts
    // without per-call retry loops — a momentary EC2 API failure must not stall
    // the warm pool (the reconciler simply continues on the next tick). The EC2
    // client also captures the SDK's region and stamps it onto each instance it
    // describes, so the CLI can open the SSM tunnel without client-side config.
    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(5))
        .load()
        .await;
    let ec2_client = Arc::new(Ec2::new(&aws_config));

    // Spawn reconciliation loop with cancellation support. The EC2 client is
    // shared with the API state so the claim handler can apply the owner tag
    // inline (see AppState::compute); the reconciler keeps the same client for
    // its periodic re-assertion.
    let cancel = CancellationToken::new();
    let reconcile_handle = spawn_reconciliation_loop(
        Arc::clone(&store),
        Arc::clone(&ec2_client),
        reconciler_config,
        cancel.clone(),
    );

    // The control plane's AWS account, advertised in the discovery document so
    // `devbox ssh` can auto-select the matching local AWS profile for the SSM
    // tunnel. Optional: when unset the CLI falls back to the caller's default
    // credentials, so behaviour is unchanged for deployments that omit it.
    let aws_account_id = std::env::var("AWS_ACCOUNT_ID")
        .ok()
        .filter(|s| !s.is_empty());

    // Build the GitHub token minter, which reads the App private key from SSM via
    // the task role. `None` when unconfigured (local/dev), so the server boots
    // without AWS and the agent git-token endpoint reports minting unavailable.
    let minter = devbox_server::github::Minter::from_env(&aws_config)
        .await
        .context("initialize GitHub token minter")?
        .map(Arc::new);

    // Session archiving: enabled iff DEVBOX_SESSION_BUCKET is set. The presigner
    // signs against the task role's S3 grants; hosts get URLs, never S3 IAM.
    let sessions = build_session_archives(&aws_config).map(Arc::new);

    // Build router. State is shared as a single Arc<AppState>.
    let app = build_router(Arc::new(AppState {
        store: Arc::clone(&store),
        auth: build_authenticator().await?,
        aws_account_id,
        minter,
        compute: Some(ec2_client),
        sessions,
    }));

    // Start server
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("listening on {}", addr);
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Signal the reconciliation loop to stop and wait for it
    cancel.cancel();
    let _join = reconcile_handle.await;

    // Cleanup
    store.pool().close().await;
    tracing::info!("server shut down");

    Ok(())
}

/// Build the session-archive presigner from the environment.
///
/// Enabled iff `DEVBOX_SESSION_BUCKET` is set (the companion Terraform sets it
/// on the ECS task). `SESSION_TTL_DAYS` (default 30) bounds how long a session
/// record survives; the bucket's lifecycle rule expires the objects on the
/// same clock.
fn build_session_archives(aws_config: &aws_config::SdkConfig) -> Option<SessionArchives> {
    let bucket = env_non_empty("DEVBOX_SESSION_BUCKET")?;
    let ttl_days = std::env::var("SESSION_TTL_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    tracing::info!(bucket = %bucket, ttl_days, "session archiving enabled");
    Some(SessionArchives::new(
        aws_sdk_s3::Client::new(aws_config),
        bucket,
        ttl_days,
    ))
}

/// Build the request authenticator, resolving OIDC endpoints from the issuer.
///
/// Authentication is always on: every claim/release binds `owner` to the
/// authenticated principal (the Unix login derived from the token's `email`
/// claim), so there is no unauthenticated path. The only OIDC config knob is
/// `AUTH_OIDC_ISSUER` (default Vouch); the JWKS URI and the dashboard
/// authorize / token / end-session endpoints are discovered once at startup
/// from `{issuer}/.well-known/openid-configuration`.
///
/// # Errors
///
/// Returns an error when discovery against the issuer fails after a bounded
/// retry — the server cannot verify any token without the issuer's JWKS, so it
/// fails fast rather than starting unable to authenticate.
async fn build_authenticator() -> Result<Authenticator> {
    // Normalize the issuer once at the source: the same trimmed value feeds the
    // discovery URL, the document issuer-match guard, and `AuthConfig.issuer`,
    // which token verification pins `iss` against by exact string match. A
    // slash-only difference here would otherwise reject every token whose `iss`
    // is the un-slashed IdP issuer even though the server booted fine.
    let issuer = std::env::var("AUTH_OIDC_ISSUER")
        .unwrap_or_else(|_| "https://us.vouch.sh".to_string())
        .trim_end_matches('/')
        .to_string();

    // Timeouts: a stalled issuer connection must fail fast rather than hang
    // startup forever (a hung request never returns an error, so it would bypass
    // discover_with_retry's retry loop entirely). Redirect::none: discovery and
    // JWKS fetches hit issuer-controlled URLs, so an open redirect at the IdP must
    // not be able to steer them elsewhere.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build OIDC discovery HTTP client")?;
    let endpoints = discover_with_retry(&http, &issuer).await?;

    let oidc = build_oidc_config(&endpoints)?;
    let agent = build_agent_auth_config(&http).await?;
    let config = AuthConfig {
        issuer,
        jwks_uri: endpoints.jwks_uri,
        alb_region: std::env::var("AWS_REGION").ok().filter(|s| !s.is_empty()),
        alb_arn: std::env::var("AUTH_ALB_ARN").ok().filter(|s| !s.is_empty()),
        oidc,
        agent,
    };
    tracing::info!(
        issuer = %config.issuer,
        dashboard_login = config.oidc.is_some(),
        agent_auth = config.agent.is_some(),
        "API authentication enabled (owner = email local part)"
    );
    Ok(Authenticator::new(config))
}

/// Build the agent-token trust config from the environment, or `Ok(None)` when
/// `DEVBOX_AGENT_OIDC_ISSUER` is unset (agent path disabled — `/api/v1/agent/*`
/// reports it unconfigured).
///
/// Agent tokens are AWS web-identity (IAM Outbound Identity Federation) JWTs; the
/// issuer's `jwks_uri` is discovered the same way as Vouch's. The other knobs are
/// non-secret trust parameters: the expected audience (the control-plane
/// resource), the platform AWS account (`AWS_ACCOUNT_ID`), and the trusted pool /
/// builder role ARNs. `DEVBOX_AGENT_ORG_ID` and `DEVBOX_AGENT_VPC_ID` are optional
/// defense-in-depth.
///
/// # Errors
///
/// Returns an error when `DEVBOX_AGENT_OIDC_ISSUER` is set but a required
/// companion (`DEVBOX_AGENT_AUDIENCE`, `AWS_ACCOUNT_ID`, `DEVBOX_POOL_ROLE_ARN`)
/// is missing, or issuer discovery fails — a misconfiguration that must fail fast
/// rather than silently leaving the agent path unverifiable.
async fn build_agent_auth_config(http: &reqwest::Client) -> Result<Option<AgentAuthConfig>> {
    let Some(issuer) = env_non_empty("DEVBOX_AGENT_OIDC_ISSUER") else {
        tracing::info!("agent OIDC auth disabled (DEVBOX_AGENT_OIDC_ISSUER unset)");
        return Ok(None);
    };
    // Trim as for the Vouch issuer above: the stored value feeds both discovery
    // and the `iss` pin in verify_agent_token, so a trailing slash must not leak in.
    let issuer = issuer.trim_end_matches('/').to_string();

    let require = |name: &str| -> Result<String> {
        env_non_empty(name).with_context(|| {
            format!("{name} is required when DEVBOX_AGENT_OIDC_ISSUER is set (agent auth)")
        })
    };
    // Trim trailing slashes to match the agent, which derives its token audience
    // from DEVBOX_SERVER_URL with trailing slashes trimmed. Without this, a
    // slash-only difference between the two env values would fail every `aud`
    // check even when both are "correctly" configured.
    let audience = require("DEVBOX_AGENT_AUDIENCE")?
        .trim_end_matches('/')
        .to_string();
    let platform_account_id = require("AWS_ACCOUNT_ID")?;

    // Trusted role ARNs are comma-separated: the pool is one role today, but there
    // are several builder roles (snapshot-builder + image-builder instances).
    let pool_role_arns = csv_list(&require("DEVBOX_POOL_ROLE_ARNS")?);
    if pool_role_arns.is_empty() {
        anyhow::bail!("DEVBOX_POOL_ROLE_ARNS must list at least one role ARN");
    }
    let builder_role_arns = env_non_empty("DEVBOX_BUILDER_ROLE_ARNS")
        .map(|raw| csv_list(&raw))
        .unwrap_or_default();

    let endpoints = discover_with_retry(http, &issuer).await?;
    tracing::info!(issuer = %issuer, "agent OIDC auth enabled (AWS web-identity tokens)");

    Ok(Some(AgentAuthConfig {
        issuer,
        jwks_uri: endpoints.jwks_uri,
        audience,
        platform_account_id,
        pool_role_arns,
        builder_role_arns,
        org_id: env_non_empty("DEVBOX_AGENT_ORG_ID"),
        vpc_id: env_non_empty("DEVBOX_AGENT_VPC_ID"),
    }))
}

/// Split a comma-separated env value into trimmed, non-empty items.
fn csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve the issuer's OIDC endpoints, retrying transient failures at boot.
///
/// Discovery is a hard startup dependency, so a momentary blip must not fail an
/// otherwise-healthy deploy: retry a few times with a short backoff before
/// giving up. The document is static and CDN-served, so a handful of attempts
/// covers real transients without masking a genuine misconfiguration.
async fn discover_with_retry(http: &reqwest::Client, issuer: &str) -> Result<OidcEndpoints> {
    const MAX_ATTEMPTS: u32 = 5;
    const BACKOFF: Duration = Duration::from_secs(1);

    let mut last_err: Option<AuthError> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match discover(http, issuer).await {
            Ok(endpoints) => return Ok(endpoints),
            Err(e) => {
                tracing::warn!(attempt, max = MAX_ATTEMPTS, error = %e, "OIDC discovery failed");
                last_err = Some(e);
            }
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(BACKOFF).await;
        }
    }

    let detail = last_err.map_or_else(|| "unknown error".to_string(), |e| e.to_string());
    Err(anyhow::anyhow!(
        "OIDC discovery against {issuer}/.well-known/openid-configuration failed \
         after {MAX_ATTEMPTS} attempts: {detail}"
    ))
}

/// Build the dashboard OIDC login config from the environment and discovered
/// endpoints.
///
/// Returns `Ok(Some)` only when `AUTH_OIDC_CLIENT_ID`, `AUTH_OIDC_CLIENT_SECRET`,
/// and `AUTH_OIDC_REDIRECT_URI` are all set; otherwise `Ok(None)` (the login page
/// shows an error — all dashboard routes require a valid session). The endpoints
/// come from discovery; only `AUTH_OIDC_SCOPE` is read here, defaulting to
/// `openid email`.
///
/// # Errors
///
/// Returns an error when the dashboard env vars are set but the issuer's
/// discovery document omits an endpoint the login flow needs
/// (`authorization_endpoint`, `token_endpoint`, or `end_session_endpoint`).
/// These are optional in [`OidcEndpoints`] so an API-only deployment boots
/// without them; once the dashboard is configured they become required, so a
/// missing one is a misconfiguration that fails fast with an actionable message.
fn build_oidc_config(endpoints: &OidcEndpoints) -> Result<Option<OidcConfig>> {
    let nonempty = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());

    let (Some(client_id), Some(client_secret), Some(redirect_uri)) = (
        nonempty("AUTH_OIDC_CLIENT_ID"),
        nonempty("AUTH_OIDC_CLIENT_SECRET"),
        nonempty("AUTH_OIDC_REDIRECT_URI"),
    ) else {
        return Ok(None);
    };

    let require = |name: &str, value: &Option<String>| -> Result<String> {
        value.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "dashboard OIDC is configured (AUTH_OIDC_* set) but the issuer's discovery \
                 document omits {name}; unset the AUTH_OIDC_* dashboard vars or use an issuer \
                 that publishes it"
            )
        })
    };

    Ok(Some(OidcConfig {
        client_id,
        client_secret: SecretString::from(client_secret),
        redirect_uri,
        authorize_endpoint: require("authorization_endpoint", &endpoints.authorization_endpoint)?,
        token_endpoint: require("token_endpoint", &endpoints.token_endpoint)?,
        end_session_endpoint: require("end_session_endpoint", &endpoints.end_session_endpoint)?,
        scope: nonempty("AUTH_OIDC_SCOPE").unwrap_or_else(|| "openid email".to_string()),
    }))
}

/// Wait for a shutdown signal (Ctrl+C or SIGTERM).
///
/// ECS/Fargate stops a task by sending SIGTERM (Docker's default `STOPSIGNAL`),
/// so listening only for SIGINT would skip the graceful-shutdown path and let the
/// kernel kill the process mid-request. A failed handler registration falls back
/// to a pending future rather than panicking.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                std::future::pending().await
            }
        }
    };

    tokio::select! {
        () = ctrl_c => tracing::info!("received SIGINT, initiating graceful shutdown"),
        () = terminate => tracing::info!("received SIGTERM, initiating graceful shutdown"),
    }
}

/// Redact password from database URL for logging.
fn redact_url(url: &str) -> String {
    if url.starts_with("sqlite:") {
        return url.to_string();
    }
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            if parsed.password().is_some() {
                let _set = parsed.set_password(Some("***"));
            }
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    }
}
