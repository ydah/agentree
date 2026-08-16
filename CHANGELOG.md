# Changelog

All notable changes to Agentree are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-08-16

Initial public release.

### Added

- Isolated Git tasks with one owned branch, linked worktree, and index per task.
- Task-local Git policy and supervised command sessions.
- Repository manifests, SQLite state, operation journaling, and plan-first recovery.
- Staged/worktree checkpoints with restore-to-new-task behavior.
- Scope and path-overlap reporting, snapshotted checks, fetch, sync, and land.
- Conservative archive, residue-free worktree removal, and explicit branch deletion.
- Human-readable and JSON output modes.

### Known limitations

- Agentree is a cooperative guardrail, not an operating-system sandbox.
- Path overlap is heuristic and does not detect semantic merge conflicts.
- Several advanced Git/index, submodule, filesystem, and power-loss scenarios
  remain outside the initial support guarantee.
