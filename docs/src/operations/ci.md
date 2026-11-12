# CI

> Status: V5 Wave 2. The matrix gates every fix wave; each job traces
> back to an audit finding.

## Jobs (`.github/workflows/ci.yml`)

| Job | What it gates | Origin |
|---|---|---|
| `fmt` | `cargo fmt --all -- --check` | hygiene |
| `clippy-test-linux` | clippy `-D warnings` + full `cargo test --locked --all-targets` | hygiene / regression suites |
| `build-windows` | `cargo check --target x86_64-pc-windows-msvc --all-targets` + tests with `--skip platform_unix` (M-13 cfg-gates unix-only code) | M-13 / CI-W1 |
| `security` | `cargo audit` (0 non-allowlisted advisories) + `cargo deny check` | supply chain |
| `proto` | `buf lint` + `buf breaking --against main` (additive-only wire) | M-15 / iron rule "wire only adds" |
| `no-handler-validation` | `scripts/check_no_handler_validation.sh` — validation logic must live in `core::validation` | M-04 / V5 rule 15 |
| `c01-boot-gate` | secretless shipped config refuses to boot naming its violations; secreted config boots, serves `/health`, and the C-01 shipped password (`admin123`) is rejected | C-01 / CI-W2 |
| `binary-size` | musl release binary < 15 MiB (baseline 12 MiB, fail at +25%); size commented on the PR | CI-W3 |
| `dashboard-smoke` | boots a secreted stack, seeds traffic, asserts the series the Grafana dashboard depends on (`scripts/dashboard_smoke.sh`) | CI-W5 |
| `docker` | image build + C-01 gate + `poc_expect_fail.sh` expect-fail smoke against compose | C-01/C-03 |
| `nightly` (schedule) | bench + `h01_upload_memory_bound` memory gate on the release box | H-01/I-04 |

## Running the gates locally

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
./scripts/check_no_handler_validation.sh

# C-01 boot story (paths overridden for a scratch boot):
cargo build --release --bin file-storage
FS_STORAGE__ROOT=/tmp/fsroot/blocks FS_STORAGE__TEMP_DIR=/tmp/fsroot/tmp \
FS_DATABASE__PATH=/tmp/fsroot/index.redb FS_ENCRYPTION__KEYS_DIR=/tmp/fsroot/keys \
  ./target/release/file-storage --config secrets/file-storage.yaml.example
# (fill the PASTE_ placeholders first; see secrets/file-storage.yaml.example)

# dashboard smoke (seeds traffic itself):
./scripts/dashboard_smoke.sh http://127.0.0.1:8080
```

## Notes

* The Prometheus exposition is **sparse**: only touched series render.
  `dashboard_smoke.sh` seeds traffic (login/upload/download ×2/admin
  dry-run reconcile) before asserting, and treats pure alarm counters
  (`hash_mismatch`, `repair_hash_mismatch_total`,
  `gc_reconcile_leaked_total`, `register_race`) as legitimately absent
  on a healthy node — dashboards query them with `rate()`/`absent()`.
* The fuzz targets under `fuzz/` run out-of-band (`cargo +nightly fuzz`,
  I-04); they are not part of the default workspace on purpose.
* Background instances: always `kill -9` the exact PID in scripts — the
  binary's graceful shutdown waits on in-flight replication workers.
