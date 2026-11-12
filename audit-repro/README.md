# audit-repro — frozen evidence from the 2026-11-11 production audit

**Status:** frozen historical evidence for commit `e4cd5a6`. These artifacts
assert the BUGGY behavior and are kept verbatim per the remediation contract
(evidence is never deleted). The fixed behavior is asserted by the inverted
regression tests under `tests/regression/` and the expect-fail smoke
`poc_expect_fail.sh` in the repo root.

## Contents

| Artifact | What it proves | Finding |
|---|---|---|
| `audit_repro.rs` | executable component-level proofs driving the real `IndexDb`/`BlockStore`/`GarbageCollector`/`Compressor`/`AuthService` types | C-01, C-02, C-03, C-03b, H-04, M-01, M-16 |
| `curl-poc.sh` | black-box PoCs against a running stack | C-01, H-02, H-03, H-05, M-04, M-16 |
| `INTERLEAVINGS.md` | scheduler-order traces | C-02, C-03, H-01, M-07, M-11, M-14 (+ sync-mode durability verification) |

## Running (on the UNFIXED tree)

```bash
# component-level proofs (all #[ignore]d — they demonstrate defects):
cargo test --test audit_repro -- --include-ignored --nocapture

# black-box PoCs against a live default-config stack:
cargo run --release -- --config config/default.yaml &
bash audit-repro/curl-poc.sh
```

## Running (after the remediation)

Every scenario here fails by design on the fixed tree. CI runs
`bash poc_expect_fail.sh` against a compose stack built from current main and
requires every section to report the attack failing. The scenario bodies in
`audit_repro.rs` are preserved; where a production signature changed during the
remediation (e.g. `Compressor::decompress` now takes a `u64` bound), the call
site in these frozen files was adapted to keep the evidence compilable — the
scenario semantics are unchanged and noted inline.
