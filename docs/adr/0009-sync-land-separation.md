# ADR 0009: Sync and land are separate operations

`sync` rewrites only a task history against an exact target OID. `land` does
not rewrite task history and only performs a fast-forward merge into a target
worktree. There is no combined `land --rebase` operation.
