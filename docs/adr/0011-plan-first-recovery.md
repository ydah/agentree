# ADR 0011: Recovery is operation-scoped and plan-first

Doctor is read-only by default. Any future apply operation must identify one
operation, reacquire locks, re-observe Git facts, and compare a fresh plan
fingerprint. Unknown ownership, unexpected refs, and successful target
advances are never automatically deleted or rolled back.
