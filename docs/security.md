# Security boundary and guarantees

Agentree is a cooperative safety tool for processes launched through its
session environment. It is not an OS sandbox, mandatory access-control layer,
or malware boundary.

It provides:

- one branch, worktree, and index per managed task;
- deterministic internal Git profiles with hooks, autostash, shared rerere,
  automatic maintenance, and unsafe fetch side effects disabled;
- a strict shim that rejects repository overrides, history/network commands,
  and unknown Git options;
- process-group supervision with stored process identity data;
- operation journaling and read-only, plan-first recovery;
- fail-closed checks for hidden index flags, ignored/non-ignored residue,
  empty directories, special nodes, and in-progress Git state.

It does not prevent a same-user process from invoking an absolute Git binary,
editing `.git` directly, changing files outside the worktree, or installing a
malicious filter/helper. Non-UTF-8 paths, submodule-internal dirty state,
sparse/split/intent-to-add index state, power loss, and filesystem destruction
are outside the initial support guarantee.
