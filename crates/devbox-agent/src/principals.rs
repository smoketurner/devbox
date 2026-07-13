//! `AuthorizedPrincipalsCommand` resolver for sshd.
//!
//! sshd invokes `devbox-agent principals <login-user>` as `nobody` on every
//! authentication and treats each printed line as a principal authorized to log
//! in as `<login-user>`. A devbox is generic until claimed, so authorization is
//! bound to the `devbox:owner` instance tag (the claimant's Vouch principal):
//! we print the owner only when it equals the requested login user, which both
//! authorizes the certificate principal and pins the login account to it.
//!
//! Fail closed: any error, an absent tag, or a mismatch prints nothing, so sshd
//! authorizes no principals and rejects the login. Logins are also held while
//! a session restore is rewriting `/workspace` (`claim --resume`), so the
//! claimant cannot race the restore — the gate self-expires, so a crashed
//! restore only delays logins.

use std::path::Path;

use crate::imds;
use crate::owner_sync::{RESTORE_GATE, RESTORE_GATE_MAX_AGE, restore_gate_active};

/// Print the authorized principal for `login_user`, or nothing.
pub(crate) async fn run(login_user: &str) {
    if restore_gate_active(Path::new(RESTORE_GATE), RESTORE_GATE_MAX_AGE) {
        return;
    }
    let client = imds::client();
    // A box tagged devbox:archive-session was released (release --keep): its
    // owner no longer logs in, and a login mid-pack could mutate the checkout
    // and corrupt the archive. Fail closed on a read error — the owner read
    // below would fail the same way.
    match imds::instance_tag(&client, "devbox:archive-session").await {
        Ok(tag) if !archiving_requested(tag.as_deref()) => {}
        _ => return,
    }
    let owner = current_owner(&client).await;
    if let Some(principal) = authorized_principal(owner.as_deref(), login_user) {
        println!("{principal}");
    }
}

/// Whether an archive-session tag value means the box owes an archive. Pure
/// (no I/O) so the rule can be unit-tested without IMDS.
fn archiving_requested(tag: Option<&str>) -> bool {
    tag.is_some_and(|v| !v.trim().is_empty())
}

/// The principal authorized to log in as `login_user`, given the current
/// `devbox:owner` tag. Fail closed: an absent owner or any mismatch authorizes
/// no one. Pure (no I/O) so the rule can be unit-tested without IMDS.
fn authorized_principal<'a>(owner: Option<&'a str>, login_user: &str) -> Option<&'a str> {
    let owner = owner?;
    if owner == login_user {
        Some(owner)
    } else {
        None
    }
}

/// Read the `devbox:owner` tag from IMDS, returning `None` on any failure.
async fn current_owner(client: &aws_config::imds::client::Client) -> Option<String> {
    let owner = imds::instance_tag(client, "devbox:owner").await.ok()??;
    let owner = owner.trim();
    if owner.is_empty() {
        return None;
    }
    Some(owner.to_string())
}

#[cfg(test)]
mod tests {
    use super::{archiving_requested, authorized_principal};

    #[test]
    fn archive_request_gates_on_non_empty_tag() {
        assert!(!archiving_requested(None));
        assert!(!archiving_requested(Some("")));
        assert!(!archiving_requested(Some("   ")));
        assert!(archiving_requested(Some("sess-123")));
    }

    #[test]
    fn absent_owner_authorizes_no_one() {
        assert_eq!(authorized_principal(None, "jdoe"), None);
    }

    #[test]
    fn mismatched_owner_authorizes_no_one() {
        assert_eq!(authorized_principal(Some("alice"), "jdoe"), None);
    }

    #[test]
    fn matching_owner_is_authorized() {
        assert_eq!(authorized_principal(Some("jdoe"), "jdoe"), Some("jdoe"));
    }
}
