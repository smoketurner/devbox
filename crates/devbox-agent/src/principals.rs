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
    let owner = current_owner().await;
    if let Some(principal) = authorized_principal(owner.as_deref(), login_user) {
        println!("{principal}");
    }
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
async fn current_owner() -> Option<String> {
    let client = imds::client();
    let owner = imds::instance_tag(&client, "devbox:owner").await.ok()??;
    let owner = owner.trim();
    if owner.is_empty() {
        return None;
    }
    Some(owner.to_string())
}

#[cfg(test)]
mod tests {
    use super::authorized_principal;

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
