//! Git credential helper for reverse-proxy git traffic.
//!
//! owner-sync configures git to call `devbox-agent git-credential` for the control
//! plane host; this returns the box's web-identity token as the password, so the
//! GitHub credential is minted server-side and never held by the box.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use devbox_common::env_non_empty;

const SERVER_URL_ENV: &str = "DEVBOX_SERVER_URL";

/// Username git records; the reverse proxy ignores it (the token is the password).
const USERNAME: &str = "x-devbox";

/// Run the credential helper for `operation`. Only `get` responds; `store`/`erase`
/// are no-ops (the token is short-lived and must not be cached by git).
pub(crate) async fn run(operation: &str) {
    if operation != "get" {
        return;
    }
    let stdout = io::stdout();
    if let Err(e) = emit(io::stdin().lock(), stdout.lock()).await {
        // Fail closed and quiet: emit no credential, so git proceeds credential-less
        // and the fetch/push fails visibly rather than the helper hanging.
        eprintln!("devbox-agent git-credential: {e:#}");
    }
}

/// Drain git's request from `input`, mint a web-identity token, and write the
/// credential to `output`.
async fn emit(input: impl BufRead, output: impl Write) -> Result<()> {
    consume_request(input);
    let Some(audience) = env_non_empty(SERVER_URL_ENV) else {
        bail!("{SERVER_URL_ENV} unset");
    };
    let token =
        crate::control_plane::mint_web_identity_token(audience.trim_end_matches('/')).await?;
    write_credential(output, &token)
}

/// Write the `username`/`password` credential lines git expects.
fn write_credential(mut output: impl Write, token: &str) -> Result<()> {
    writeln!(output, "username={USERNAME}").context("write username")?;
    writeln!(output, "password={token}").context("write password")?;
    Ok(())
}

/// Read and discard git's credential request (attribute lines until a blank line
/// or EOF).
fn consume_request(input: impl BufRead) {
    for line in input.lines() {
        match line {
            Ok(l) if l.is_empty() => break,
            Ok(_) => {}
            Err(_) => break,
        }
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
    fn writes_credential_in_git_protocol() {
        let mut out = Vec::new();
        write_credential(&mut out, "the-token").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "username=x-devbox\npassword=the-token\n"
        );
    }

    #[test]
    fn consume_request_stops_at_blank_line() {
        // git ends the attribute list with a blank line; we must not block reading
        // past it (git may keep stdin open awaiting our response).
        let input = "protocol=https\nhost=cp.example\n\ntrailing";
        let mut cursor = io::Cursor::new(input);
        consume_request(&mut cursor);
        let mut rest = String::new();
        cursor.read_line(&mut rest).unwrap();
        assert_eq!(rest, "trailing");
    }
}
