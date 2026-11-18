# Configuration Reference

Every configuration key with default, effect, and boot validation.
Derivation method: grep of `serde(default)` fields + defaults functions in
`src/config.rs`, cross-checked against `config/default.yaml` (2026-11-18,
PG-1R run; unknown-key boot WARN lint is pending PG-4 §5.4).

## server
| key | default | effect |
|---|---|---|
| host | 127.0.0.1 | bind address for REST+gRPC |
| port / grpc_port | 8080 / 9090 | listeners |
| grpc_tls.enabled | false | server TLS; requires cert/key paths |
| grpc_tls.client_ca_path | null | mTLS when set |

## storage
| key | default | effect |
|---|---|---|
| root | data/blocks | CAS blob tree (sharded) |
| temp_dir | data/tmp | multipart spool |
| shard_depth | 2 | shard levels (boot-validated ≤4) |
| quarantine_max_bytes | 1 GiB | M-07 quarantine bound (oldest evicted) |
| quarantine_retention_secs | 7d | boot sweep window |

## database
| key | default | effect |
|---|---|---|
| path | data/index.redb | redb file |

## chunking / compression
| key | default | effect |
|---|---|---|
| chunking.min/avg/max | 8192/32768/131072 | FastCDC bounds (validated: min>0, min≤avg≤max, max≤16 MiB) |
| compression.level / min_size | 3 / 1024 | zstd (level validated 1..=22) |

## cache
| key | default | effect |
|---|---|---|
| l1_max_bytes / l1_ttl_secs | 256 MiB / 300 | plaintext chunk cache |
| l2_max_bytes / l2_ttl_secs | 512 MiB / 3600 | blob cache |
| manifest_max_bytes | 64 MiB | L3 byte budget (H-01) |
| manifest_ttl_secs | 60 | L3 TTL |

## auth
| key | default | effect |
|---|---|---|
| jwt_secret | — | ≥32 chars, no shipped literals (C-01 fail-closed) |
| access_ttl_secs / refresh_ttl_secs | 900 / 604800 | token lifetimes |
| login_throttle.enabled | true | per-username + per-IP exponential lockout (M-05) |
| login_throttle.base_ms / multiplier | 1000 / 2.0 | backoff ladder |
| login_throttle.lockout_cap_ms | 15 min | per-username cap |
| login_throttle.ip_lockout_cap_ms | 1 h | per-IP cap (boot-validated ≥ username cap — V8 C-5) |
| login_throttle.map_capacity / ip_map_capacity | 10k / 20k | oldest-evicted maps |

## dedup
| key | default | effect |
|---|---|---|
| scope | **tenant** | block key = `BLAKE3(tenant‖hash)` (M-16 closed); `global` = legacy raw-hash (WARN at boot) |

## wal (PG-1R I-9a — wal.md §6.2)
| key | default | effect |
|---|---|---|
| group_commit_delay_ms | 5 | batch window before the single fsync |
| group_commit_max_bytes | 262144 | flush-early threshold |
Both read at boot into the WAL builder; flip either and the builder value
changes (`wal_config_keys_read_at_boot`).

## migration
| key | default | effect |
|---|---|---|
| auto | **false** | `true` migrates v1→v2 inline on open; `false` refuses with the `xtask migrate` error |

## replication
| key | default | effect |
|---|---|---|
| enabled / mode | false / async | shipper on/off; sync blocks upload until ack |
| role | primary | replica refuses refcount GC (C-02) |
| target_url / upstream_url / api_key | null | peer config (api_key ≥24 chars, C-01) |
| queue_capacity / batch_size / flush_interval_ms | 10000 / 64 / 5000 | shipper bounds |
| sync_timeout_ms / enqueue_timeout_ms | 5000 / 5000 | M-14 bounded enqueue |

## retention
| key | default | effect |
|---|---|---|
| gc_mode | refcount | refcount / reachability / disabled |
| grace_period_secs / gc_interval_secs / orphan_min_age_secs | 86400 / 300 / 3600 | GC cadence |
| reconcile_interval_secs | 3600 | M-01 drift reconciliation (0 = off) |

## limits
| key | default | effect |
|---|---|---|
| max_file_size / max_body | 10 GiB / 1 GiB | upload caps |
| max_in_flight_bytes / max_concurrent_uploads | 2 GiB / 64 | H-01 admission control |
| max_quota_bytes | null | I-2 per-tenant quota (commit-txn re-check; None = unlimited) |
| internal_body | 11 GiB+64 MiB | /internal bundles (M-09) |

## metrics
| key | default | effect |
|---|---|---|
| exposure | loopback | loopback / api_key / public (warn) |
| api_key | — | required when exposure=api_key |

## validation
| key | default | effect |
|---|---|---|
| mode | enforce | magic-byte check (H-03); `off` logs loudly |
