# Agentree

Agentree is a local Rust CLI for running independent coding tasks in isolated
Git linked worktrees. Each task owns one branch, worktree, index, immutable
configuration snapshot, and at most one supervised session.

## Status

The current release implements the core isolation and recovery-oriented
workflow: repository manifests, SQLite state with WAL/FULL durability,
journaled task creation, strict task-local Git execution, sessions, dual-tree
checkpoints, scope/overlap reporting, checks, fetch, sync, fast-forward land,
archive, branch deletion, and read-only doctor diagnostics.

```text
agentree init
agentree new parser-error --base main --scope 'src/parser/**'
agentree run parser-error -- cargo test
agentree checkpoint create parser-error -m 'before integration'
agentree check parser-error
agentree land parser-error --onto main
agentree remove parser-error
```

Agentree is a cooperative guardrail, not an operating-system sandbox. A
process that bypasses the shim or directly edits Git metadata is outside the
guarantee. Dirty worktrees are archived in place; v0.1 through v0.3 never
delete dirty data and never expose a broad `--force` removal path.

## Build and test

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

The CLI intentionally invokes Git with argument vectors and machine-readable
queries. Internal history operations use deterministic Git profiles, disable
autostash/shared rerere, and update target branches only through a checked-out
worktree running `merge --ff-only`.
