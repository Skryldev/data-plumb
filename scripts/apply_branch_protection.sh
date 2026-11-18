#!/usr/bin/env bash
# V6-17 operator script: apply branch protection to `main`.
# Requires: gh CLI authenticated with admin rights on the repo.
# The JSON payload mirrors docs/BRANCH_PROTECTION.md exactly.
set -euo pipefail

REPO="${1:-$(git remote get-url origin 2>/dev/null | sed -E 's#.*[:/]([^/]+/[^/.]+)(\.git)?$#\1#')}"
: "${REPO:?usage: apply_branch_protection.sh [owner/repo] (or set origin)}"

cat <<'JSON' | gh api "repos/${REPO}/branches/main/protection" -X PUT --input -
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "fmt", "clippy-test-linux", "build-windows", "security", "proto",
      "no-handler-validation", "c01-boot-gate", "binary-size",
      "dashboard-smoke", "ci-liveness"
    ]
  },
  "required_pull_request_reviews": {
    "required_approving_review_count": 1,
    "dismiss_stale_reviews": true
  },
  "enforce_admins": false,
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "required_linear_history": true,
  "allow_squash_merge": false,
  "allow_merge_commit": true,
  "allow_rebase_merge": true
}
JSON

echo "branch protection applied to ${REPO}:main"
echo "next: add CODEOWNERS entry for .github/workflows/* (see docs/BRANCH_PROTECTION.md)"
