# ADR 0012: Durability claims are bounded

SQLite uses WAL with `synchronous=FULL`; manifest, marker, and snapshot files
use temp-write, file sync, atomic rename, and parent-directory sync. These
claims cover process crashes on a local filesystem, not power loss or
filesystem/hardware destruction.
