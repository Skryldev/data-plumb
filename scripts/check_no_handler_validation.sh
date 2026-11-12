#!/usr/bin/env bash
# CI rule `no-handler-validation` (M-04 / V5 Wave 2 rule 15).
#
# Validation lives in `core::validation`, never in a handler. Transport
# handlers may only EXTRACT fields from the wire type into
# `UploadRequestView` and call the shared validators — any validation
# LOGIC re-entering a handler is a structural drift bug (the defect class
# the audit's M-04a..M-04e cataloged).
#
# NOTE on paths: V5 §3.7 names `src/transport/{rest,grpc}/**`; this tree's
# real transport dirs are `src/api/**` (REST) and `src/grpc/services/**`
# (gRPC). Same semantics, real names.
set -euo pipefail

cd "$(dirname "$0")/.."

PATTERN='validate_name|validate_tags|validate_mime|check_size|len\(\)\s*[<>]=?\s*[0-9]{2,}'

# grep exit codes: 0 = match (bad), 1 = no match (good). `|| true` keeps
# `set -e` from aborting on the expected no-match case.
hits=$(grep -rnE "$PATTERN" src/api src/grpc/services \
    --include='*.rs' | grep -v 'core::validation::' || true)

if [ -n "$hits" ]; then
    echo "no-handler-validation: FAIL — validation logic found in transport handlers:"
    echo "$hits"
    echo
    echo "Move the check into src/core/validation.rs (handlers extract, core validates)."
    exit 1
fi

echo "no-handler-validation: OK (no validation logic in src/api or src/grpc/services)"
