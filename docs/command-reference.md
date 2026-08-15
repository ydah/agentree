# Command reference

Global options are accepted before or after the command: `--json`, `--quiet`,
and `--repository <path>`.

| Command | Behavior |
| --- | --- |
| `init` | Creates the common-Git-dir manifest, SQLite state, lock namespace, and managed worktree root. |
| `config scaffold` | Writes a new root `.agentree.toml`; refuses to overwrite an existing file. |
| `new <slug> [--base <ref>] [--scope <glob>]...` | Creates one owned branch and linked worktree from an exact base OID. |
| `run <task> -- <argv...>` | Runs one supervised task session with a task-local Git shim. |
| `git <task> -- <git-argv...>` | Runs the strict task-local Git policy outside a session. |
| `status`, `context`, `diff` | Read facts and derived readiness without changing task state. |
| `checkpoint create/list/show/restore` | Captures or restores staged and worktree trees. Restore always creates a new task. |
| `overlap [<task>...]` | Reports actual path intersections and planned-scope violations. It is not semantic conflict detection. |
| `check <task>` | Runs snapshotted argv checks and records exact revision provenance. |
| `fetch --remote <name>` | Updates only validated remote-tracking refs with pruning, tags, recursion, and FETCH_HEAD writes disabled. |
| `sync <task> --onto <branch>` | Explicitly rebases the task against an exact target OID. `--continue` and `--abort` are separate recovery actions. |
| `land <task> --into-current` | Fast-forwards the exact target worktree from its current directory. |
| `land <task> --onto <branch>` | Uses an owned temporary worktree, only when the target is not checked out elsewhere. |
| `archive <task>` | Archives metadata while leaving dirty worktree data untouched. |
| `remove <task>` | Removes only an exact `RemovalSafe` worktree; the branch remains. No `--force` path exists. |
| `delete-branch <task> --yes` | Separately deletes an exact owned task branch after worktree removal. |
| `doctor` | Read-only operation diagnosis. `--apply` requires an operation id and fresh plan fingerprint. |

Administrative commands are denied from inside a supervised task session.
