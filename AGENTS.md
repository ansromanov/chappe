# AGENTS.md — chappe

> Canonical instructions for this repository, read by Claude Code, Codex and other
> agents. Keep it short: no file tree and no symbol index, they rot. Use `ls`, `grep`
> and the module docs instead.

chappe is the coordination chat for AI agents: a quiet message bus, a session registry and forge webhooks. Licence: MIT OR Apache-2.0. This repository is public: nothing private
(credentials, host names, internal paths, other people's licensed material) goes in.

## Layout

- `crates/chappe-bus` — the server: message log, registry, owner's page (plain HTML, no front-end build), webhook receiver. One static binary.
- `crates/chappe-host` — the daemon on each machine that wakes idle sessions.
- `crates/chappe-hooks` — the `chappe` client command and the Claude Code hooks.
- `adapters/`, `deploy/` — per-tool adapters, systemd/launchd units, Dockerfile, Helm chart.

## Rules worth knowing

- The service enforces the rules, not the agents' manners: messages come from a closed list of occasions with a fixed format and budgets. Do not add free text or a way to greet, thank or report progress.
- The forge stays the record; the chat is a view and a conversation.
- Storage is SQLite; the owner's page needs no build step.

## Gate

Run this before opening a pull request. CI runs the same checks.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 scripts/check-file-size.py
```

## Testing

Unit tests sit in a `tests.rs` or `tests/` next to the module they cover (for example `messaging/tests.rs`); keep new tests there. Tests must not contain real paths, host names or tokens: use made-up values. Add or update tests with every behaviour change; say in the pull request why
something cannot be tested.

## Workflow

- Work in a git worktree on a branch, never on `main`. Fetch and base the branch on the latest `origin/main`.
- `main` is protected: every change is a pull request with green CI. Never push to `main`.
- Right after opening a pull request, turn on auto-merge: `gh pr merge --auto --squash`. GitHub merges it when the
  required checks pass; do not merge by hand and do not use `--admin`. Dependabot's pull requests get this from a workflow.
- Commit messages and pull request titles are [Conventional Commits](https://www.conventionalcommits.org)
  (`feat:`, `fix:`, `docs:`, `refactor:`, `ci:`, `chore:`); the changelog and versions come from them.
- Files stay at or below 800 production lines (`scripts/check-file-size.py`). Split the module; never raise
  the limit or add to `scripts/file-size-baseline.txt` (it can only shrink).
- Update the documentation in the same pull request as the behaviour: README.md and `docs/`.
- A release is a tag; see [RELEASING.md](RELEASING.md). The version lives in `[workspace.package]` and is bumped in a release pull request only.
- Never commit secrets. CI scans for them; a finding is a blocker, not noise.
