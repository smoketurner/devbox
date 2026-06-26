//! `devbox ssh`: open an SSH session to a claimed devbox over an SSM tunnel.
//!
//! Pool instances have no public IP. We reach them by running the local `ssh`
//! client with a `ProxyCommand` that opens an `AWS-StartSSHSession` Session
//! Manager stream to the instance — no bastion, VPN, or public IP required.
//! Authentication is the caller's Vouch SSH certificate; the login user is the
//! certificate principal, which is the same `owner` the box was claimed with.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use devbox_common::DevboxResponse;

/// How long to wait for the box to become loginable before falling through.
///
/// A freshly-claimed box can take up to ~35s to become loginable:
/// reconciler tick → IMDS tag propagation → `owner-sync` 5s poll → `useradd`.
/// 60s gives comfortable headroom for the provisioning window.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-probe `ConnectTimeout` value (seconds) passed to ssh.
const PROBE_CONNECT_TIMEOUT_SECS: u64 = 15;

/// How long the ControlMaster socket lingers after the last channel exits.
///
/// The probe opens the master and exits; this window must outlast the
/// microsecond gap before the interactive `exec` joins the master. 10 s is
/// conservative — the gap is sub-millisecond in practice. The live interactive
/// channel keeps the master alive for the duration of the SSH session; this
/// only governs the post-logout linger of the ssm-proxy process.
const CONTROL_PERSIST_SECS: u64 = 10;

/// Options controlling how the SSH session is opened.
pub(crate) struct SshOptions {
    /// AWS profile for the SSM tunnel.
    pub profile: Option<String>,
    /// Print the ssh command instead of executing it.
    pub print: bool,
    /// Extra arguments passed through to ssh (e.g. a remote command).
    pub extra: Vec<String>,
}

/// The connection-args core: ssh options that appear before the destination,
/// plus the destination itself. Shared by the interactive and probe invocations
/// to avoid duplicating the ProxyCommand and identity setup.
struct SshCore {
    /// All `-o Key=Val` pairs that precede the destination (ProxyCommand + optional identity).
    pre_host: Vec<String>,
    /// The SSH destination: `user@instance-id`.
    destination: String,
}

impl SshCore {
    /// Full interactive argument vector: pre_host + destination + caller-supplied extra args.
    fn interactive_args(&self, extra: &[String]) -> Vec<String> {
        let mut args = self.pre_host.clone();
        args.push(self.destination.clone());
        args.extend_from_slice(extra);
        args
    }

    /// Probe argument vector: pre_host + probe-only options + destination + `true`.
    ///
    /// When a pinned identity is present, `pre_host` already contains
    /// `ControlMaster=auto` and `ControlPath=~/.ssh/devbox-ssm-%C`, so the
    /// probe establishes the multiplexing master. The interactive session that
    /// follows then joins that master — one SSM tunnel total.
    ///
    /// `ControlPersist=<N>s` is probe-only: it keeps the master socket alive
    /// for the gap between probe success and the interactive `exec`. The live
    /// interactive channel keeps the master alive during the session; this
    /// value only governs the post-logout linger (see [`CONTROL_PERSIST_SECS`]).
    ///
    /// `StrictHostKeyChecking=accept-new` adds the host key on first contact
    /// without prompting — appropriate for single-use cattle boxes reached over an
    /// IAM-authenticated SSM channel (instance-ids are unique per box, so there is
    /// never a key-mismatch; the only effect is an ever-growing `known_hosts`).
    ///
    /// **Known limitation:** a permanently-broken certificate (CA not trusted,
    /// principal ≠ owner, passphrase-encrypted on-disk key without an agent) yields
    /// the same `Permission denied (publickey)` as "not provisioned yet", so the
    /// probe waits the full budget before the fall-through. Bounded and acceptable;
    /// the explicit timeout message keeps it honest.
    fn probe_args(&self) -> Vec<String> {
        let mut args = self.pre_host.clone();
        args.extend([
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-o".to_string(),
            format!("ConnectTimeout={PROBE_CONNECT_TIMEOUT_SECS}"),
            "-o".to_string(),
            format!("ControlPersist={CONTROL_PERSIST_SECS}s"),
        ]);
        args.push(self.destination.clone());
        args.push("true".to_string());
        args
    }
}

/// Returns the sleep duration before the next probe attempt.
///
/// Schedule (seconds): 2, 4, 8, 16, 16, 16, … (capped at 16 s).
fn probe_backoff(attempt: u32) -> Duration {
    let secs = match attempt {
        0 => 2,
        1 => 4,
        2 => 8,
        _ => 16,
    };
    Duration::from_secs(secs)
}

/// Connect to `devbox` over SSM, replacing this process with `ssh` (Unix) or
/// spawning it and waiting (other platforms).
///
/// If the Vouch default key/cert pair exists at
/// `~/.ssh/id_ed25519_vouch{,-cert.pub}`, ssh is constrained to that single
/// identity (`-o IdentitiesOnly=yes`), preventing the "Too many authentication
/// failures" error caused by offering every agent key before the box's sshd
/// reaches the cert.
///
/// **Note on stale certs:** with `IdentitiesOnly=yes` and an on-disk
/// `CertificateFile`, ssh presents that on-disk cert exclusively — the
/// ssh-agent's potentially fresher cert is not offered. This is acceptable
/// because the gate requires the on-disk cert to exist, and Vouch keeps
/// the on-disk cert fresh via its credential renewal flow.
///
/// When a pinned identity is present, probes until the box accepts the cert
/// (up to ~60s of elapsed budget; a final in-flight probe can add up to
/// `ConnectTimeout` beyond that), absorbing the first-login provisioning
/// window (reconciler tick → IMDS propagation → `owner-sync useradd`) rather
/// than surfacing a confusing `Permission denied`. When no pinned identity
/// exists, the probe is skipped entirely — without `IdentitiesOnly=yes`,
/// probing would flood MaxAuthTries with every agent key and never succeed.
///
/// **SSH ControlMaster multiplexing (identity-present path only):** the probe
/// opens a multiplexing master (`ControlMaster=auto`, `ControlPath=~/.ssh/devbox-ssm-%C`),
/// and the interactive session that follows joins that master rather than
/// opening a second SSM tunnel — one `StartSession` + WebSocket total instead
/// of two. The `%C` token is expanded by `ssh` itself (hash of host+port+user),
/// so probe and interactive always resolve to the same socket. `ControlPersist`
/// keeps the master alive for ~10 s after the probe exits; the live interactive
/// channel extends it for the duration of the session. The master and its
/// `ssm-proxy` subprocess linger for up to `CONTROL_PERSIST_SECS` after logout.
/// When `identity` is `None`, no ControlMaster/ControlPath/ControlPersist
/// options are set anywhere.
///
/// `--print` short-circuits before probing: prints the interactive command and
/// returns without running any ssh process.
///
/// # Errors
///
/// Returns an error if the devbox is not claimed (no owner), if the probe ssh
/// process cannot be spawned, or if `ssh` cannot be launched for the
/// interactive session.
pub(crate) async fn connect(devbox: &DevboxResponse, opts: &SshOptions) -> Result<()> {
    let identity = vouch_identity();
    let core = build_core(devbox, opts, identity.as_ref())?;

    if opts.print {
        println!("ssh {}", shell_join(&core.interactive_args(&opts.extra)));
        return Ok(());
    }

    // Opening the SSM tunnel (and, on a fresh box, waiting out provisioning)
    // takes a moment with no output, so signal that work has started.
    eprintln!("connecting...");

    // Only probe when a pinned identity is available. Without `IdentitiesOnly=yes`
    // the probe would flood MaxAuthTries with every agent key and always fail,
    // adding a misleading ~60s delay before falling through to the same result.
    if identity.is_some() {
        let started = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            let probe = tokio::process::Command::new("ssh")
                .args(core.probe_args())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;

            match probe {
                Ok(s) if s.success() => break,
                Ok(_) => {
                    // Non-zero exit: box not yet ready; retry within budget. We
                    // announce the wait only here — after a probe has actually
                    // failed — so a box that is already up connects with no
                    // spurious "waiting" line.
                    let elapsed = started.elapsed();
                    if elapsed >= PROBE_TIMEOUT {
                        eprintln!(
                            "box still not accepting logins after {}s; \
                             launching ssh to surface the error",
                            elapsed.as_secs()
                        );
                        break;
                    }
                    eprintln!(
                        "waiting for box to finish provisioning... ({}s)",
                        elapsed.as_secs()
                    );
                    tokio::time::sleep(probe_backoff(attempt)).await;
                    attempt = attempt.saturating_add(1);
                }
                Err(e) => {
                    // Spawn error (ssh binary missing, permission denied on exec, etc.):
                    // this will not improve with retries — fail immediately.
                    return Err(
                        anyhow::Error::new(e).context("failed to spawn ssh for readiness probe")
                    );
                }
            }
        }
    }

    exec_ssh(&core.interactive_args(&opts.extra))
}

/// Build the connection-args core for reaching `devbox` over an SSM tunnel.
///
/// Returns the pre-host options (ProxyCommand + optional Vouch identity flags
/// + optional ControlMaster/ControlPath) and the SSH destination.
///
/// The caller extends these into interactive or probe argument vectors via
/// [`SshCore::interactive_args`] and [`SshCore::probe_args`].
///
/// When `identity` is `Some`, the identity options (`IdentitiesOnly`,
/// `IdentityFile`, `CertificateFile`) and the multiplexing options
/// (`ControlMaster=auto`, `ControlPath=~/.ssh/devbox-ssm-%C`) are appended to
/// `pre_host` so they appear in both the probe and interactive argv. The probe
/// then opens the ControlMaster; the interactive session joins it, avoiding a
/// second SSM tunnel. `ControlPersist` is added by [`SshCore::probe_args`]
/// only — it governs the post-logout linger of the master socket.
///
/// `identity` carries already-validated UTF-8 paths (see [`vouch_identity_for`]),
/// so a `Some` identity always yields the options — there is no in-band UTF-8
/// check here that could diverge from the caller's probe gate.
fn build_core(
    devbox: &DevboxResponse,
    opts: &SshOptions,
    identity: Option<&(String, String)>,
) -> Result<SshCore> {
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

    let mut pre_host = vec!["-o".to_string(), format!("ProxyCommand={proxy}")];

    // When the Vouch default key/cert pair is pinned (vouch_identity already
    // verified both files exist and their paths are valid UTF-8), constrain ssh
    // to that single identity so the box's sshd is not flooded with every agent
    // key before reaching the cert, and enable connection multiplexing so the
    // probe and interactive session share one SSM tunnel. When absent
    // (non-default path, cert-only in agent, non-UTF-8 home, etc.) fall back to
    // today's unconstrained behaviour, and connect() skips the probe.
    if let Some((key, cert)) = identity {
        pre_host.extend([
            "-o".to_string(),
            "IdentitiesOnly=yes".to_string(),
            "-o".to_string(),
            format!("IdentityFile={key}"),
            "-o".to_string(),
            format!("CertificateFile={cert}"),
            // ControlMaster=auto: probe opens the master; interactive joins it.
            // ControlPath uses ssh token %C (hash of host+port+user) so probe
            // and interactive always resolve to the same socket. ssh expands
            // both ~ and %C; we pass the literal so no Rust path computation is
            // needed and the sun_path limit is never an issue.
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            "ControlPath=~/.ssh/devbox-ssm-%C".to_string(),
        ]);
    }

    Ok(SshCore {
        pre_host,
        destination: format!("{user}@{instance_id}"),
    })
}

/// Build the full ssh argument vector for `devbox` over an SSM tunnel.
///
/// This is the interactive invocation: connection-args core plus `opts.extra`.
/// Use [`SshCore::probe_args`] (via [`build_core`]) for the readiness-probe argv.
#[cfg(test)]
fn build_args(
    devbox: &DevboxResponse,
    opts: &SshOptions,
    identity: Option<&(String, String)>,
) -> Result<Vec<String>> {
    let core = build_core(devbox, opts, identity)?;
    Ok(core.interactive_args(&opts.extra))
}

/// Resolve the Vouch default SSH key and certificate under `home`.
///
/// Returns `Some((key, cert))` only when both
/// `<home>/.ssh/id_ed25519_vouch` and `<home>/.ssh/id_ed25519_vouch-cert.pub`
/// exist on disk **and** their paths are valid UTF-8 (so they can be passed as
/// `ssh -o` values). Returns `None` if either is absent (non-default layout,
/// cert held only in the agent, etc.) or non-UTF-8 — callers fall back to
/// unconstrained identity selection and skip the readiness probe. Validating
/// UTF-8 here (rather than in the caller) keeps a single signal: a `Some`
/// identity always yields the `IdentitiesOnly` options, so the probe can never
/// run without them and flood every agent key.
fn vouch_identity_for(home: &Path) -> Option<(String, String)> {
    let key = home.join(".ssh").join("id_ed25519_vouch");
    let cert = home.join(".ssh").join("id_ed25519_vouch-cert.pub");
    if !(key.exists() && cert.exists()) {
        return None;
    }
    let key = key.into_os_string().into_string().ok()?;
    let cert = cert.into_os_string().into_string().ok()?;
    Some((key, cert))
}

/// Resolve the Vouch SSH identity from `$HOME` (falling back to `$USERPROFILE`).
///
/// Returns `None` when the home directory cannot be determined or when either
/// of the Vouch default paths is absent.
fn vouch_identity() -> Option<(String, String)> {
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)?;
    vouch_identity_for(&home)
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

    // -- Existing tests (updated to pass None for identity) --

    #[test]
    fn builds_ssm_proxy_command_with_owner_as_login() {
        let devbox = claimed("i-0abc", Some("jdoe"));
        let args = build_args(&devbox, &opts(), None).expect("args");
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
        let args = build_args(&devbox, &o, None).expect("args");
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
        let args = build_args(&devbox, &opts(), None).expect("args");
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
        let args = build_args(&devbox, &o, None).expect("args");
        assert_eq!(args.last().map(String::as_str), Some("uptime"));
    }

    #[test]
    fn errors_without_owner() {
        let devbox = claimed("i-0abc", None);
        assert!(build_args(&devbox, &opts(), None).is_err());
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

    // -- Tests for Vouch identity injection --

    #[test]
    fn identity_options_precede_destination() {
        let key = "/home/jdoe/.ssh/id_ed25519_vouch".to_string();
        let cert = "/home/jdoe/.ssh/id_ed25519_vouch-cert.pub".to_string();
        let devbox = claimed("i-0abc", Some("jdoe"));
        let args = build_args(&devbox, &opts(), Some(&(key, cert))).expect("args");

        let dest_pos = args
            .iter()
            .position(|a| a == "jdoe@i-0abc")
            .expect("destination in args");

        let id_only_pos = args
            .iter()
            .position(|a| a == "IdentitiesOnly=yes")
            .expect("IdentitiesOnly=yes in args");
        assert!(
            id_only_pos < dest_pos,
            "IdentitiesOnly=yes must precede destination"
        );

        let id_file_pos = args
            .iter()
            .position(|a| a.starts_with("IdentityFile="))
            .expect("IdentityFile in args");
        assert!(
            id_file_pos < dest_pos,
            "IdentityFile must precede destination"
        );

        let cert_file_pos = args
            .iter()
            .position(|a| a.starts_with("CertificateFile="))
            .expect("CertificateFile in args");
        assert!(
            cert_file_pos < dest_pos,
            "CertificateFile must precede destination"
        );

        // Verify the exact injected path values.
        assert!(
            args.iter()
                .any(|a| a == "IdentityFile=/home/jdoe/.ssh/id_ed25519_vouch"),
            "exact IdentityFile path must match"
        );
        assert!(
            args.iter()
                .any(|a| a == "CertificateFile=/home/jdoe/.ssh/id_ed25519_vouch-cert.pub"),
            "exact CertificateFile path must match"
        );
    }

    #[test]
    fn identity_none_omits_identity_options() {
        let devbox = claimed("i-0abc", Some("jdoe"));
        let args = build_args(&devbox, &opts(), None).expect("args");
        assert!(!args.iter().any(|a| a == "IdentitiesOnly=yes"));
        assert!(!args.iter().any(|a| a.starts_with("IdentityFile=")));
        assert!(!args.iter().any(|a| a.starts_with("CertificateFile=")));
    }

    // -- Tests for probe argv --

    #[test]
    fn probe_args_exclude_extra() {
        let devbox = claimed("i-0abc", Some("jdoe"));
        let mut o = opts();
        o.extra = vec!["uptime".to_string()];
        let core = build_core(&devbox, &o, None).expect("core");
        let probe = core.probe_args();
        assert!(
            !probe.iter().any(|a| a == "uptime"),
            "probe args must not include the extra user command"
        );
        assert!(
            probe.iter().any(|a| a == "BatchMode=yes"),
            "BatchMode=yes must be in probe"
        );
        assert!(
            probe
                .iter()
                .any(|a| a == "StrictHostKeyChecking=accept-new"),
            "StrictHostKeyChecking=accept-new must be in probe"
        );
        assert!(
            probe.iter().any(|a| a.starts_with("ConnectTimeout=")),
            "ConnectTimeout must be in probe"
        );
        assert_eq!(
            probe.last().map(String::as_str),
            Some("true"),
            "probe must end with 'true'"
        );
    }

    #[test]
    fn probe_args_destination_precedes_true() {
        let devbox = claimed("i-0abc", Some("jdoe"));
        let core = build_core(&devbox, &opts(), None).expect("core");
        let probe = core.probe_args();
        let dest_pos = probe
            .iter()
            .position(|a| a == "i-0abc@i-0abc" || a.ends_with("@i-0abc"))
            .expect("destination in probe args");
        let true_pos = probe
            .iter()
            .rposition(|a| a == "true")
            .expect("true in probe args");
        assert!(
            dest_pos < true_pos,
            "destination must precede 'true' in probe"
        );
    }

    // -- Tests for vouch_identity_for gating --

    #[test]
    fn vouch_identity_for_returns_none_when_both_absent() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("devbox-ssh-test-{pid}-none"));
        std::fs::create_dir_all(&dir).expect("create tmpdir");
        assert!(vouch_identity_for(&dir).is_none(), "no files => None");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn vouch_identity_for_returns_none_when_only_key() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("devbox-ssh-test-{pid}-key-only"));
        let ssh_dir = dir.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).expect("create tmpdir");
        std::fs::File::create(ssh_dir.join("id_ed25519_vouch")).expect("create key");
        assert!(vouch_identity_for(&dir).is_none(), "key only => None");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn vouch_identity_for_returns_none_when_only_cert() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("devbox-ssh-test-{pid}-cert-only"));
        let ssh_dir = dir.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).expect("create tmpdir");
        std::fs::File::create(ssh_dir.join("id_ed25519_vouch-cert.pub")).expect("create cert");
        assert!(vouch_identity_for(&dir).is_none(), "cert only => None");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn vouch_identity_for_returns_some_when_both_present() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("devbox-ssh-test-{pid}-both"));
        let ssh_dir = dir.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).expect("create tmpdir");
        std::fs::File::create(ssh_dir.join("id_ed25519_vouch")).expect("create key");
        std::fs::File::create(ssh_dir.join("id_ed25519_vouch-cert.pub")).expect("create cert");
        let result = vouch_identity_for(&dir);
        let (key, cert) = result.expect("both files present => Some");
        assert_eq!(
            key,
            ssh_dir
                .join("id_ed25519_vouch")
                .to_str()
                .expect("utf8 key path")
        );
        assert_eq!(
            cert,
            ssh_dir
                .join("id_ed25519_vouch-cert.pub")
                .to_str()
                .expect("utf8 cert path")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- Test for probe_backoff schedule --

    #[test]
    fn probe_backoff_schedule() {
        assert_eq!(probe_backoff(0), Duration::from_secs(2));
        assert_eq!(probe_backoff(1), Duration::from_secs(4));
        assert_eq!(probe_backoff(2), Duration::from_secs(8));
        assert_eq!(probe_backoff(3), Duration::from_secs(16));
        assert_eq!(probe_backoff(4), Duration::from_secs(16));
        assert_eq!(probe_backoff(5), Duration::from_secs(16));
    }

    // -- Tests for ControlMaster multiplexing --

    #[test]
    fn control_master_present_in_both_args_when_identity_some() {
        let key = "/home/jdoe/.ssh/id_ed25519_vouch".to_string();
        let cert = "/home/jdoe/.ssh/id_ed25519_vouch-cert.pub".to_string();
        let devbox = claimed("i-0abc", Some("jdoe"));
        let core = build_core(&devbox, &opts(), Some(&(key, cert))).expect("core");

        let interactive = core.interactive_args(&[]);
        assert!(
            interactive.iter().any(|a| a == "ControlMaster=auto"),
            "ControlMaster=auto must be in interactive_args when identity is Some"
        );
        assert!(
            interactive
                .iter()
                .any(|a| a == "ControlPath=~/.ssh/devbox-ssm-%C"),
            "ControlPath must be in interactive_args when identity is Some"
        );

        let probe = core.probe_args();
        assert!(
            probe.iter().any(|a| a == "ControlMaster=auto"),
            "ControlMaster=auto must be in probe_args when identity is Some"
        );
        assert!(
            probe
                .iter()
                .any(|a| a == "ControlPath=~/.ssh/devbox-ssm-%C"),
            "ControlPath must be in probe_args when identity is Some"
        );
    }

    #[test]
    fn control_master_absent_when_identity_none() {
        let devbox = claimed("i-0abc", Some("jdoe"));
        let core = build_core(&devbox, &opts(), None).expect("core");
        let interactive = core.interactive_args(&[]);

        assert!(
            !interactive.iter().any(|a| a == "ControlMaster=auto"),
            "ControlMaster must be absent when identity is None"
        );
        assert!(
            !interactive
                .iter()
                .any(|a| a == "ControlPath=~/.ssh/devbox-ssm-%C"),
            "ControlPath must be absent when identity is None"
        );
        assert!(
            !interactive.iter().any(|a| a.starts_with("ControlPersist=")),
            "ControlPersist must be absent when identity is None"
        );
    }

    #[test]
    fn control_persist_in_probe_but_not_interactive() {
        let key = "/home/jdoe/.ssh/id_ed25519_vouch".to_string();
        let cert = "/home/jdoe/.ssh/id_ed25519_vouch-cert.pub".to_string();
        let devbox = claimed("i-0abc", Some("jdoe"));
        let core = build_core(&devbox, &opts(), Some(&(key, cert))).expect("core");

        let probe = core.probe_args();
        assert!(
            probe.iter().any(|a| a.starts_with("ControlPersist=")),
            "ControlPersist must appear in probe_args"
        );

        let interactive = core.interactive_args(&[]);
        assert!(
            !interactive.iter().any(|a| a.starts_with("ControlPersist=")),
            "ControlPersist must NOT appear in interactive_args"
        );
    }

    #[test]
    fn control_persist_value_matches_const() {
        let key = "/home/jdoe/.ssh/id_ed25519_vouch".to_string();
        let cert = "/home/jdoe/.ssh/id_ed25519_vouch-cert.pub".to_string();
        let devbox = claimed("i-0abc", Some("jdoe"));
        let core = build_core(&devbox, &opts(), Some(&(key, cert))).expect("core");
        let probe = core.probe_args();
        let expected = format!("ControlPersist={CONTROL_PERSIST_SECS}s");
        assert!(
            probe.contains(&expected),
            "ControlPersist value must match CONTROL_PERSIST_SECS ({CONTROL_PERSIST_SECS}s)"
        );
    }
}
