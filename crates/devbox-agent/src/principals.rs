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
//! authorizes no principals and rejects the login.

use crate::imds;

/// Print the authorized principal for `login_user`, or nothing.
pub(crate) fn run(login_user: &str) {
    if let Some(owner) = current_owner()
        && owner == login_user
    {
        println!("{owner}");
    }
}

/// Read the `devbox:owner` tag from IMDS, returning `None` on any failure.
fn current_owner() -> Option<String> {
    let token = imds::fetch_token().ok()?;
    let owner = imds::instance_tag(&token, "devbox:owner").ok()??;
    let owner = owner.trim();
    if owner.is_empty() {
        return None;
    }
    Some(owner.to_string())
}
