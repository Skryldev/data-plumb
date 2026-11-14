# audit-repro-v8 — V8 reproductions

V8 findings needing execution evidence. All three probes were EXECUTED
during the audit on the Windows host (per-binary evidence in
AUDIT_REPORT_V8.md §4/§5); the scripts here reproduce them.

## fsync-teeth probe (V8-REG-01)

```bash
# 1. prove no test references the durability sequence:
rg -n 'sync_all|fsync' tests/          # expect: zero hits
# 2. scratch branch, remove both cfg(unix) fsync blocks in
#    src/storage/block_store.rs (fsync_dir + fsync_dir_blocking),
#    then: cargo test --locked --all-targets
# EXPECTED (probe result): 265 passed / 0 failed — the invariant is
# undiscriminated. Restore the fsyncs. On Linux CI the same probe must
# be paired with the new failpoint test (condition C-3).
```

## gate-teeth probes (§4.6, both discriminators proven)

```bash
# double-charge reintroduction: edit charge() to ignore self.declared,
# run: cargo test --locked --lib core::gate::tests::h01_known_size_not_double_charged
# EXPECTED: FAILED ("declared lease must not re-charge")
# boot-sweep counter zeroing: force `removed = 0` after the spawn_blocking join,
# run: cargo test --locked --lib state::boot_sweep_tests
# EXPECTED: FAILED ("this boot swept 3 stale files; metric shows 0")
```

## boot-sweep resurrection window (V8-REG-02)

The trace is deterministic (AUDIT_REPORT_V8.md §4): runtime crash between
the resurrection re-check failing and restore_claimed leaves
row-live + canonical-missing + pending-holds-last-copy; the boot sweep's
name+age filter then deletes the pending unconditionally. A executable
test requires a failpoint between gc.rs steps (d)-(e) — scheduled with
C-3's failpoint seam in V9.
