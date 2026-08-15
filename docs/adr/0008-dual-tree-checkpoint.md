# ADR 0008: Checkpoints preserve the stage boundary

A checkpoint records an index tree and an independently captured worktree
tree. Both are structurally referenced by a metadata tree and commit, keeping
the objects reachable after the task branch is deleted. The real index is
copied and verified before and after capture; a single tree or porcelain
status fingerprint is not sufficient.
