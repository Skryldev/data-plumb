# Design: Boot sequence

Status: APPROVED (V9 PG-1, contract §2.3).

Fixed order — each step gates the next; no write surface binds before
step 5 completes:

1. **Single-writer lock** — exclusive create of `<data_dir>/run/writer.lock`
   (pid + timestamp inside; V6-18/V8 §7.7: one writer per data dir is a
   boot-enforced contract, not a deployment hope). A live lock ⇒ refuse
   boot naming the pid; a stale lock (>1 h mtime) is stolen with a WARN.
2. **WAL recovery** — per wal.md §5: CRC-validated replay of committed
   txns past the redb watermark; tail truncation counted + WARNed.
3. **redb open + schema gate** — `schema/version` checked per
   storage-format-v2.md §2 (v1 + auto-migrate-off ⇒ refuse pointing at
   `xtask migrate`; newer-than-binary ⇒ refuse naming versions). The
   V8 C-5 config lint runs here too (unknown keys WARN by name;
   contradictory combos — e.g. throttle TTL ordering, quota math that
   cannot hold — refuse).
4. **Boot sweep** — `.tmp-*`, spool, and `.gc-pending` cleanup. The
   `.gc-pending` handler carries the **V8 C-4 resurrection re-check**:
   before deleting a claimed blob, `chunk_is_referenced` is consulted —
   a LIVE row restores the pending blob to canonical (the crash window
   V8-REG-02 identified is closed by construction); stale rows delete.
5. **Scrub schedule + listeners** — scrubber timer arms (PG-2); only now
   do REST/gRPC bind.

Shutdown is the mirror: stop accepting → drain in-flight uploads (leases
released) → final WAL group-commit flush → CHECKPOINT → close redb →
exit 0 within `shutdown.grace_period` (default 30 s). On timeout: abort
— the state is by construction recoverable (WAL replay), which IS the
contract; never corrupt-on-timeout. Gates:
`sigterm_drains_and_exits_clean`, `sigterm_timeout_leaves_recoverable_state`.

The c01 boot-gate story extends: a v2 data dir opened by an old binary
refuses at step 3, before step 5 binds anything.
