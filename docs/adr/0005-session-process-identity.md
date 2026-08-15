# ADR 0005: Session process identity

Session records keep the child PID, process group, and a birth identity. The
environment is only a hint; it is not authorization by itself. A stale or
reused PID is never signaled without a matching birth identity.
