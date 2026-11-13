# Dependency Review Cadence (V6-OPS-02)

## Quarterly review (owner: repo maintainer; next: 2027-02)

1. `cargo audit` — every advisory must be either **fixed** (upgrade) or
   listed in `.cargo/audit.toml` **with a current rationale**. Entries
   whose rationale has expired (upgrade landed, usage removed, feature
   now exercised) must be removed from the list, not extended.
2. `cargo deny check` — same IDs mirrored in `deny.toml` (cargo-deny
   shares the RustSec DB). License additions need a one-line provenance
   note (which crate, why permissive).
3. `cargo update --dry-run` — record available semver-compatible bumps;
   apply non-breaking bumps, file issues for majors.

## Current accepted advisories (as of 2026-11)

| ID | Crate | Why accepted | Expires when |
|---|---|---|---|
| RUSTSEC-2025-0141 | bincode 1.3.3 | internal-only ChunkMeta codec; handled error path; no wire use | M-03 storage-format rewrite |
| RUSTSEC-2025-0134 | rustls-pemfile 2.2.0 | transitive via tonic 0.12 TLS; PEM parse at boot only | tonic 0.13+ upgrade |

## Liveness (rule 19)

The `security` CI job runs `cargo audit --deny warnings`:
- with the config: green (verified 2026-11-13, cargo-audit 0.23.0);
- with `.cargo/audit.toml`'s ignore list emptied: **fails** with
  "2 denied warnings found" (verified — the gate can say no).
