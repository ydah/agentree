# ADR 0010: Target branches advance through checked-out worktrees

Agentree never updates a land target with `update-ref`. `--into-current`
requires the caller's exact pristine target worktree. `--onto` creates an
owned temporary worktree for an unclaimed target and runs `merge --ff-only`
there.
