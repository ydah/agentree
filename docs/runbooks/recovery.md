# Recovery runbook

Agentree treats Git and SQLite as separate systems. If a process stops between
steps, do not reset, remove, or manually delete the expected object first.

1. Run `agentree doctor --json` from the repository.
2. Select one incomplete operation and inspect its expected and observed facts.
3. Re-run `agentree doctor --operation <id> --plan --json` and retain the plan fingerprint.
4. If the plan says a known Git mutation succeeded, apply only that operation with `--apply --plan-fingerprint <fingerprint>`.
5. If a branch, worktree, marker, target OID, or path differs from the expected facts, stop and preserve the files for manual intervention.

Doctor never rolls back a successful target fast-forward and never removes an
unknown worktree, branch, ref, or dirty path. A dirty task can be made
inactive with `agentree archive <task>`; this changes metadata only.

For a conflicted sync, use `agentree resolve <task> --shell`, then either
`agentree sync <task> --continue` or `agentree sync <task> --abort`. Land is a
separate command and never runs sync implicitly.
