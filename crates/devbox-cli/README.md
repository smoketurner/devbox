# devbox-cli

The `devbox` command-line client for [devbox](../../README.md). Talks to the
control-plane HTTP API to claim, release, inspect, and SSH into remote dev boxes.

## Commands

```
devbox login                                         # authenticate via device-code OAuth
devbox logout                                        # clear the cached session token
devbox claim   [--instance-type <t>]                 # claim a Ready devbox
devbox release [--id <id>]                           # release a claimed devbox
devbox list                                          # list all devboxes (table)
devbox status  [--id <id>]                           # one devbox, key/value
devbox ssh     [--id <id>] [--user <u>] [-- <cmd...>] # SSH in over an SSM tunnel
```

Global flag: `--server` (env `DEVBOX_SERVER`). Defaults to the server from your
last `devbox login`, or `http://localhost:3000` if never logged in. Run
`devbox login` once; the CLI caches your session under `~/.config/devbox/`
(keyed by server hostname so multiple servers stay logged in) and sends it
automatically on API calls. `claim` and `release` require a login — the owner is
always the authenticated principal (derived from your Vouch token's email claim),
never supplied by the client.

The `--id` flag is optional for `release`, `status`, and `ssh`. The CLI remembers
active claims locally; if you hold exactly one, it is used by default. With
multiple active claims, you'll be prompted to select one (or pass `--id` explicitly).

## `devbox ssh`

Pool instances have no public IP. `devbox ssh` looks the devbox up, then runs the
local `ssh` client with a `ProxyCommand` that opens an `AWS-StartSSHSession`
Session Manager stream to the instance — no bastion, VPN, or public IP. The login
user defaults to the devbox `owner` (the Vouch certificate principal); the
connection is authenticated by the caller's Vouch SSH certificate. Trailing args
after `--` run as a remote command.

The SSM Session Manager data-channel protocol is implemented natively in the CLI
(WebSocket over rustls/aws-lc-rs), so the AWS `session-manager-plugin` and the
`aws` CLI are **not** required — only the system `ssh` client and
`ssm:StartSession` on the target. The region and login user are read from the
devbox record; AWS credentials come from your environment or, when the
control-plane account can be matched, an auto-selected `~/.aws` profile (override
with `--region`/`--user`/`--profile`).

### IDE Remote-SSH (VS Code, JetBrains Gateway, Cursor)

These connect with the system `ssh` against a `~/.ssh/config` Host entry. Point
the `ProxyCommand` at `devbox ssm-proxy` instead of the old `aws ssm
start-session`:

```sshconfig
Host devbox-<id>
    HostName <instance-id>
    User <principal>
    ProxyCommand devbox ssm-proxy --target %h --port %p --region <region> [--profile <profile>]
    # Optional: collapse VS Code's several connections into one SSM session.
    ControlMaster auto
    ControlPersist 5m
```

(`devbox ssm-proxy` is the internal proxy `devbox ssh` wires up automatically; it
is not meant to be run by hand.)

```bash
cargo build -p devbox-cli                 # builds the `devbox` binary
devbox --server https://… list
```
