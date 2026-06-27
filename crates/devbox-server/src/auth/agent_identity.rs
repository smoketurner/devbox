//! Map an AWS web-identity (IAM Outbound Identity Federation) token's claims to a
//! verified devbox-host identity.
//!
//! The token's signature, issuer, audience, and expiry are validated in
//! [`super::jwt`] against the AWS account's JWKS *before* this runs. Here we read
//! the **STS-asserted** claims — `sub` (the requesting role ARN) and the
//! `https://sts.amazonaws.com/` namespace AWS populates — and never the
//! caller-supplied `request_tags`, which an attacker on the box could set freely.

use serde_json::Value;

use devbox_common::InstanceId;

use super::jwt::AuthError;

/// AWS nests its identity-specific claims under this namespace in the token.
const STS_NAMESPACE: &str = "https://sts.amazonaws.com/";

/// Which kind of devbox host presented the token, resolved from the `sub` role ARN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// A warm-pool host.
    Pool,
    /// A snapshot-/image-builder host.
    Builder,
}

/// A verified devbox-host identity. Constructed only by [`from_claims`], after the
/// token's signature/issuer/audience have been validated — a value of this type
/// therefore attests a genuine, trusted devbox instance.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    /// The EC2 instance, from the STS-asserted `ec2_source_instance_arn` claim.
    pub instance_id: InstanceId,
    /// Pool vs builder, from the `sub` role ARN.
    pub role: AgentRole,
    /// The box's claimant when known (resolved from `DevboxDoc`, not the token);
    /// not required for git-token authorization (a warming box has no owner yet).
    pub owner: Option<String>,
}

/// Trust parameters for agent-token authorization. Configuration, not secrets.
#[derive(Clone, Debug)]
pub struct AgentAuthConfig {
    /// Account-specific AWS issuer URL (`DEVBOX_AGENT_OIDC_ISSUER`).
    pub issuer: String,
    /// JWKS URI discovered from the issuer.
    pub jwks_uri: String,
    /// Expected `aud`: the control-plane resource the agent requests a token for.
    pub audience: String,
    /// The platform AWS account; the token's `aws_account` must equal it.
    pub platform_account_id: String,
    /// IAM role ARNs of warm-pool hosts (`sub` ∈ this ⇒ [`AgentRole::Pool`]). The
    /// pool is one ASG/role today, but a list keeps it symmetric with builders.
    pub pool_role_arns: Vec<String>,
    /// IAM role ARNs of builder hosts (`sub` ∈ this ⇒ [`AgentRole::Builder`]).
    /// There is more than one — the snapshot-builder and image-builder instances
    /// run the agent under distinct roles.
    pub builder_role_arns: Vec<String>,
    /// Optional defense-in-depth: require the token's `org_id` to equal this.
    pub org_id: Option<String>,
    /// Optional defense-in-depth: require `ec2_instance_source_vpc` to equal this.
    pub vpc_id: Option<String>,
}

impl AgentAuthConfig {
    /// Resolve the [`AgentRole`] for a `sub` role ARN, or `None` if untrusted.
    fn role_for(&self, sub: &str) -> Option<AgentRole> {
        if self.pool_role_arns.iter().any(|arn| arn == sub) {
            Some(AgentRole::Pool)
        } else if self.builder_role_arns.iter().any(|arn| arn == sub) {
            Some(AgentRole::Builder)
        } else {
            None
        }
    }
}

/// Build a verified [`AgentIdentity`] from already-signature-verified token claims.
///
/// Reads only STS-asserted claims; the caller-supplied `request_tags` are never
/// consulted, so a box user cannot forge their instance/role by passing tags.
///
/// # Errors
///
/// [`AuthError::Invalid`] when `sub` is not a trusted role, the `aws_account`
/// doesn't match the platform account, the `ec2_source_instance_arn` claim is
/// absent or unparseable, or an enabled defense-in-depth check (org, vpc) fails.
pub(super) fn from_claims(
    claims: &Value,
    config: &AgentAuthConfig,
) -> Result<AgentIdentity, AuthError> {
    let sub = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthError::Invalid("agent token missing 'sub'".to_string()))?;
    let role = config.role_for(sub).ok_or_else(|| {
        AuthError::Invalid(format!(
            "agent token 'sub' is not a trusted devbox role: {sub}"
        ))
    })?;

    let sts = claims.get(STS_NAMESPACE).ok_or_else(|| {
        AuthError::Invalid(format!("agent token missing '{STS_NAMESPACE}' claims"))
    })?;

    let account = sts
        .get("aws_account")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::Invalid("agent token missing 'aws_account'".to_string()))?;
    if account != config.platform_account_id {
        return Err(AuthError::Invalid(format!(
            "agent token from unexpected AWS account {account}"
        )));
    }

    let instance_arn = sts
        .get("ec2_source_instance_arn")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AuthError::Invalid("agent token missing 'ec2_source_instance_arn'".to_string())
        })?;
    let instance_id = InstanceId::from_ec2_arn(instance_arn).ok_or_else(|| {
        AuthError::Invalid(format!(
            "agent token instance ARN not parseable: {instance_arn}"
        ))
    })?;

    if let Some(expected) = &config.org_id
        && sts.get("org_id").and_then(Value::as_str) != Some(expected.as_str())
    {
        return Err(AuthError::Invalid(
            "agent token AWS Organization mismatch".to_string(),
        ));
    }
    if let Some(expected) = &config.vpc_id
        && sts.get("ec2_instance_source_vpc").and_then(Value::as_str) != Some(expected.as_str())
    {
        return Err(AuthError::Invalid(
            "agent token source VPC mismatch".to_string(),
        ));
    }

    Ok(AgentIdentity {
        instance_id,
        role,
        owner: None,
    })
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use serde_json::json;

    const POOL_ROLE: &str = "arn:aws:iam::123456789012:role/devbox-pool";
    const BUILDER_ROLE: &str = "arn:aws:iam::123456789012:role/devbox-builder";
    const ACCOUNT: &str = "123456789012";
    const INSTANCE_ARN: &str = "arn:aws:ec2:us-east-1:123456789012:instance/i-abc123def456";

    fn config() -> AgentAuthConfig {
        AgentAuthConfig {
            issuer: "https://uuid.tokens.sts.global.api.aws".to_string(),
            jwks_uri: "https://uuid.tokens.sts.global.api.aws/.well-known/jwks.json".to_string(),
            audience: "https://cp.devbox.farm".to_string(),
            platform_account_id: ACCOUNT.to_string(),
            pool_role_arns: vec![POOL_ROLE.to_string()],
            builder_role_arns: vec![BUILDER_ROLE.to_string()],
            org_id: None,
            vpc_id: None,
        }
    }

    fn claims(sub: &str) -> Value {
        json!({
            "sub": sub,
            STS_NAMESPACE: {
                "aws_account": ACCOUNT,
                "ec2_source_instance_arn": INSTANCE_ARN,
            }
        })
    }

    #[test]
    fn pool_token_resolves_to_pool_and_instance() {
        let id = from_claims(&claims(POOL_ROLE), &config()).unwrap();
        assert_eq!(id.role, AgentRole::Pool);
        assert_eq!(id.instance_id, InstanceId("i-abc123def456".to_string()));
        assert_eq!(id.owner, None);
    }

    #[test]
    fn builder_token_resolves_to_builder() {
        let id = from_claims(&claims(BUILDER_ROLE), &config()).unwrap();
        assert_eq!(id.role, AgentRole::Builder);
    }

    #[test]
    fn untrusted_role_rejected() {
        let other = "arn:aws:iam::123456789012:role/some-other-role";
        assert!(from_claims(&claims(other), &config()).is_err());
    }

    #[test]
    fn foreign_account_rejected() {
        let c = json!({
            "sub": POOL_ROLE,
            STS_NAMESPACE: {
                "aws_account": "999999999999",
                "ec2_source_instance_arn": INSTANCE_ARN,
            }
        });
        assert!(from_claims(&c, &config()).is_err());
    }

    #[test]
    fn missing_instance_arn_rejected() {
        let c = json!({
            "sub": POOL_ROLE,
            STS_NAMESPACE: { "aws_account": ACCOUNT }
        });
        assert!(from_claims(&c, &config()).is_err());
    }

    #[test]
    fn request_tags_instance_hint_does_not_authorize() {
        // The instance hint lives only in caller-supplied request_tags, never the
        // STS-asserted ec2_source_instance_arn — must NOT authorize.
        let c = json!({
            "sub": POOL_ROLE,
            STS_NAMESPACE: {
                "aws_account": ACCOUNT,
                "request_tags": { "ec2_source_instance_arn": INSTANCE_ARN }
            }
        });
        assert!(from_claims(&c, &config()).is_err());
    }

    #[test]
    fn org_mismatch_rejected_when_configured() {
        let mut cfg = config();
        cfg.org_id = Some("o-expected".to_string());
        // Token carries no org_id at all.
        assert!(from_claims(&claims(POOL_ROLE), &cfg).is_err());
        // Token carries a different org.
        let c = json!({
            "sub": POOL_ROLE,
            STS_NAMESPACE: {
                "aws_account": ACCOUNT,
                "ec2_source_instance_arn": INSTANCE_ARN,
                "org_id": "o-different",
            }
        });
        assert!(from_claims(&c, &cfg).is_err());
    }

    #[test]
    fn vpc_match_accepted_mismatch_rejected_when_configured() {
        let mut cfg = config();
        cfg.vpc_id = Some("vpc-pool".to_string());
        let ok = json!({
            "sub": POOL_ROLE,
            STS_NAMESPACE: {
                "aws_account": ACCOUNT,
                "ec2_source_instance_arn": INSTANCE_ARN,
                "ec2_instance_source_vpc": "vpc-pool",
            }
        });
        assert!(from_claims(&ok, &cfg).is_ok());
        let bad = json!({
            "sub": POOL_ROLE,
            STS_NAMESPACE: {
                "aws_account": ACCOUNT,
                "ec2_source_instance_arn": INSTANCE_ARN,
                "ec2_instance_source_vpc": "vpc-elsewhere",
            }
        });
        assert!(from_claims(&bad, &cfg).is_err());
    }
}
