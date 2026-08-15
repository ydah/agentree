# ADR 0006: Strict Git shim

The shim parses argv without a shell and applies a fail-closed command table.
Read-only commands and task-local staging are allowed. Repository overrides,
history rewrites, network operations, maintenance, and unknown options are
denied. Internal Agentree operations use typed Git runners directly and do
not rely on a bypass token in the child environment.
