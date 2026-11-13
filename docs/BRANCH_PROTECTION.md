# Branch Protection — file-storage

V6-17 operator requirement (the agent cannot set GitHub settings from
inside the repo). Apply with `scripts/apply_branch_protection.sh` (needs
`gh` + admin rights), or by hand per the table below.

## `main`

| Setting | Value |
|---|---|
| Required pull request reviews | 1 approval |
| Required status checks | fmt, clippy-test-linux, build-windows, security, proto, no-handler-validation, c01-boot-gate, binary-size (PRs), dashboard-smoke, ci-liveness |
| Require branches up-to-date | yes |
| Allow force pushes | no |
| Allow deletions | no |
| Allow squash merge | **no** (rule 18: rebase/merge only, per-finding commits preserved) |
| Required linear history | yes |

## `.github/workflows/*` — CODEOWNERS

```
# .github/CODEOWNERS (create if absent)
/.github/workflows/  @<repo-owner> @platform-team
/scripts/            @<repo-owner>
/poc_expect_fail.sh  @<repo-owner>
```

Rationale (V6 threat model §5, CI-as-attack-surface): workflow edits can
exfiltrate `GITHUB_TOKEN` or backdoor the release binary; they require
explicit owner review. No `pull_request_target` triggers exist or are
allowed (fork-PR secret exposure).

## CI secrets scoping

CI jobs generate their own secrets at runtime (`openssl rand`, in-job
hash-password); no repository secrets are referenced. Keep it that way —
a new job needing a repo secret requires a CODEOWNERS-reviewed PR.

## Applying (operator)

```bash
gh api repos/:owner/:repo/branches/main/protection -X PUT \
  --input - <<'JSON' < scripts/apply_branch_protection.json
{ "required_status_checks": { "strict": true, "contexts": [
  "fmt", "clippy-test-linux", "build-windows", "security", "proto",
  "no-handler-validation", "c01-boot-gate", "binary-size",
  "dashboard-smoke", "ci-liveness"] },
  "required_pull_request_reviews": { "required_approving_review_count": 1,
  "dismiss_stale_reviews": true },
  "enforce_admins": false, "restrictions": null,
  "allow_force_pushes": false, "allow_deletions": false,
  "required_linear_history": true,
  "allow_squash_merge": false, "allow_merge_commit": true,
  "allow_rebase_merge": true }
JSON
```
