# PROGRAM_REPORT_PG1R2 — Final Completion + Verdict Discipline

## 0. Verdict
**BLOCKED — Mode A precondition unmet (A-14/§2.2).** No item work started (§9: stop before any code change). A-9 formula: **N = 0 of T = 6** (BLOCKED precedes the formula; no item may count without a run). The three cc rows owned by PG-2/3/4 remain out of scope (§0.4).
**PG-1R verdict correction (§0.2, recorded verbatim):** PG-1R verdict corrected per A-9 formula: 8/11 = **NO-SHIP**. The Mode-B suffix was misapplied to partial completion (A-9: the suffix applies only when all items land with CI absent — never to partial completion). PG-1R2 closes the gap. The historical PG-1R report stands unmodified (rule 3).
Scorecard: unchanged from PG-1R (no code landed this run).

## 1. Pre-flight (§2.1–§2.3)
- Lineage: head `4f9f08b` (strict descendant of `894755e` ✓); tree clean ✓.
- Baseline re-derived **273 passed / 0 failed / 5 ignored** across 14 binaries ✓ (matches PROGRAM_REPORT_PG1R §4 reference exactly).
- Tools: clippy `-D warnings` 0 ✓ · fmt clean ✓ · `cargo audit --deny warnings` exit 0 ✓ (allowlist unchanged) · `cargo deny` all-ok ✓ · machete clean ✓.
- A-5 pre-state: `todo!/unimplemented!/FIXME` = 0 · `allow(dead_code)` = 0 · `register_chunk_verified` = 0 (carried) ✓.
- Design docs frozen at merge `ac7fb8e` (boot.md 36L, crash-consistency.md 35L, storage-format-v2.md 139L, wal.md 96L) ✓.
- **CI Mode A result: UNMET** — intake contains no run URLs for the PG-1R push. The attached `ci-errors.txt` is stale pre-F1–F6 evidence (its six failure clusters are precisely what fixes F1–F6 addressed); it is not green-run evidence for head `4f9f08b`, and per A-14 the F1–F6 fixes + I-11 discriminator are load-bearing until Linux CI is green on the current tree.
- **Errata §6 sign-off: NOT CONFIRMED** — I-3c rotate-keys parked per A-12 (I-3a/I-3b would proceed if the run were not blocked).

## 2. Provenance
Empty — no item work per §2.2/A-14. The only commit this run is this report (`docs(pg1r2): BLOCKED ...`).

## 3. Per-item delivery
None started (§2.2). The six contracted items (I-1a, I-1b, I-3a/b/c, I-5) plus I-12/I-13 are fully specified in the V9-R2 contract §3 and untouched.

## 4. Test re-derivation per binary
174p/1i unit · 0p main · 19p gRPC · 16p(+1i) integration · 6p property · 1p c01-boot · 23p(+1i) hardening · 6p m01 · 5p m04 · 10p m05 · 5p m07 · 5p race_gc · 3p replica_purge · 0p(+2i) soak → **273/0** ✓ (matches the §2.1 reference; the first streamed parse under-counted at 258 due to pipe truncation — the file-captured re-run above is authoritative, per rule 10).

## 5. Tool suite + rg deltas + failpoint matrix
All green (§1). Failpoint matrix: unchanged from PG-1R (6 PG-1-owned points wired); the I-1a point (`summary-write`) is specified in contract §3 but not wired — BLOCKED.

## 6. Errata status
Errata §6 (legacy-shared-chunk rotate-keys semantics) is recorded in PROGRAM_REPORT_PG1R §6 only — the A-6 deviation stands uncorrected until I-3c runs. Sign-off: **not confirmed**; the errata will be appended to `docs/design/storage-format-v2.md` as the first commit of the I-3 branch when the follow-up run starts (A-12).

## 7. Parking lot
Unchanged: replication backoff jitter, Idempotency-Key storage (both PG-2).

## 8. Carry-forward
**The entire run is carried:** I-1a, I-1b, I-3a, I-3b, I-3c (blocked-on-sign-off), I-5, I-12, I-13 — all fully specified in V9-R2 §3. Blocking cause: **A-14 — no green Linux CI evidence for the PG-1R push.** Named unblock: operator pastes green run URLs in the next intake.

## 9. Operator actions
1. Push `main` (head `4f9f08b` carries F1–F6 + I-11 + the 8 landed PG-1R items).
2. Paste the CI run URLs for that push into the next intake message (green = the V9-R2 run unblocks; red = fix-forward per V9-R1 §9 before item work).
3. Sign off errata §6 (explicitly confirm the legacy-shared-chunk decision) in the same message to enable I-3c.
4. Then re-issue the V9-R2 prompt: the six items execute against a validated Mode A baseline.
