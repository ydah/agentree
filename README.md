# Agentree

[![CI](https://github.com/ydah/agentree/actions/workflows/ci.yml/badge.svg)](https://github.com/ydah/agentree/actions/workflows/ci.yml)
[![actionlint](https://github.com/ydah/agentree/actions/workflows/actionlint.yml/badge.svg)](https://github.com/ydah/agentree/actions/workflows/actionlint.yml)
[![zizmor](https://github.com/ydah/agentree/actions/workflows/zizmor.yml/badge.svg)](https://github.com/ydah/agentree/actions/workflows/zizmor.yml)
[![Crates.io](https://img.shields.io/crates/v/agentree.svg)](https://crates.io/crates/agentree)
[![docs.rs](https://docs.rs/agentree/badge.svg)](https://docs.rs/agentree)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A safety-oriented Rust CLI for running independent coding tasks in isolated Git
worktrees.

[Features](#key-features) · [Quick start](#quick-start) ·
[Commands](#commands) · [Configuration](#configure) ·
[Security boundary](#security-boundary) · [Development](#development)

Agentree gives every task its own branch, worktree, Git index, and validation
contract. Run work in isolation, inspect exactly what changed, synchronize
explicitly, and land only through a fast-forward operation you can explain.

```text
create task → run and check → sync explicitly → fast-forward land → clean up
```

## Key Features

### Isolated task worktrees

Create parallel tasks without sharing a working directory or Git index. Each
task starts from an exact base commit and owns its generated branch and
worktree. Optional scopes make likely path overlaps visible before work begins.

### Guarded task execution

`agentree run` supervises a command inside the task worktree and exposes a
task-local Git shim. The shim rejects repository overrides and disallowed
history or network operations, helping a cooperative task stay within its
assigned boundary.

### Checks bound to the exact task HEAD

Configured checks are snapshotted when a task is created. `agentree check`
records the command, configuration, and revision provenance, so readiness is
derived from the task state instead of a mutable working copy.

### Explicit synchronization and landing

Sync and landing are separate operations. Rebase a task onto an exact target
commit, resolve conflicts deliberately, then fast-forward the target branch
through a managed worktree. Agentree never silently rebases during landing.

### Durable recovery

Task creation, checkpoints, sync, landing, removal, and branch deletion are
journaled. SQLite state, safety refs, deterministic Git profiles, and
plan-first `doctor` diagnostics keep interrupted operations inspectable.

### Automation-friendly output

Use `--json` for scripts and coordinators. Human-readable output remains the
default for interactive work, while task facts, checks, diffs, and recovery
plans can all be queried without scraping terminal text.

### Conservative cleanup

Dirty worktrees are preserved. `remove` only removes an exact, residue-free
managed worktree, and branch deletion is a separate explicit operation. There
is no broad `--force` removal path.

## Usage

### Quick Start

Install Agentree, then run it from an existing Git repository:

```bash
cargo install agentree --locked

cd path/to/repository
agentree init
agentree config scaffold

agentree new parser-error --base main --scope 'src/parser/**'
agentree run parser-error -- cargo test
agentree check parser-error
agentree diff parser-error
agentree land parser-error --onto main

agentree remove parser-error
agentree delete-branch parser-error --yes
```

The task can be inspected at any point:

```bash
agentree status
agentree context parser-error
agentree overlap parser-error
```

`land --onto <branch>` uses a temporary managed worktree when the target branch
is not checked out. If the target branch is already checked out, use
`agentree land <task> --into-current` from that exact target worktree instead.

### Commands

Global options can be placed before or after a command:

```text
--json                   Emit machine-readable output where supported
--quiet                  Suppress non-essential output
--repository <path>      Resolve the repository from an explicit path
-h, --help               Show command help
-V, --version            Show the installed version
```

#### Repository and task management

| Command | Purpose |
| --- | --- |
| `init` | Create the repository manifest, SQLite state, lock namespace, and managed worktree root. |
| `config scaffold` | Create a root `.agentree.toml` without overwriting an existing file. |
| `new <slug>` | Create a task from an exact base commit; accepts `--base` and repeated `--scope`. |
| `status` | List managed tasks and their lifecycle state. |
| `context <task>` | Report task facts, checks, content state, and derived readiness. |
| `diff <task>` | Show the task's review diff. |
| `overlap [<task>...]` | Report actual path intersections and planned-scope violations. |

#### Execution and validation

| Command | Purpose |
| --- | --- |
| `run <task> -- <argv...>` | Run one supervised command with the task-local Git environment. |
| `shell <task>` | Open the task through the current shell under supervision. |
| `git <task> -- <git-argv...>` | Run an allowed Git command in the task context. |
| `check <task>` | Run configuration-snapshotted checks and record exact revision provenance. |
| `fetch [--remote <name>]` | Update validated remote-tracking refs with unsafe fetch side effects disabled. |

#### Synchronization and integration

| Command | Purpose |
| --- | --- |
| `sync <task> --onto <branch>` | Rebase a task onto an exact target OID. |
| `sync <task> --continue` | Continue a previously interrupted conflict resolution. |
| `sync <task> --abort` | Abort a previously interrupted rebase and recover the task. |
| `resolve <task> --shell` | Enter the task shell to resolve a sync conflict. |
| `land <task> --onto <branch>` | Fast-forward a target branch through a managed temporary worktree. |
| `land <task> --into-current` | Fast-forward the exact target worktree from its current directory. |

#### Checkpoints and cleanup

| Command | Purpose |
| --- | --- |
| `checkpoint create <task>` | Capture staged and worktree trees as a durable checkpoint. |
| `checkpoint list <task>` | List checkpoints belonging to a task. |
| `checkpoint show <id>` | Inspect checkpoint metadata. |
| `checkpoint restore <id> --to-new-task <slug>` | Restore a checkpoint into a new task without deleting the source. |
| `archive <task>` | Archive task metadata while leaving filesystem data untouched. |
| `remove <task>` | Remove only an exact residue-free worktree; the task branch remains. |
| `delete-branch <task> --yes` | Separately delete an owned task branch after its worktree is gone. |
| `doctor` | Diagnose operations or sessions; `--apply` requires a fresh plan fingerprint. |

See the [full command reference](docs/command-reference.md) for lifecycle
rules, recovery behavior, and option details.

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
agentree --json doctor
```

The JSON envelope includes the command, result or error, and operation details
where applicable. It is designed for programmatic inspection; human-readable
output remains the default for interactive use.

## Install

### From crates.io

```bash
cargo install agentree --locked
```

Agentree requires Git and a Rust toolchain compatible with Rust 1.80 or newer.

### Release binary

Download an archive from [GitHub Releases](https://github.com/ydah/agentree/releases),
verify its `.sha256` file, and place the `agentree` binary somewhere on your
`PATH`. Published release archives currently target Linux x86_64 and macOS
Apple Silicon.

### Build from source

```bash
git clone https://github.com/ydah/agentree.git
cd agentree
cargo install --path . --locked
```

### Development build

```bash
cargo build
cargo run -- --help
cargo test --all-features
```

## Configure

`agentree config scaffold` creates `.agentree.toml` at the repository root
without overwriting an existing file. Checks are read from the exact task base
commit when a task is created, so changing the working copy later does not
silently change an existing task's validation contract.

Commit `.agentree.toml` before creating tasks if those checks should be part of
their validation contract.

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

Checks use argv arrays. Agentree does not perform shell interpolation for
configured commands; use an explicit shell executable when shell behavior is
intentional.

## Recipes

### Rebase a task onto the latest target

Synchronization is explicit and never happens implicitly during landing:

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

### Require checks before a command finishes

For a task that must pass its configured post-checks after a command completes:

```bash
agentree run parser-error --require-post-checks -- cargo test
```

The regular `check` command remains useful when checks should be run separately
or inspected in automation.

### Preserve a dirty task

`remove` refuses dirty or unexpected residue. Use `archive` when the task must
be made inactive without touching its worktree or branch:

```bash
agentree archive parser-error
```

### Create a recovery point

Checkpoints preserve both the staged index tree and the worktree tree. Restoring
always creates a new task, leaving the source task intact:

```bash
agentree checkpoint create parser-error -m 'before integration'
agentree checkpoint list parser-error
agentree checkpoint restore <checkpoint-id> --to-new-task parser-error-recovered
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

## FAQ

### Is Agentree an OS sandbox?

No. Agentree is a cooperative guardrail for processes launched through its
session environment. A same-user process can bypass the shim with an absolute
Git binary or direct filesystem access.

### Does `overlap` detect merge conflicts?

No. It reports path intersections and planned-scope violations. It cannot
reason about semantic conflicts, generated files, or behavior-level coupling.

### Why did `remove` refuse my task?

`remove` is intentionally limited to an exact, residue-free managed worktree.
Use `context <task>` to inspect the state, clean the task if appropriate, or
use `archive <task>` to preserve dirty data without deleting it.

### What happens if the target branch is already checked out?

Use `land <task> --into-current` from that target worktree. The command verifies
the current directory and performs only the allowed fast-forward update.

### What happens if landing fails halfway through?

Agentree records the operation and creates safety refs before the target update.
Run `doctor --operation <id> --plan` and apply only a fresh, matching plan.

## Security Boundary

Agentree is useful for coordinating cooperative coding tasks, but it is not an
operating-system sandbox, mandatory-access-control layer, or malware boundary.

An in-scope process can still invoke an absolute Git binary, edit `.git`
directly, modify files outside the worktree, or install a malicious
filter/helper. Path overlap is heuristic and does not predict semantic merge
conflicts. Non-UTF-8 paths, submodule-internal dirty state, sparse or split
indexes, intent-to-add state, power loss, and filesystem destruction are
outside the initial support guarantee.

Read [Security boundary and guarantees](docs/security.md) before using
Agentree as part of an automation or review policy. See [SECURITY.md](SECURITY.md)
for vulnerability reporting.

## Limitations

The initial release intentionally keeps its support boundary narrow:

- Git is required, and POSIX-style development environments are the primary
  target.
- Agentree coordinates processes launched through its session; it does not
  contain arbitrary child-process filesystem or OS behavior.
- `overlap` is path-based and cannot predict semantic or generated-file
  conflicts.
- Recovery guarantees depend on the filesystem and Git preserving the recorded
  state and refs.
- Unsupported or unusual repository features should be tested in a disposable
  repository before adoption in automation.

## Project Goals

- Keep one task's branch, worktree, index, and validation contract together.
- Make synchronization, landing, cleanup, and branch deletion explicit.
- Prefer observable, operation-scoped recovery over guessing or broad cleanup.
- Support both interactive workflows and scriptable JSON output.
- Keep the safety boundary honest: useful guardrails without pretending to be a
  sandbox.

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
See [CONTRIBUTING.md](CONTRIBUTING.md) for the contribution workflow.

## License

MIT. See [LICENSE](LICENSE).
