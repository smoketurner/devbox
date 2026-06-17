//! Devbox CLI client.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use devbox_common::{ClaimRequest, DevboxListResponse, DevboxResponse, ReleaseRequest};

/// Devbox CLI - manage remote development environments.
#[derive(Parser)]
#[command(name = "devbox", version, about)]
struct Cli {
    /// Server URL to connect to.
    #[arg(long, default_value = "http://localhost:3000", global = true)]
    server_url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Claim an available devbox.
    Claim {
        /// Your user/owner identifier.
        #[arg(long)]
        owner: String,
        /// Preferred instance type.
        #[arg(long)]
        instance_type: Option<String>,
    },
    /// Release a claimed devbox.
    Release {
        /// Devbox ID to release.
        #[arg(long)]
        id: String,
        /// Your user/owner identifier.
        #[arg(long)]
        owner: String,
    },
    /// List all devboxes.
    List,
    /// Get status of a specific devbox.
    Status {
        /// Devbox ID to check.
        #[arg(long)]
        id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    match cli.command {
        Commands::Claim {
            owner,
            instance_type,
        } => {
            let url = format!("{}/api/devboxes/claim", cli.server_url);
            let req = ClaimRequest {
                owner,
                instance_type,
            };
            let resp = client
                .post(&url)
                .json(&req)
                .send()
                .await
                .context("failed to send claim request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                println!("Claimed devbox: {}", devbox.id);
                println!("  State: {:?}", devbox.state);
                println!("  Instance: {}", devbox.instance_id.unwrap_or_default());
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("claim failed ({}): {}", status, body);
            }
        }
        Commands::Release { id, owner } => {
            let url = format!("{}/api/devboxes/{}/release", cli.server_url, id);
            let req = ReleaseRequest { owner };
            let resp = client
                .post(&url)
                .json(&req)
                .send()
                .await
                .context("failed to send release request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                println!("Released devbox: {}", devbox.id);
                println!("  State: {:?}", devbox.state);
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("release failed ({}): {}", status, body);
            }
        }
        Commands::List => {
            let url = format!("{}/api/devboxes", cli.server_url);
            let resp = client
                .get(&url)
                .send()
                .await
                .context("failed to send list request")?;

            if resp.status().is_success() {
                let list: DevboxListResponse =
                    resp.json().await.context("failed to parse response")?;
                if list.devboxes.is_empty() {
                    println!("No devboxes found.");
                } else {
                    for devbox in &list.devboxes {
                        println!(
                            "{} | {:?} | {} | owner: {}",
                            devbox.id,
                            devbox.state,
                            devbox.instance_type,
                            devbox.owner.as_deref().unwrap_or("-")
                        );
                    }
                }
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("list failed ({}): {}", status, body);
            }
        }
        Commands::Status { id } => {
            let url = format!("{}/api/devboxes/{}", cli.server_url, id);
            let resp = client
                .get(&url)
                .send()
                .await
                .context("failed to send status request")?;

            if resp.status().is_success() {
                let devbox: DevboxResponse =
                    resp.json().await.context("failed to parse response")?;
                println!("Devbox: {}", devbox.id);
                println!("  State: {:?}", devbox.state);
                println!("  Type: {}", devbox.instance_type);
                println!("  AMI: {}", devbox.ami_id);
                println!("  Instance: {}", devbox.instance_id.unwrap_or_default());
                println!("  Owner: {}", devbox.owner.unwrap_or_default());
                println!("  Created: {}", devbox.created_at);
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("status failed ({}): {}", status, body);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_parses() {
        // Verify the CLI definition is valid
        Cli::command().debug_assert();
    }
}
