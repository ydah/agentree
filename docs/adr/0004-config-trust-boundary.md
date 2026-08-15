# ADR 0004: Configuration is snapshotted from the task base

Task configuration is read from `.agentree.toml` at the exact base commit,
normalized, hashed, and stored before task activation. Working-tree config is
not execution policy. Refresh is an explicit future operation and invalidates
revision-bound checks.
