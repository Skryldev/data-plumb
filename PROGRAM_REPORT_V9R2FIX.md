# PROGRAM_REPORT_V9R2FIX — CI Red Fix-Forward

## 0. Summary
3 of 3 clusters **reproduced** on the current head and **fixed** (CF-1/CF-2/CF-3, merged). Exact CI suite green locally (Windows; Linux definitive via operator push). Plus: the history was rewritten since `4f9f08b` (PG-1R lineage collapsed; commit gone) — PG-1R code survived in the tree but three CI fixes were lost and are re-landed here. 273/0 re-derived.

## 1. Per-cluster
| Cluster | CI command | State | Fix | Verification |
|---|---|---|---|---|
| A: clippy E0433 `BlockStore` @ block_store.rs:452 | `cargo clippy --all-targets -- -D warnings` | **reproduced** via cfg-relaxation probe (`all(test,unix)`→`test`; Windows structurally blind — module compiled out locally) | CF-1: `use super::*;` in `fsync_discriminator_tests` | clippy exit 0; `cargo test --no-run` exit 0; unix gate restored |
| B: buf lint naming conventions | `buf lint --error-format github-actions proto` | **reproduced** (buf 1.75.0 — exact CI version): PG-1R F2 added 3 of 4 needed rules and **missed `RPC_REQUEST_STANDARD_NAME`**; the rewrite dropped the rest of the except list | CF-2: all 4 rules excepted (D9 rationale in buf.yaml) | `buf lint proto` exit 0; gh-format exit 0 |
| C: script exec bit | `git ls-files -s scripts/` | **reproduced** (all `100644` on current head — lost in the rewrite) | CF-3: chmod re-applied to all 3 scripts | all `100755` |

## 2. Pre-flight correction
The exact CI invocations are now the mandatory pre-flight: `cargo clippy --all-targets -- -D warnings` (the `--all-targets` + `-D warnings` suffix was missing from prior pre-flights) plus `cargo test --no-run --all-targets` as the test-compilation discriminator. Platform caveat recorded: unix-gated test modules are invisible to the Windows compiler; the cfg-relaxation probe is the local reproduction technique, and Linux CI is the definitive execution.

## 3. Baseline re-derivation
273 passed / 0 failed / 5 ignored across 14 binaries (unit 174(+1 ign=0) · main 0 · gRPC 19 · integration 16(+1i) · property 6 · c01-boot 1 · hardening 23(+1i) · m01 6 · m04 5 · m05 10 · m07 5 · race_gc 5 · replica_purge 3 · soak 0(+2i)) — confirms the 273/0 reference on the fixed head.

## 4. Provenance
CF-1: `fix/pg1r2fix/clippy-alltargets-import` → merge `9138c46` · CF-2: `fix/pg1r2fix/buf-naming-conventions` → merge `c127758` · CF-3: `fix/pg1r2fix/script-exec-bit` → merge `2cef44e`.

## 5. Findings recorded (rule 3)
1. **History rewritten since `4f9f08b`** (PG-1R lineage collapsed — the V6-17 pattern): PG-1R code survived in the tree; three CI fixes (F2 except-list, F3 exec bits) and the F4 ci.yml overrides were **lost** and are re-landed (F4/F5 verified still present — the earlier grep that reported them lost was itself wrong: a literal alternation without `grep -E`).
2. **ci-errors.txt diagnosis corrected**: Cluster A is a real Linux compile error (not mapped in PG-1R); Cluster B's errors are naming conventions, not placeholders — the placeholder half was already fixed, the naming half needed the except.
3. **Windows-verification blindness**: unix-cfg'd test modules are invisible to the Windows compiler; local "273/0" was Windows-true, Linux-false until CF-1. The cfg-relaxation probe is now the standing technique for unix-gated code.

## 6. Operator actions
1. Push `main` (head `2cef44e` + this report) and paste the **fresh** CI run URLs — green converts V9-R2 to Mode A and unblocks the 6-item run.
2. buf breaking cannot be validated locally (`--against` repo not reachable without CI auth) — it runs in CI with the checkout token.
3. Sign off errata §6 in the next intake to enable I-3c (A-12 still pending).
