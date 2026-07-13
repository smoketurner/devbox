//! Presigned-URL access to the session-archive bucket.
//!
//! Devbox hosts have **no S3 IAM** — the control plane's task role holds the
//! only `s3:PutObject`/`s3:GetObject` grants, and hands the box short-lived
//! presigned URLs instead (upload at `release --keep`, download at
//! `claim --resume`). Presigning is offline SigV4: no AWS round-trip happens
//! here, so tests exercise real URLs with placeholder static credentials.

use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_s3::presigning::PresigningConfig;

/// How long a presigned URL stays valid. Generous enough for a slow upload of
/// a large archive; far shorter than the credentialed grant it derives from.
const PRESIGN_TTL: Duration = Duration::from_secs(900);

/// Presigner for the session-archive bucket.
pub struct SessionArchives {
    s3: aws_sdk_s3::Client,
    bucket: String,
    /// Session record TTL, in days (`SESSION_TTL_DAYS`). The bucket's
    /// lifecycle rule expires the S3 objects on the same clock.
    ttl_days: u32,
}

impl SessionArchives {
    pub fn new(s3: aws_sdk_s3::Client, bucket: String, ttl_days: u32) -> Self {
        Self {
            s3,
            bucket,
            ttl_days,
        }
    }

    /// When a session created at `now` ages out. Saturates at `now` on
    /// (practically unreachable) timestamp overflow rather than panicking.
    #[must_use]
    pub fn expires_at(&self, now: jiff::Timestamp) -> jiff::Timestamp {
        let ttl = jiff::SignedDuration::from_hours(24i64.saturating_mul(i64::from(self.ttl_days)));
        now.checked_add(ttl).unwrap_or(now)
    }

    /// The object key a session's archive lives at.
    #[must_use]
    pub fn object_key(session_id: &str) -> String {
        format!("sessions/{session_id}.tar.gz")
    }

    /// Presigned PUT URL for uploading a session archive.
    ///
    /// # Errors
    ///
    /// Returns an error if presigning fails (malformed config; no network I/O
    /// is involved).
    pub async fn presigned_put(&self, key: &str) -> Result<String> {
        let config = PresigningConfig::expires_in(PRESIGN_TTL)
            .context("invalid presigning configuration")?;
        let presigned = self
            .s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .context("failed to presign session upload URL")?;
        Ok(presigned.uri().to_string())
    }

    /// Presigned GET URL for downloading a session archive.
    ///
    /// # Errors
    ///
    /// Returns an error if presigning fails (malformed config; no network I/O
    /// is involved).
    pub async fn presigned_get(&self, key: &str) -> Result<String> {
        let config = PresigningConfig::expires_in(PRESIGN_TTL)
            .context("invalid presigning configuration")?;
        let presigned = self
            .s3
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .context("failed to presign session download URL")?;
        Ok(presigned.uri().to_string())
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    /// A presigner over placeholder static credentials. SigV4 presigning is
    /// pure computation, so no AWS access (or mock) is needed.
    async fn test_archives() -> SessionArchives {
        let creds =
            aws_sdk_s3::config::Credentials::new("AKIDEXAMPLE", "test-secret", None, None, "test");
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(creds)
            .build();
        SessionArchives::new(
            aws_sdk_s3::Client::from_conf(config),
            "devbox-sessions-test".to_string(),
            30,
        )
    }

    #[tokio::test]
    async fn expires_at_adds_the_ttl() {
        let archives = test_archives().await;
        let now = jiff::Timestamp::UNIX_EPOCH;
        let expires = archives.expires_at(now);
        assert_eq!(
            expires.as_second().saturating_sub(now.as_second()),
            30 * 24 * 3600
        );
    }

    #[test]
    fn object_key_is_namespaced() {
        assert_eq!(
            SessionArchives::object_key("0197-abc"),
            "sessions/0197-abc.tar.gz"
        );
    }

    #[tokio::test]
    async fn presigned_put_targets_bucket_key_and_is_signed() {
        let archives = test_archives().await;
        let url = archives
            .presigned_put("sessions/0197-abc.tar.gz")
            .await
            .unwrap();
        assert!(url.contains("devbox-sessions-test"));
        assert!(url.contains("sessions/0197-abc.tar.gz"));
        assert!(url.contains("X-Amz-Signature="));
    }

    #[tokio::test]
    async fn presigned_get_targets_bucket_key_and_is_signed() {
        let archives = test_archives().await;
        let url = archives
            .presigned_get("sessions/0197-abc.tar.gz")
            .await
            .unwrap();
        assert!(url.contains("devbox-sessions-test"));
        assert!(url.contains("sessions/0197-abc.tar.gz"));
        assert!(url.contains("X-Amz-Signature="));
    }
}
