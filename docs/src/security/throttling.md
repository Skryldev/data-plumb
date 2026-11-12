# Throttling & Rate Limits

> Status: normative since V5 Wave 2 (audit findings M-05 + I-06). The
> login lockout is a security control and defaults ON.

## The two controls

| Control | Scope | Keyed by | Where |
|---|---|---|---|
| RPC governor (token bucket) | every gRPC call — unary **and** streaming (`Upload`, `Download`) | tenant id | `state.0.rate_limiter`, taken by the unary interceptor and at streaming-handler entry |
| Login throttle (exponential lockout) | `POST /api/v1/auth/login`, `AuthService/Login` | username | `security::throttle::LoginThrottle`, inside `AuthService::login` |

The REST path keeps its own per-IP governor; the gRPC governor is
per-tenant. Both transports share ONE governor instance for unary and
streaming calls — there is no streaming bypass (the pre-M-05 defect), and
CI + `streaming_governor_matches_unary` pin that.

## RPC governor

* `rate_limit.per_second` tokens/s per tenant, `rate_limit.burst` ceiling
  (defaults in `config.toml`); disabled configs pass through.
* Over rate → `429 Too Many Requests` / `RESOURCE_EXHAUSTED`, refused
  before any pipeline work.
* The tenant map is bounded (10 000 entries) with insertion-order
  oldest-first eviction — never a wholesale `clear()` (I-06). One flood
  cannot reset other tenants' budgets.

## Login throttle (M-05)

Config:

```toml
[auth.login_throttle]
enabled = true        # security control: defaults ON (fail-closed posture)
base_ms = 1000        # lockout after the first failure
multiplier = 2.0      # exponential: 1s, 2s, 4s, 8s, ...
lockout_cap_ms = 900000
map_capacity = 10000
```

Semantics:

1. `check` runs **before** the argon2 verify — a locked-out username is
   refused without spending the 64 MiB verify budget (`429` /
   `RESOURCE_EXHAUSTED`, message carries `retry_after=<ms>ms`).
2. Only `InvalidCredentials` grows the backoff. **Verifier/backend errors
   never count** (a flaky password store must not lock out its user).
3. Success resets the state.
4. Throttled logins are audited as `auth.login.throttled` (distinct from
   `auth.login.failure`) with the retry window in the detail field.

### Fail-open vs fail-closed (rule 17)

* The **throttle is fail-open**: a missing entry, a poisoned lock or any
  internal surprise degrades to "allow" — the throttle may never become a
  self-inflicted outage for legitimate users.
* The **auth verifier is fail-closed**: a verifier error NEVER
  authenticates.
* The two failure modes are asserted by `verifier_error_does_not_throttle`
  (tests/regression/m05_throttle.rs).

## Regression gates

`tests/regression/m05_throttle.rs`: `grpc_stream_flood_throttled`,
`login_lockout_exponential`,
`login_throttle_correct_password_inside_window_still_rejected`,
`login_throttle_audited`, `throttle_map_evicts_oldest_not_clear`,
`verifier_error_does_not_throttle`, `streaming_governor_matches_unary`.
