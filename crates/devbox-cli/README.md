# devbox-cli

The `devbox` command-line client for [devbox](../../README.md). Talks to the
control-plane HTTP API to claim, release, inspect, and SSH into remote dev boxes.

## Commands

```
devbox claim   --owner <id> [--instance-type <t>]    # claim a Ready devbox
devbox release --id <id> --owner <id>                # release a claimed devbox
devbox list                                          # list all devboxes (table)
devbox status  --id <id>                             # one devbox, key/value
devbox ssh     --id <id> [--user <u>] [-- <cmd...>]  # SSH in over an SSM tunnel
```

Global flags: `--server-url` (default `http://localhost:3000`) and `--token`
(env `DEVBOX_TOKEN`) — a Vouch OIDC bearer token sent on API calls when the
server has authentication enabled.

## `devbox ssh`

Pool instances have no public IP. `devbox ssh` looks the devbox up, then runs the
local `ssh` client with a `ProxyCommand` that opens an `AWS-StartSSHSession`
Session Manager stream to the instance — no bastion, VPN, or public IP. The login
user defaults to the devbox `owner` (the Vouch certificate principal); the
connection is authenticated by the caller's Vouch SSH certificate. Trailing args
after `--` run as a remote command.

Requires the AWS `session-manager-plugin` locally and `ssm:StartSession` on the
target.

```bash
cargo build -p devbox-cli                 # builds the `devbox` binary
devbox --server-url https://… list
```
