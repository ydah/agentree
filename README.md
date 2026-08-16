# Agentree

A safety-oriented Rust CLI for running independent coding tasks in isolated Git
worktrees.

[Key Features](#key-features) | [Usage](#usage) | [Install](#install) | [Configure](#configure) | [FAQ](#faq)

`agentree` keeps independent coding tasks isolated without making them share a
working directory or Git index. Create a task, run it under a task-local Git
policy, inspect its state, and land it through explicit, fast-forward-only
operations.

* * *

## Key Features

### Isolated task worktrees

Create parallel tasks without sharing a working directory or index. Each task
starts from an exact base commit and owns its generated branch and worktree.
Planned scopes can be recorded to make likely overlaps visible before work
begins.

### Guarded task execution

`agentree run` supervises a command inside the task worktree and exposes a
task-local Git shim. The shim rejects repository overrides and disallowed
history or network operations, helping tasks stay within their assigned
boundary.

### Durable operations and recovery

Task creation, checkpoints, sync, landing, removal, and branch deletion are
journaled. SQLite state, safety refs, deterministic Git profiles, and
read-only, plan-first `doctor` diagnostics make interrupted operations
inspectable instead of silently ambiguous.

### Review-aware workflow

Use `status`, `context`, `diff`, `check`, `overlap`, and `checkpoint` to keep
task state explicit. Required checks are bound to the exact task HEAD and
configuration snapshot before a task can be landed.

### Conservative cleanup

Dirty worktrees are archived in place. `remove` only removes a residue-free
managed worktree, and branch deletion is a separate explicit operation. There
is no broad `--force` removal path.

## Usage

### Quick Start

Run these commands from an existing Git repository:

```bash
agentree init

agentree new parser-error --base main --scope 'src/parser/**'
agentree status
agentree context parser-error

agentree run parser-error -- cargo test
agentree check parser-error
agentree diff parser-error
agentree checkpoint create parser-error -m 'before integration'

agentree land parser-error --onto main
agentree remove parser-error
agentree delete-branch parser-error --yes
```

`land --onto <branch>` uses a temporary managed worktree when the target branch
is not checked out. If the target branch is already checked out, run
`agentree land <task> --into-current` from that exact target worktree instead.

### Commands

Global options can be used before or after a command:

```text
--json                   Emit machine-readable output where supported
--quiet                  Suppress non-essential output
--repository <path>      Resolve the repository from an explicit path
-h, --help               Show command help
-V, --version            Show the installed version
```

| Command | Purpose |
| --- | --- |
| `init` | Create the repository manifest, SQLite state, lock namespace, and managed worktree root. |
| `config scaffold` | Create a root `.agentree.toml` without overwriting an existing file. |
| `new <slug>` | Create a task from an exact base commit; accepts `--base` and repeated `--scope`. |
| `status` | List managed tasks and their current lifecycle state. |
| `context <task>` | Report task facts, checks, content state, and derived readiness. |
| `diff <task>` | Show the task's review diff. |
| `run <task> -- <argv...>` | Run one supervised command with the task-local Git environment. |
| `shell <task>` | Open the task through the current shell under supervision. |
| `git <task> -- <git-argv...>` | Run an allowed Git command in the task context. |
| `checkpoint create/list/show/restore` | Capture or restore staged and worktree trees; restore always creates a new task. |
| `overlap [<task>...]` | Report actual path intersections and planned-scope violations. This is not semantic conflict detection. |
| `check <task>` | Run configuration-snapshotted checks and record exact revision provenance. |
| `fetch [--remote <name>]` | Update validated remote-tracking refs with unsafe fetch side effects disabled. |
| `sync <task> --onto <branch>` | Rebase a task onto an exact target OID; use `--continue` or `--abort` for recovery. |
| `land <task> --onto <branch>` | Fast-forward a target branch through a managed temporary worktree. |
| `land <task> --into-current` | Fast-forward the exact target worktree from its current directory. |
| `archive <task>` | Archive task metadata while leaving filesystem data untouched. |
| `remove <task>` | Remove only an exact residue-free worktree; the task branch remains. |
| `delete-branch <task> --yes` | Separately delete an owned task branch after its worktree is gone. |
| `doctor` | Diagnose operations or sessions; `--apply` requires a fresh plan fingerprint. |

See the [full command reference](docs/command-reference.md) for lifecycle
rules and recovery behavior.

### Task Lifecycle

| State | Meaning | Typical next action |
| --- | --- | --- |
| `active` | The task worktree is available for development. | `run`, `check`, `checkpoint`, `sync`, or `land` |
| `conflicted` | A sync/rebase needs an explicit resolution. | `resolve`, then `sync --continue` or `sync --abort` |
| `landed` | The task commit was fast-forwarded into its target branch. | `remove`, then `delete-branch --yes` |
| `archived` | Task metadata is inactive; filesystem data is retained. | Inspect or clean manually, then remove when residue-free |
| `broken` | Recovery or manual intervention is required. | `doctor` and the [recovery runbook](docs/runbooks/recovery.md) |

### Output for Automation

Use `--json` when integrating Agentree with scripts or another coordinator:

```bash
agentree --json status
agentree --json context parser-error
agentree --json check parser-error
```

The JSON envelope includes the command, result or error, and operation details
where applicable. Human-readable output remains the default for interactive
use.

* * *

## Install

### Build from Source

Requirements: Git and a Rust toolchain compatible with Rust 1.80 or newer.

```bash
git clone https://github.com/ydah/agentree.git
cd agentree
cargo install --path .
```

### Install a Release Binary

Download a platform archive from the [GitHub Releases](https://github.com/ydah/agentree/releases)
page, verify its checksum, and place the `agentree` binary somewhere on your
`PATH`.

### Development Build

```bash
cargo build
cargo run -- --help
cargo test --all-features
```

* * *

## Configure

`agentree config scaffold` creates `.agentree.toml` at the repository root.
Checks are read from the exact task base commit when a task is created, so
changing the working copy later does not silently change an existing task's
validation contract. Commit `.agentree.toml` before creating tasks if those
checks should be part of their validation contract.

```toml
# .agentree.toml
[[checks]]
name = "format"
command = ["cargo", "fmt", "--all", "--", "--check"]
required = true

[[checks]]
name = "tests"
command = ["cargo", "test", "--all-features"]
required = true
timeout_seconds = 1800
output_limit_bytes = 8388608
```

Commands are argv arrays. Agentree does not perform shell interpolation for
configured checks; use an explicit shell executable when shell behavior is
intentional.

## Recipes

### Rebase a task onto the latest target

Sync is explicit and never happens implicitly during landing:

```bash
agentree fetch --remote origin
agentree sync parser-error --onto main
```

If Git reports conflicts, resolve them through the supervised task shell and
choose exactly one continuation:

```bash
agentree resolve parser-error --shell
agentree sync parser-error --continue
# or: agentree sync parser-error --abort
```

### Preserve a dirty task

`remove` refuses dirty or unexpected residue. Use `archive` when the task must
be made inactive without touching its worktree or branch:

```bash
agentree archive parser-error
```

### Diagnose an interrupted operation

Keep the operation evidence intact and let `doctor` produce a plan before
applying any known repair:

```bash
agentree doctor --json
agentree doctor --operation <operation-id> --plan --json
agentree doctor --operation <operation-id> --apply \
  --plan-fingerprint <fingerprint> --json
```

Never manually delete an unknown worktree, branch, ref, or dirty path while
recovering an operation. See the [recovery runbook](docs/runbooks/recovery.md)
for the complete procedure.

## Security boundary

Agentree is a cooperative guardrail for processes launched through its session
environment. It is not an operating-system sandbox, mandatory-access-control
layer, or malware boundary.

An in-scope process can still invoke an absolute Git binary, edit `.git`
directly, modify files outside the worktree, or install a malicious
filter/helper. Path overlap is heuristic and does not predict semantic merge
conflicts. Non-UTF-8 paths, submodule-internal dirty state, sparse or split
indexes, intent-to-add state, power loss, and filesystem destruction are
outside the initial support guarantee.

Read [Security boundary and guarantees](docs/security.md) before using
Agentree as part of an automation or review policy.

## FAQ

### Is Agentree an OS sandbox?

No. It is a cooperative guardrail for processes launched through its session
environment. A same-user process can bypass the shim with an absolute Git
binary or direct filesystem access.

### Does `overlap` detect merge conflicts?

No. It reports path intersections and planned-scope violations. It cannot
reason about semantic conflicts, generated files, or behavior-level coupling.

### Why did `remove` refuse my task?

`remove` is intentionally limited to an exact, residue-free managed worktree.
Use `context <task>` to inspect the state, clean the task if appropriate, or
use `archive <task>` to preserve dirty data without deleting it.

### What happens if landing fails halfway through?

Agentree records the operation and creates safety refs before the target update.
Run `doctor --operation <id> --plan` and apply only a fresh, matching plan.

## Project goals

- Keep one task's branch, worktree, index, and validation contract together.
- Make synchronization, landing, cleanup, and branch deletion explicit.
- Prefer observable, operation-scoped recovery over guessing or broad cleanup.
- Support both interactive workflows and scriptable JSON output.
- Keep the safety boundary honest: useful guardrails without pretending to be a sandbox.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --doc
```

CI runs the Rust quality checks on Ubuntu and macOS. GitHub Actions workflow
syntax and security are checked separately with actionlint and zizmor.

Architecture decisions are documented in [docs/adr](docs/adr), and interrupted
operation procedures are collected in the [recovery runbook](docs/runbooks/recovery.md).

## License

MIT
