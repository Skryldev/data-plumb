# V6 interleavings & CI traces

Scheduler- and pipe-order traces for findings that cannot be forced
deterministically in-process. Every step cites code.

---

## [V6-01] UploadGate reservation panic — gate.rs:118-134

```
semaphore capacity = 1 (max_concurrent_uploads), two upload tasks A, B:

A: acquire_permit()  → try_acquire_owned() Ok
A: .map(|_| ())      → OwnedSemaphorePermit DROPPED → permit RELEASED   (gate.rs:121)
── preempted ───────────────────────────────────────────────────────────
B: acquire_permit()  → Ok (the just-released permit)                    (gate.rs:118)
B: take_permit()     → Ok, B now HOLDS the permit                       (gate.rs:128)
── resume A ────────────────────────────────────────────────────────────
A: take_permit()     → try_acquire_owned() Err
A: .expect("permit reserved by acquire_permit")  → PANIC                (gate.rs:133)
```

REST: the H-04 CatchPanicLayer converts it to a spurious 500 for A; B
proceeds. gRPC: recovery layer → INTERNAL. Net: near gate capacity
(max_concurrent_uploads), concurrent uploads sporadically 500/INTERNAL.
Window = a few instructions, no await between the calls — pure thread
preemption; the multi-thread hammer in `audit_repro_v6.rs::v6_01_*`
catches it with nonzero probability per run.
Fix: make `acquire_permit` RETURN the permit and move it into the lease
(one acquire, no release/re-acquire), deleting `take_permit`.

---

## [V6-02] known-size gRPC upload double-charge — file_service.rs:141-183

```
budget = max_in_flight_bytes (default 2 GiB); client declares expected_size S
with S > budget/2 (legal: S ≤ max_file_size = 10 GiB in default.yaml):

1. lease = try_reserve_known(S)      → bytes_in_flight = S        (gate.rs:88-98)
2. bridge: for EVERY Data message of the SAME upload:
      lease.charge(msg.len())        → bytes_in_flight += n       (file_service.rs:171)
3. when S + streamed > budget → charge returns BudgetExhausted
   → StorageError::TooLarge mid-stream, upload ABORTS             (file_service.rs:172-182)
```

Every declared gRPC upload larger than half the byte budget fails
mid-stream despite being admissible; smaller ones consume double their
size (starving other uploads). The gate unit tests pass because their
fixtures use sizes where 2·S ≤ budget. The REST raw path never calls
`charge` at all (under-enforcement for chunked bodies) — the two
transports implement two different wrong models.
Fix: `charge` only for unknown-size leases; known leases reserve once.

---

## [V6-05] c01-boot-gate CI job can never pass — .github/workflows/ci.yml:58-71

Two independent defects:

```
(a) pipeline exit code:
      ./target/release/file-storage --config config/production.yaml 2>&1 | tee /tmp/boot.txt
      code=$?                       # $? = tee's status = 0 (no pipefail set)
      test "$code" -ne 0 || exit 1  # ALWAYS exits 1 → job RED even on a
                                    # correct refusal

(b) grep pattern:
      grep -qiE "violation" /tmp/boot.txt
    the binary's boot error is `invalid configuration` + per-field
    messages (lib.rs:88 context; config.rs:559-660 strings) — the word
    "violation" appears NOWHERE in runtime output (only in config.rs
    comments and test names) → the grep ALWAYS fails → job RED
```

Consequence: the "CI-W2: c01-boot-gate verified LIVE" claim cannot be
true for this workflow as written — the job is red on every run
regardless of binary behavior. (The Rust-level gate
`tests/regression_c01_boot.rs` greps the right strings and is sound; the
workflow job is the broken half.)
Fix: `set -o pipefail` + capture before the pipe (or check the binary's
status directly), and grep for `invalid configuration` / `jwt_secret`.

---

## [V6-06] nightly job unreachable — .github/workflows/ci.yml:131-140

`nightly:` is gated on `if: github.event_name == 'schedule'` but the
workflow's `on:` block defines **no `schedule:` trigger** — only
`push` and `pull_request`. The event can never fire, so the nightly job
(including the ONLY place the H-01 RSS bound test
`h01_upload_memory_bound -- --include-ignored` runs) is dead code. The
memory-bound claim of H-01 is therefore enforced nowhere.

---

## [V6-07] C-03 residual window — index_db.rs:239-291 vs gc.rs:238-273

```
upload txn U (write, Durability::Immediate)      GC sweep (victim h)
─────────────────────────────────────────────    ──────────────────────────
begin_write
blob_exists(canonical) = TRUE   (t0)
  … insert row, not yet committed …              finalize_sweep: row absent (no-op)
                                                 claim_for_gc: blob → .gc-pending (t1)
  … commit path (fsync) …                        chunk_is_referenced → FALSE
                                                   (MVCC read: U uncommitted) (t2)
                                                 delete_claimed: pending unlinked (t3)
commit (t4) → row refcount=1
─────────────────────────────────────────────
RESULT: live row, blob gone — dangling reference.
```

Window = U's own transaction body (one stat + insert + fsync commit,
sub-millisecond to low milliseconds). The `race_gc_upload_x100` gate
cannot reliably hit a window that narrow (100 iterations of a µs window),
so green ×5 ≠ impossibility. Downstream defense: reconcile flags the row
as a phantom (`phantom_manifests`), but Repair never re-pulls on a
primary — reads fail until manual action.
Fix (cheap): after a successful `register_chunk_verified`, the pipeline
still holds the bytes — re-assert `blocks.exists(hash)` (or the cheap
`path_exists`) post-commit and rewrite once if missing; that closes t1–t4
entirely because the rewrite happens after GC's delete.

---

## [V6-15] login throttle residuals — throttle.rs, auth.rs:133-166, interceptors

* **Eviction attack (map_capacity 10 000, insertion-order):** an
  authenticated-or-anonymous attacker submitting logins for 10 001
  usernames evicts the oldest throttle entries. A victim's accumulated
  backoff disappears; the attacker's NEXT failure against that username
  restarts at base (1 s) — the exponential ladder must be rebuilt from
  scratch, so sustained brute force achieves ~2–3 guesses per eviction
  cycle instead of one per 15 min. Mitigated in part by the per-username
  ladder re-forming instantly; there is **no per-IP layer on gRPC login**
  (the AuthService has no interceptor; the shared governor is per-tenant
  and only wired into File/Admin/Upload/Download). REST keeps the
  per-IP tower_governor (default 500 rps — weak).
* **Arbitrary-username lockout DoS:** any failed login records a failure,
  including for usernames the attacker does not own — ~11 bad passwords
  lock a known username for the 15-min cap. Classic per-username-lockout
  tradeoff; accepted by rule 17, not documented in
  `docs/src/security/throttling.md` as a residual (checked).
* **No throttle metrics:** lockouts are visible only via
  `auth.login.throttled` audit rows; there is no
  `login_throttle_*` counter family to alert on.
