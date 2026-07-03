# Git Branching

Branch naming conventions for the project.
This file is read by `/rust-agents:solve-issue` to derive branch names from GitHub issues.

## Branch Naming

- Features: `feat/{issue-number}-{feature-slug}`
- Bug fixes: `fix/{issue-number}-{short-slug}`
- Hotfixes: `hotfix/{issue-number}-{short-slug}`
- If no issue exists, omit the issue-number segment: `feat/{feature-slug}`
- Examples: `feat/42-egress-proxy`, `fix/58-reconciler-leader-lock`, `hotfix/99-claim-race`

This project does not use milestone segments.

## Workflow

- Never push directly to `main` — all changes land via feature branches and PRs.
- Parallel subagents each work in their own worktree (`wt switch <branch>`), never the main repo.
- For each new issue, use `/rust-agents:solve-issue <number>` to create a branch and start development.

## Before Creating a PR

`make hooks` (or `prek install`) wires the project gate into git hooks:
- `cargo fmt` + file hygiene + `actionlint`/`zizmor`/`shellcheck` run on commit
- `cargo clippy` + `cargo test` run on push

If hooks are installed, these checks are automatic. Run manually for faster feedback:

```bash
make fmt    # cargo fmt --all
make lint   # cargo clippy --all-targets --all-features -- -D warnings
make test   # cargo test --all-features (in-memory SQLite; no AWS needed)
```

All three must be clean — clippy runs with `-D warnings` and the workspace denies
panic-prone patterns (`unwrap`/`expect`/`panic`/indexing/unchecked arithmetic).

PR descriptions describe what the code does now — not discarded approaches or prior
iterations. Plain, factual language; avoid words like "critical", "comprehensive",
"robust", "elegant".
