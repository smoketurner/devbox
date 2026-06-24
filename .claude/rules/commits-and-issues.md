# Commit Messages and Issue Guidelines

This file is read by `rust-team`, `rust-code-reviewer`, and `/rust-agents:solve-issue`.

## Commit Message Format

Follow the [Conventional Commits 1.0.0 specification](https://www.conventionalcommits.org/en/v1.0.0/#specification).

### Structure

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

### Rules (from the spec)

1. Every commit MUST have a type prefix followed by a colon and space.
2. `BREAKING CHANGE:` footer or `!` after the type/scope signals a breaking change.
3. Types other than `fix` and `feat` are allowed; they do not imply a semver bump unless they include `BREAKING CHANGE`.
4. Scopes are optional; when used, they MUST be a noun in parentheses: `feat(reconcile): ...`
5. Description MUST follow the `type: ` prefix — use imperative, present tense, no period at end.
6. Body and footers are separated from the description by a blank line.
7. `fix` maps to a **PATCH** semver bump; `feat` maps to a **MINOR** semver bump; `BREAKING CHANGE` maps to a **MAJOR** bump.

### Allowed Types

| Type | Semver | Use for |
|------|--------|---------|
| `feat` | MINOR | New feature visible to the user |
| `fix` | PATCH | Bug fix visible to the user |
| `docs` | — | Documentation only |
| `style` | — | Formatting, whitespace — no logic change |
| `refactor` | — | Code restructure without behavior change |
| `test` | — | Adding or correcting tests |
| `build` | — | Build system, dependency updates |
| `ci` | — | CI/CD pipeline changes |
| `perf` | — | Performance improvement |
| `chore` | — | Housekeeping (version bumps, lock files) |

Likely scopes in this repo: `reconcile`, `compute`, `db`, `auth`, `ssh`, `ssm`,
`cli`, `agent`, `ui`, `routes`.

### Breaking Changes

```
feat!: drop support for the legacy x-amzn-oidc-data header

BREAKING CHANGE: the ALB OIDC path is removed; use bearer-token auth
```

### Examples

```
feat(ssh): open sessions over a native SSM data channel

fix(reconcile): skip the tick when the ASG is absent

docs: document the warm-pool readiness gate

chore: bump aws-sdk-ec2 to 1.233.0
```

### Anti-patterns

- Do not use past tense: ~~"added support"~~ → `feat: add support`
- Do not use vague types: ~~`update: ...`~~ — pick a specific type from the table
- Do not use emoji or marketing language ("critical", "comprehensive") in the subject or body
- The repo's required `Co-Authored-By:` trailer (appended automatically) is the
  only co-author/attribution line; do not add other AI-tooling mentions

## Issue Guidelines

### Severity Labels

| Severity | Label | Description | Action |
|----------|-------|-------------|--------|
| Critical | P0 | Broken core functionality, data loss, security | File immediately, dedicate fix session |
| High | P1 | Degraded UX, incorrect non-destructive behavior | File and prioritize for next PR |
| Medium | P2 | Suboptimal behavior, minor inconsistency | File with `bug` or `enhancement` |
| Low | P3 | Cosmetic, edge case unlikely in practice | Backlog |
| Nice-to-have | P4 | Research ideas, future enhancements | File with `research` label |

### Filing Protocol

1. **Reproduce** — confirm the issue is consistent, not a one-off fluke
2. **Check duplicates** before filing:
   ```bash
   gh issue list --state open --limit 100 --json number,title,labels
   ```
3. **File** via `gh issue create` with:
   - Title: short imperative description of the problem (not the fix)
   - Body: use the template below
   - Labels: priority label (P0–P4) + category (`bug`, `enhancement`, `research`)
4. **Link** related issues when they share a root cause

### Issue Title Conventions

- Describe the problem, not the fix: `reconciler reaps a claimed box` not `fix reaper`
- Use lowercase, no trailing period
- Be specific: mention the component or context if helpful

### Issue Body Template

```markdown
## Description
[What happened and why it matters]

## Reproduction Steps
1. [Step one]
2. [Step two]
3. Observe: [...]

## Expected Behavior
[What should happen]

## Actual Behavior
[What actually happened]

## Environment
- Version: [project version or commit]
- Features: [feature flags enabled]

## Logs / Evidence
[Relevant excerpts]
```

### Triage Rules

- Issues labeled `wontfix` or `duplicate` are skipped in future cycles
- When a previously filed issue is no longer reproducible, add a comment with verification result
- After a fix lands, re-run the original scenario and update the issue
