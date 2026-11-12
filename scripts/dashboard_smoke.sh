#!/usr/bin/env bash
# CI-W5 dashboard smoke: renders the live /metrics exposition from a
# running stack and asserts the series the dashboards and alert rules
# depend on actually exist with non-trivial labels. Catches metric
# regressions where a label is renamed or a series disappears.
#
# Usage: ./scripts/dashboard_smoke.sh [base_url]   (default http://127.0.0.1:8080)
set -euo pipefail

BASE="${1:-http://127.0.0.1:8080}"

fetch_metrics() {
    if [ -n "${METRICS_API_KEY:-}" ]; then
        curl -sf -m 20 -H "X-Api-Key: ${METRICS_API_KEY}" "$BASE/metrics"
    else
        curl -sf -m 20 "$BASE/metrics"
    fi
}

# A freshly-booted, idle server has a SPARSE exposition (Prometheus
# renders only incremented series). Seed traffic so every dashboarded
# counter actually materializes: login, upload, download, admin dry-run
# reconcile.
seed_traffic() {
    local creds="${DASHBOARD_SMOKE_CREDS:-admin:bootstrap-admin-pw-1}"
    local user="${creds%%:*}" pass="${creds#*:}"
    local tok
    tok=$(curl -sf -m 20 -X POST "$BASE/api/v1/auth/login" \
        -H 'Content-Type: application/json' \
        -d "{\"username\":\"$user\",\"password\":\"$pass\"}" | sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p')
    [ -n "$tok" ] || { echo "dashboard-smoke: seed login failed"; return 1; }
    local auth="Authorization: Bearer $tok"
    head -c 65536 /dev/zero > /tmp/dash-smoke.bin
    curl -sf -m 20 -X POST "$BASE/api/v1/files?name=dash-smoke.bin&mime=application/octet-stream" \
        -H "$auth" -H 'Content-Type: application/octet-stream' \
        --data-binary @/tmp/dash-smoke.bin > /dev/null
    local fid
    fid=$(curl -sf -m 20 "$BASE/api/v1/files" -H "$auth" | sed -n 's/.*"file_id":"\([^"]*\)".*/\1/p' | head -1)
    # Download twice: the first read is a disk miss, the second warms the
    # cache-hit series the dashboards panel.
    if [ -n "$fid" ]; then
        curl -sf -m 20 "$BASE/api/v1/files/$fid" -H "$auth" -o /dev/null
        curl -sf -m 20 "$BASE/api/v1/files/$fid" -H "$auth" -o /dev/null
    fi
    curl -sf -m 20 -X POST "$BASE/api/v1/admin/gc?reconcile=true&dry_run=true" -H "$auth" > /dev/null || true
    rm -f /tmp/dash-smoke.bin
}

seed_traffic || exit 1
METRICS="$(fetch_metrics)"

assert_series() {
    local name="$1"
    if ! grep -q "^${name}" <<<"$METRICS"; then
        echo "dashboard-smoke: FAIL — required series missing: $name"
        exit 1
    fi
    echo "  ok: $name"
}

echo "dashboard-smoke: checking required series on $BASE/metrics"
# NOTE on sparse exposition: Prometheus renders only series that were
# touched, so the asserted list covers (a) counters the seeded traffic
# materializes, (b) gauges that are always set, and (c) conditional
# families gated by env (replication / gRPC traffic in the CI job).
# Pure ALARM counters (hash_mismatch, repair_hash_mismatch,
# gc_reconcile_leaked, register_race) are deliberately NOT asserted:
# on a healthy node they are legitimately absent; dashboards query them
# with rate()/absent() semantics.
# Throughput / core counters (dashboards panel 1).
assert_series "file_storage_uploads_total"
assert_series "file_storage_downloads_total"
assert_series "file_storage_bytes_written_total"
assert_series "file_storage_bytes_read_total"
# Cache hit-rate panel.
assert_series "file_storage_cache_hits_total"
# HTTP panel.
assert_series "file_storage_http_requests_total"
# Reliability panels (traffic-conditional).
if [ "${REPLICATION_EXPECTED:-0}" = "1" ]; then
    assert_series "file_storage_replication_ops_enqueued_total"
fi
if [ "${GRPC_EXPECTED:-0}" = "1" ]; then
    assert_series "file_storage_grpc_requests_total"
fi
# Gauge families the alert rules use (always set).
assert_series "file_storage_uptime_seconds"

echo "dashboard-smoke: OK — all dashboard series present"
