# ADR 0002: Git facts and a durable operation journal

Git remains the source of truth for current refs, HEAD, worktree and index
facts. SQLite stores intent, ownership, history, expected values, and
operation phases. Git processes are never run while a SQLite write transaction
is held. Recovery observes successful Git mutations and never guesses a
rollback that could erase an external change.
