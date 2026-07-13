//! Devbox CLI client.

mod auth;
mod aws_profile;
mod command;
mod format;
mod session;
mod ssh;
mod ssm;
mod state;

use anyhow::Result;
use clap::{Parser, Subcommand};

use command::resolve_server;

/// Devbox CLI - manage remote development environments.
#[derive(Parser)]
#[command(name = "devbox", version, about)]
struct Cli {
    /// Server URL to connect to. Defaults to the server from your last
    /// `devbox login`, then to http://localhost:3000.
    #[arg(long, global = true, env = "DEVBOX_SERVER")]
    server: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate to the devbox server via device-code OAuth.
    Login,
    /// Forget the cached session (keeps the registered OAuth client).
    Logout,
    /// Claim an available devbox.
    Claim {
        /// Optional name for the box (lowercase letters, digits, '_' and '-').
        /// Leave unset to keep the auto-generated name.
        #[arg(long)]
        name: Option<String>,
        /// Restore a previously archived session onto the box (a session name
        /// or id from `devbox sessions`).
        #[arg(long)]
        resume: Option<String>,
    },
    /// Release a claimed devbox.
    Release {
        /// Devbox name or id to release (defaults to your active claim).
        target: Option<String>,
        /// Archive the session (git work-in-progress + agent context) before
        /// the box terminates, restorable later with `claim --resume`.
        #[arg(long)]
        keep: bool,
    },
    /// List your archived sessions (created by `release --keep`).
    Sessions,
    /// Rename a claimed devbox.
    Rename {
        /// Devbox name or id to rename.
        target: String,
        /// New name for the box (lowercase letters, digits, '_' and '-').
        new_name: String,
    },
    /// List all devboxes.
    List,
    /// Get status of a specific devbox.
    Status {
        /// Devbox name or id to check (defaults to your active claim).
        target: Option<String>,
    },
    /// SSH into a claimed devbox over an SSM tunnel.
    ///
    /// A dead or wedged tunnel is detected by an SSH keepalive and the session
    /// self-terminates after ~45 s, returning you to your shell. To disconnect a
    /// hung session immediately, press Enter and type `~.` (the SSH escape
    /// sequence; `~?` lists the rest).
    Ssh {
        /// Devbox name or id to connect to (defaults to your active claim).
        target: Option<String>,
        /// AWS profile for the SSM tunnel (auto-selected by the control-plane
        /// account when omitted).
        #[arg(long)]
        profile: Option<String>,
        /// Print the ssh command instead of running it.
        #[arg(long)]
        print: bool,
        /// Arguments passed through to ssh (e.g. a remote command after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Internal: native SSM data-channel proxy used as an ssh `ProxyCommand`.
    /// Not meant to be run directly; `devbox ssh` wires it up automatically.
    #[command(hide = true)]
    SsmProxy {
        /// Target EC2 instance id (ssh substitutes `%h`).
        #[arg(long)]
        target: String,
        /// AWS region the instance runs in.
        #[arg(long)]
        region: String,
        /// SSH port on the instance (ssh substitutes `%p`).
        #[arg(long, default_value_t = 22)]
        port: u16,
        /// AWS profile for SSM credentials.
        #[arg(long)]
        profile: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Redirect::none: OIDC discovery hits issuer-controlled URLs, so an open
    // redirect at the IdP must not be able to steer the login flow elsewhere.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let server = resolve_server(cli.server)?;

    match cli.command {
        Commands::Login => {
            command::cmd_login(&http, &server).await?;
        }
        Commands::Logout => {
            command::cmd_logout(&server)?;
        }
        Commands::Claim { name, resume } => {
            command::cmd_claim(&http, &server, name, resume).await?;
        }
        Commands::Release { target, keep } => {
            command::cmd_release(&http, &server, target, keep).await?;
        }
        Commands::Sessions => {
            command::cmd_sessions(&http, &server).await?;
        }
        Commands::Rename { target, new_name } => {
            command::cmd_rename(&http, &server, target, new_name).await?;
        }
        Commands::List => {
            command::cmd_list(&http, &server).await?;
        }
        Commands::Status { target } => {
            command::cmd_status(&http, &server, target).await?;
        }
        Commands::Ssh {
            target,
            profile,
            print,
            args,
        } => {
            command::cmd_ssh(&http, &server, target, profile, print, args).await?;
        }
        Commands::SsmProxy {
            target,
            region,
            port,
            profile,
        } => {
            command::cmd_ssm_proxy(&target, &region, port, profile.as_deref()).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_parses() {
        // Verify the CLI definition is valid (includes new Login/Logout subcommands).
        Cli::command().debug_assert();
    }

    #[test]
    fn claim_parses_name_flag() {
        let cli = Cli::try_parse_from(["devbox", "claim", "--name", "my-proj"]).unwrap();
        assert!(matches!(
            &cli.command,
            Commands::Claim { name, resume } if name.as_deref() == Some("my-proj") && resume.is_none()
        ));
    }

    #[test]
    fn rename_parses_mv_style_positionals() {
        let cli = Cli::try_parse_from(["devbox", "rename", "calm-quilt", "my-feature"]).unwrap();
        assert!(matches!(
            &cli.command,
            Commands::Rename { target, new_name }
                if target == "calm-quilt" && new_name == "my-feature"
        ));
    }

    #[test]
    fn rename_single_positional_is_error() {
        // Missing required <NEW_NAME> must be a parse error.
        assert!(Cli::try_parse_from(["devbox", "rename", "my-feature"]).is_err());
    }

    #[test]
    fn release_and_status_take_a_positional_target() {
        let rel = Cli::try_parse_from(["devbox", "release", "calm-quilt"]).unwrap();
        assert!(matches!(
            &rel.command,
            Commands::Release { target, keep } if target.as_deref() == Some("calm-quilt") && !keep
        ));
        let stat = Cli::try_parse_from(["devbox", "status", "calm-quilt"]).unwrap();
        assert!(matches!(
            &stat.command,
            Commands::Status { target } if target.as_deref() == Some("calm-quilt")
        ));
    }

    #[test]
    fn release_parses_keep_flag() {
        let cli = Cli::try_parse_from(["devbox", "release", "calm-quilt", "--keep"]).unwrap();
        assert!(matches!(
            &cli.command,
            Commands::Release { target, keep } if target.as_deref() == Some("calm-quilt") && *keep
        ));
    }

    #[test]
    fn claim_parses_resume_flag() {
        let cli = Cli::try_parse_from(["devbox", "claim", "--resume", "calm-quilt"]).unwrap();
        assert!(matches!(
            &cli.command,
            Commands::Claim { name, resume }
                if name.is_none() && resume.as_deref() == Some("calm-quilt")
        ));
    }

    #[test]
    fn sessions_parses() {
        let cli = Cli::try_parse_from(["devbox", "sessions"]).unwrap();
        assert!(matches!(&cli.command, Commands::Sessions));
    }

    #[test]
    fn ssh_separates_target_from_trailing_args() {
        // `devbox ssh <name> -- <cmd...>` must bind the name to `target` and the
        // post-`--` tokens to `args`, not fold them together.
        let cli =
            Cli::try_parse_from(["devbox", "ssh", "calm-quilt", "--", "uptime", "-l"]).unwrap();
        assert!(matches!(
            &cli.command,
            Commands::Ssh { target, args, .. }
                if target.as_deref() == Some("calm-quilt")
                    && args.len() == 2
                    && args.first().map(String::as_str) == Some("uptime")
        ));
    }

    #[test]
    fn ssh_without_target_defaults_to_active_claim() {
        let cli = Cli::try_parse_from(["devbox", "ssh"]).unwrap();
        assert!(matches!(
            &cli.command,
            Commands::Ssh { target, args, .. } if target.is_none() && args.is_empty()
        ));
    }
}
