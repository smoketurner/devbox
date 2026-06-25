//! `devbox ssh`: open an SSH session to a claimed devbox over an SSM tunnel.
//!
//! Pool instances have no public IP. We reach them by running the local `ssh`
//! client with a `ProxyCommand` that opens an `AWS-StartSSHSession` Session
//! Manager stream to the instance — no bastion, VPN, or public IP required.
//! Authentication is the caller's Vouch SSH certificate; the login user is the
//! certificate principal, which is the same `owner` the box was claimed with.

use anyhow::{Context, Result};

use devbox_common::DevboxResponse;

/// Options controlling how the SSH session is opened.
pub(crate) struct SshOptions {
    /// AWS profile for the SSM tunnel.
    pub profile: Option<String>,
    /// Print the ssh command instead of executing it.
    pub print: bool,
    /// Extra arguments passed through to ssh (e.g. a remote command).
    pub extra: Vec<String>,
}

/// Connect to `devbox` over SSM, replacing this process with `ssh` (Unix) or
/// spawning it and waiting (other platforms).
///
/// # Errors
///
/// Returns an error if the devbox is not claimed (no owner) or if `ssh` cannot
/// be launched.
pub(crate) fn connect(devbox: &DevboxResponse, opts: &SshOptions) -> Result<()> {
    let args = build_args(devbox, opts)?;

    if opts.print {
        println!("ssh {}", shell_join(&args));
        return Ok(());
    }

    exec_ssh(&args)
}

/// Build the ssh argument vector for reaching `devbox` over an SSM tunnel.
fn build_args(devbox: &DevboxResponse, opts: &SshOptions) -> Result<Vec<String>> {
    let instance_id = devbox.instance_id.as_str();

    // The login user is the certificate principal, which is the `owner` the box
    // was claimed with — never a caller override.
    let user = devbox
        .owner
        .clone()
        .with_context(|| format!("devbox {} is not claimed", devbox.id))?;

    // The devbox always carries the region it runs in. The region is baked into
    // the ProxyCommand because `ssh` only substitutes its own tokens (`%h`,
    // `%p`) — it cannot supply a region or profile — so `ssh` never depends on
    // the caller's ambient AWS config.
    let region = devbox.region.as_str();

    // `ssh` runs the ProxyCommand via `/bin/sh -c`, so shell-quote the executable
    // path (it may contain spaces) and the baked-in values. `%h`/`%p` stay
    // unquoted so `ssh` substitutes the instance id and port. The native proxy
    // replaces the external `aws ssm start-session` / `session-manager-plugin`.
    let exe = std::env::current_exe().context("failed to locate the devbox executable")?;
    let exe = exe
        .to_str()
        .context("devbox executable path is not valid UTF-8")?;

    let mut proxy = format!(
        "{} ssm-proxy --target %h --port %p --region {}",
        shell_quote(exe),
        shell_quote(region),
    );
    if let Some(profile) = opts.profile.as_deref() {
        proxy.push_str(&format!(" --profile {}", shell_quote(profile)));
    }

    let mut args = vec![
        "-o".to_string(),
        format!("ProxyCommand={proxy}"),
        format!("{user}@{instance_id}"),
    ];
    args.extend(opts.extra.iter().cloned());
    Ok(args)
}

/// Replace the current process with `ssh` so it owns the terminal directly.
#[cfg(unix)]
fn exec_ssh(args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // `exec` only returns on failure.
    let err = std::process::Command::new("ssh").args(args).exec();
    Err(anyhow::Error::new(err).context("failed to exec ssh"))
}

/// Spawn `ssh` and propagate its exit status (non-Unix fallback).
#[cfg(not(unix))]
fn exec_ssh(args: &[String]) -> Result<()> {
    let status = std::process::Command::new("ssh")
        .args(args)
        .status()
        .context("failed to run ssh")?;
    if !status.success() {
        anyhow::bail!("ssh exited with status {:?}", status.code());
    }
    Ok(())
}

/// Render an argument vector for display as a copy-pasteable shell command.
/// Shell-safe arguments pass through unquoted; anything else is single-quoted
/// with embedded single quotes escaped (`'\''`).
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote `arg` unless every character is shell-safe.
fn shell_quote(arg: &str) -> String {
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-' | '_')
    };
    if !arg.is_empty() && arg.chars().all(safe) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use devbox_common::{AmiId, DevboxState, InstanceType};

    fn claimed(instance_id: &str, owner: Option<&str>) -> DevboxResponse {
        DevboxResponse {
            id: "abc123".to_string(),
            instance_id: instance_id.to_string(),
            name: "calm-quilt".to_string(),
            state: DevboxState::Claimed,
            instance_type: InstanceType("m5.large".to_string()),
            ami_id: AmiId("ami-123".to_string()),
            owner: owner.map(str::to_string),
            region: "us-west-2".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            claimed_at: Some("2024-01-02T00:00:00Z".to_string()),
        }
    }

    fn opts() -> SshOptions {
        SshOptions {
            profile: None,
            print: false,
            extra: Vec::new(),
        }
    }

    #[test]
    fn builds_ssm_proxy_command_with_owner_as_login() {
        let devbox = claimed("i-0abc", Some("jdoe"));
        let args = build_args(&devbox, &opts()).expect("args");
        assert_eq!(args.first().map(String::as_str), Some("-o"));
        assert!(args.iter().any(|a| a == "jdoe@i-0abc"));
        // The ProxyCommand runs the native proxy; `ssh` substitutes %h/%p.
        assert!(args.iter().any(|a| {
            a.starts_with("ProxyCommand=")
                && a.contains("ssm-proxy --target %h --port %p")
                && a.contains("--region us-west-2")
        }));
    }

    #[test]
    fn profile_is_forwarded() {
        let devbox = claimed("i-0abc", Some("jdoe"));
        let mut o = opts();
        o.profile = Some("dev".to_string());
        let args = build_args(&devbox, &o).expect("args");
        let proxy = args
            .iter()
            .find(|a| a.starts_with("ProxyCommand="))
            .expect("proxy");
        assert!(proxy.contains("--profile dev"));
    }

    #[test]
    fn region_defaults_to_devbox_region() {
        // With no --region flag, the ProxyCommand is still fully specified from
        // the devbox's own region, so ssh never needs ambient AWS region config.
        let devbox = claimed("i-0abc", Some("jdoe"));
        let args = build_args(&devbox, &opts()).expect("args");
        let proxy = args
            .iter()
            .find(|a| a.starts_with("ProxyCommand="))
            .expect("proxy");
        assert!(proxy.contains("--region us-west-2"));
    }

    #[test]
    fn extra_args_are_appended() {
        let devbox = claimed("i-0abc", Some("jdoe"));
        let mut o = opts();
        o.extra = vec!["uptime".to_string()];
        let args = build_args(&devbox, &o).expect("args");
        assert_eq!(args.last().map(String::as_str), Some("uptime"));
    }

    #[test]
    fn errors_without_owner() {
        let devbox = claimed("i-0abc", None);
        assert!(build_args(&devbox, &opts()).is_err());
    }

    #[test]
    fn shell_join_escapes_embedded_single_quote() {
        let args = vec!["foo'bar baz".to_string()];
        assert_eq!(shell_join(&args), "'foo'\\''bar baz'");
    }

    #[test]
    fn shell_join_passes_safe_arg_unquoted() {
        let args = vec!["jdoe@i-0abc".to_string()];
        assert_eq!(shell_join(&args), "jdoe@i-0abc");
    }

    #[test]
    fn shell_join_quotes_whitespace_without_quote() {
        let args = vec!["a b".to_string()];
        assert_eq!(shell_join(&args), "'a b'");
    }
}
