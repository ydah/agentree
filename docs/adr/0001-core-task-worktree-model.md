# ADR 0001: One task owns one worktree

Agentree allocates one local branch and one linked worktree per task. The
worktree's per-worktree Git index is the task's mutation boundary. A task can
have at most one active supervised session.
