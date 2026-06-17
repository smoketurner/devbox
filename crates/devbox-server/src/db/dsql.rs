//! Aurora DSQL authentication token generation.
//!
//! This module provides functions for connecting to Aurora DSQL clusters
//! using IAM-based authentication.

use anyhow::{Context, Result, bail};
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_sdk_dsql::auth_token::{AuthTokenGenerator, Config};
use sqlx::postgres::PgSslMode;

/// Load AWS SDK config with credential chain support.
pub(crate) async fn load_sdk_config(region: Option<&str>) -> SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(r) = region {
        loader = loader.region(Region::new(r.to_string()));
    }
    loader.load().await
}

/// Generate a DSQL authentication token using AWS credentials.
///
/// # Errors
///
/// Returns an error if token generation fails.
pub(crate) async fn generate_dsql_token(
    sdk_config: &SdkConfig,
    cluster_endpoint: &str,
    region: &str,
    is_admin: bool,
) -> Result<String> {
    let config = Config::builder()
        .hostname(cluster_endpoint)
        .region(Region::new(region.to_string()))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build DSQL auth token config: {e}"))?;

    let signer = AuthTokenGenerator::new(config);

    let token = if is_admin {
        signer
            .db_connect_admin_auth_token(sdk_config)
            .await
            .map_err(|e| anyhow::anyhow!("failed to generate admin DSQL token: {e}"))?
    } else {
        signer
            .db_connect_auth_token(sdk_config)
            .await
            .map_err(|e| anyhow::anyhow!("failed to generate DSQL token: {e}"))?
    };

    Ok(token.to_string())
}

/// Extract AWS region from a DSQL endpoint hostname.
pub(crate) fn extract_region_from_endpoint(endpoint: &str) -> Option<&str> {
    let parts: Vec<&str> = endpoint.split('.').collect();
    if parts.len() >= 5 && parts.get(1) == Some(&"dsql") && parts.get(4) == Some(&"aws") {
        parts.get(2).copied()
    } else {
        None
    }
}

/// Check if a hostname is a DSQL endpoint.
pub(crate) fn is_dsql_endpoint(host: &str) -> bool {
    host.contains(".dsql.") && host.ends_with(".on.aws")
}

// ============================================================================
// DsqlEndpoint enum
// ============================================================================

/// Parsed DSQL connection endpoint.
#[derive(Debug, Clone)]
pub(crate) enum DsqlEndpoint {
    /// Direct DSQL connection: `cluster-id.dsql.region.on.aws`
    Direct {
        hostname: String,
        region: String,
    },
    /// VPC PrivateLink connection
    VpcEndpoint {
        hostname: String,
        region: String,
        cluster_id: String,
        auth_hostname: String,
    },
}

impl DsqlEndpoint {
    /// Parse a database URL and determine if it's a DSQL endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if URL parsing fails or VPC endpoint is missing cluster_id.
    pub(crate) fn from_url(url: &str) -> Result<Option<Self>> {
        let parsed = url::Url::parse(url).context("failed to parse database URL")?;
        let host = parsed.host_str().unwrap_or("");

        if is_dsql_endpoint(host) {
            let region = extract_region_from_endpoint(host)
                .context("failed to extract region from DSQL endpoint")?;
            return Ok(Some(Self::Direct {
                hostname: host.to_string(),
                region: region.to_string(),
            }));
        }

        if is_vpc_endpoint(host) {
            let cluster_id = parsed
                .query_pairs()
                .find(|(k, _)| k == "dsql_cluster_id")
                .map(|(_, v)| v.to_string())
                .context(
                    "VPC endpoint URL requires a 'dsql_cluster_id' query parameter",
                )?;

            if cluster_id.is_empty() {
                bail!("dsql_cluster_id query parameter must not be empty");
            }

            let region = extract_region_from_vpc_endpoint(host)
                .context("failed to extract region from VPC endpoint hostname")?;

            let service_id = extract_service_id_from_vpc_endpoint(host)
                .context("failed to extract DSQL service ID from VPC endpoint hostname")?;

            let auth_hostname = format!("{cluster_id}.{service_id}.{region}.on.aws");

            return Ok(Some(Self::VpcEndpoint {
                hostname: host.to_string(),
                region: region.to_string(),
                cluster_id,
                auth_hostname,
            }));
        }

        Ok(None)
    }

    /// The hostname to connect to.
    #[must_use]
    pub(crate) fn connect_hostname(&self) -> &str {
        match self {
            Self::Direct { hostname, .. } | Self::VpcEndpoint { hostname, .. } => hostname,
        }
    }

    /// The hostname to use for IAM token generation.
    #[must_use]
    pub(crate) fn token_hostname(&self) -> &str {
        match self {
            Self::Direct { hostname, .. } => hostname,
            Self::VpcEndpoint { auth_hostname, .. } => auth_hostname,
        }
    }

    /// The AWS region for this DSQL endpoint.
    #[must_use]
    pub(crate) fn region(&self) -> &str {
        match self {
            Self::Direct { region, .. } | Self::VpcEndpoint { region, .. } => region,
        }
    }

    /// The SSL mode to use for the connection.
    #[must_use]
    pub(crate) fn ssl_mode(&self) -> PgSslMode {
        match self {
            Self::Direct { .. } => PgSslMode::VerifyFull,
            Self::VpcEndpoint { .. } => PgSslMode::Require,
        }
    }

    /// The DSQL cluster ID, if this is a VPC endpoint connection.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn cluster_id(&self) -> Option<&str> {
        match self {
            Self::Direct { .. } => None,
            Self::VpcEndpoint { cluster_id, .. } => Some(cluster_id),
        }
    }

    /// Connection options required for DSQL to identify the cluster.
    #[must_use]
    pub(crate) fn pg_options(&self) -> Option<(&str, &str)> {
        match self {
            Self::Direct { .. } => None,
            Self::VpcEndpoint { cluster_id, .. } => Some(("amzn-cluster-id", cluster_id)),
        }
    }
}

/// Check if a hostname is a VPC PrivateLink endpoint.
fn is_vpc_endpoint(host: &str) -> bool {
    host.contains(".vpce.amazonaws.")
}

/// Find the position of the `vpce` segment in a dot-split hostname.
fn find_vpce_position(parts: &[&str]) -> Option<usize> {
    parts
        .iter()
        .position(|&p| p == "vpce")
        .filter(|&i| parts.get(i.saturating_add(1)) == Some(&"amazonaws"))
}

/// Extract the AWS region from a VPC endpoint hostname.
fn extract_region_from_vpc_endpoint(host: &str) -> Option<String> {
    let parts: Vec<&str> = host.split('.').collect();
    let vpce_idx = find_vpce_position(&parts)?;
    if vpce_idx == 0 {
        return None;
    }
    parts
        .get(vpce_idx.saturating_sub(1))
        .map(|s| (*s).to_string())
}

/// Extract the DSQL service ID from a VPC endpoint hostname.
fn extract_service_id_from_vpc_endpoint(host: &str) -> Option<String> {
    let parts: Vec<&str> = host.split('.').collect();
    find_vpce_position(&parts)?;
    if parts.len() >= 4 {
        parts.get(1).map(|s| (*s).to_string())
    } else {
        None
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_region_from_endpoint() {
        assert_eq!(
            extract_region_from_endpoint("abc123.dsql.us-east-1.on.aws"),
            Some("us-east-1")
        );
        assert_eq!(
            extract_region_from_endpoint("xyz789.dsql.eu-west-2.on.aws"),
            Some("eu-west-2")
        );
    }

    #[test]
    fn test_extract_region_from_endpoint_invalid() {
        assert_eq!(extract_region_from_endpoint("localhost"), None);
        assert_eq!(extract_region_from_endpoint("db.example.com"), None);
    }

    #[test]
    fn test_is_dsql_endpoint() {
        assert!(is_dsql_endpoint("abc123.dsql.us-east-1.on.aws"));
        assert!(!is_dsql_endpoint("localhost"));
        assert!(!is_dsql_endpoint("db.example.com"));
    }

    #[test]
    fn test_from_url_direct_dsql() {
        let url = "postgres://admin@abc123.dsql.us-east-1.on.aws/postgres";
        let ep = DsqlEndpoint::from_url(url).unwrap().unwrap();
        assert!(matches!(ep, DsqlEndpoint::Direct { .. }));
        assert_eq!(ep.connect_hostname(), "abc123.dsql.us-east-1.on.aws");
        assert_eq!(ep.token_hostname(), "abc123.dsql.us-east-1.on.aws");
        assert_eq!(ep.region(), "us-east-1");
        assert!(matches!(ep.ssl_mode(), PgSslMode::VerifyFull));
        assert!(ep.cluster_id().is_none());
        assert!(ep.pg_options().is_none());
    }

    #[test]
    fn test_from_url_vpc_endpoint() {
        let url = "postgres://user@vpce-0abc123.dsql-fnh4.us-east-1.vpce.amazonaws.com/postgres?dsql_cluster_id=mycluster";
        let ep = DsqlEndpoint::from_url(url).unwrap().unwrap();
        assert!(matches!(ep, DsqlEndpoint::VpcEndpoint { .. }));
        assert_eq!(
            ep.connect_hostname(),
            "vpce-0abc123.dsql-fnh4.us-east-1.vpce.amazonaws.com"
        );
        assert_eq!(ep.token_hostname(), "mycluster.dsql-fnh4.us-east-1.on.aws");
        assert_eq!(ep.region(), "us-east-1");
        assert!(matches!(ep.ssl_mode(), PgSslMode::Require));
        assert_eq!(ep.cluster_id(), Some("mycluster"));
    }

    #[test]
    fn test_from_url_plain_postgres() {
        let url = "postgres://user:pass@localhost:5432/mydb";
        let ep = DsqlEndpoint::from_url(url).unwrap();
        assert!(ep.is_none());
    }

    #[test]
    fn test_from_url_vpc_endpoint_missing_cluster_id() {
        let url = "postgres://user@vpce-0abc123.dsql-fnh4.us-east-1.vpce.amazonaws.com/postgres";
        let err = DsqlEndpoint::from_url(url).unwrap_err();
        assert!(err.to_string().contains("dsql_cluster_id"));
    }
}
