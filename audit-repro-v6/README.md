# audit-repro-v6 — V6 re-audit reproductions

Reproductions for V6 findings (`new` / `regression` / `missed-by-v3`) in
`AUDIT_REPORT_V6.md`. **Environment limitation (same as V3):** the audit
host is Windows without WSL/Docker; although M-13 fixed the *compile*
barrier, no Rust toolchain execution of the full suite was possible in the
sandbox, so every artifact here is **runnable but not executed during the
audit**; findings carry exact file:line traces and are graded
`High (trace)` rather than `Confirmed (executed)`.

## How to run (Linux host, repo root)

```bash
cp audit-repro-v6/audit_repro_v6.rs tests/audit_repro_v6.rs
cargo test --test audit_repro_v6 -- --include-ignored --nocapture
rm tests/audit_repro_v6.rs
```

## Files

| File | Findings | What it proves |
|---|---|---|
| `audit_repro_v6.rs` | V6-01, V6-02, V6-03, V6-04, V6-07, V6-08, V6-09 | Gate permit panic race & double-charge arithmetic; boot sweep misses shard-dir `.tmp-`/`.gc-pending`; replication lag counter never decrements; C-03 sub-txn claim window; reconcile full materialization; key-missing misclassified as hostile |
| `INTERLEAVINGS-v6.md` | V6-01, V6-02, V6-05, V6-07, V6-15 | Scheduler/pipe interleavings that cannot be forced deterministically in-process |
| `ci-checks.sh` | V6-05, V6-06, V6-13 | Static verification of the CI workflow defects (pipe exit-code semantics, grep-pattern mismatch, dead nightly trigger, grep-rule holes) — runnable anywhere with bash + grep, no Rust needed |

## Conventions

Tests drive the real production types (`UploadGate`, `IndexDb`,
`BlockStore`, `GarbageCollector`, `Quarantine`, `RetrievalEngine` helpers)
exactly as the pipeline/GC/state assemble them; no product code is
modified; races are enforced as legal orderings, never sleep-and-hope.
