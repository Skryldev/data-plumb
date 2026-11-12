#!/usr/bin/env bash
# audit-repro-v6/ci-checks.sh — static verification of the CI-workflow
# findings (V6-05, V6-06, V6-13). Runs anywhere with bash+grep; no Rust
# toolchain needed. Read-only.
set -u
cd "$(dirname "$0")/.."
CI=.github/workflows/ci.yml
PASS=0; FAIL=0
ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

echo "== [V6-05a] c01-boot-gate captures the pipeline's LAST exit code (tee) =="
# The step pipes the binary into tee and then reads $? — without pipefail
# that is tee's status (0), so the 'must refuse' check inverts.
if grep -q 'tee /tmp/boot.txt' "$CI" && ! grep -q 'pipefail' "$CI"; then
  ok "no pipefail + tee pipeline → \$? is tee's status (gate inverted)"
else
  bad "pattern not found — job may have been fixed"
fi

echo "== [V6-05b] boot output never contains the word 'violation' =="
# The grep gate looks for 'violation'; runtime error text is
# 'invalid configuration' + per-field messages (lib.rs / config.rs).
if ! grep -rn '"violation\|violation:' src/config.rs src/lib.rs | grep -v '^\s*//' | grep -q 'violation'; then
  ok "no runtime string contains 'violation' → grep can never match"
else
  bad "a runtime 'violation' string exists — re-check"
fi
grep -n "invalid configuration" src/lib.rs >/dev/null && ok "actual boot wording: 'invalid configuration' (lib.rs)"

echo "== [V6-06] nightly job gated on a schedule trigger that is not defined =="
if grep -q 'github.event_name == .schedule.' "$CI" && ! grep -q '^  schedule:' "$CI"; then
  ok "nightly job unreachable (no schedule: trigger in on:) — H-01 memory gate never runs"
else
  bad "schedule trigger present or job changed — re-check"
fi

echo "== [V6-06b] h01 memory-bound test lives only in the dead nightly job =="
REFS=$(grep -c 'h01_upload_memory_bound' "$CI")
if [ "$REFS" -ge 1 ] && [ "$(grep -rl 'h01_upload_memory_bound' .github | wc -l)" -eq 1 ] \
   && grep -q '#\[ignore' tests/regression/hardening.rs; then
  ok "h01_upload_memory_bound: #[ignore]d, referenced only inside ci.yml's nightly job"
else
  bad "memory-bound gate may run elsewhere — re-check"
fi

echo "== [V6-13] no-handler-validation grep holes =="
# The CI rule matches validate_* / check_size / len() <digits>. A handler
# using chars().count() or a named constant evades it. Demonstrate the
# evasion strings the pattern must catch but does not:
PATTERN='validate_name|validate_tags|validate_mime|check_size|len\(\)\s*[<>]=?\s*[0-9]{2,}'
EVASIONS=(
  'if name.chars().count() > 255 { return Err(...) }'
  'const MAX_NAME: usize = 255; if name.len() > MAX_NAME { ... }'
  'if mime.len() > MAX_MIME_LEN { ... }'
)
for e in "${EVASIONS[@]}"; do
  if ! echo "$e" | grep -qE "$PATTERN"; then
    ok "evasion accepted by the rule: $e"
  else
    bad "evasion actually caught: $e"
  fi
done

echo
echo "RESULT: $PASS passed, $FAIL failed (all PASS rows = defects verified)"
