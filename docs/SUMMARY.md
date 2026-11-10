# Summary

[Introduction](./src/README.md)

# Architecture
- [Overview](./src/architecture/overview.md)
- [Data Flow](./src/architecture/data-flow.md)
- [Storage Layout](./src/architecture/storage-layout.md)
- [Chunking & Dedup](./src/architecture/chunking.md)
- [Caching](./src/architecture/caching.md)
- [Replication](./src/architecture/replication.md)
- [Garbage Collection](./src/architecture/gc.md)
- [Security](./src/architecture/security.md)
- [ADR 0001 — CAS block store](./src/architecture/decisions/0001-cas-over-block-store.md)
  - [ADR 0002 — FastCDC](./src/architecture/decisions/0002-fastcdc-over-fixed-chunking.md)
  - [ADR 0003 — redb](./src/architecture/decisions/0003-redb-over-sled.md)
  - [ADR 0004 — moka cache hierarchy](./src/architecture/decisions/0004-moka-cache-hierarchy.md)
  - [ADR 0005 — AES-GCM per-tenant](./src/architecture/decisions/0005-aes-gcm-per-tenant-key.md)
  - [ADR 0006 — WAL shipping](./src/architecture/decisions/0006-wal-shipping-replication.md)
  - [ADR 0007 — Encrypted dedup key](./src/architecture/decisions/0007-encrypted-dedup-key-derivation.md)
  - [ADR 0008 — gRPC alongside REST](./src/architecture/decisions/0008-grpc-alongside-rest.md)

# Getting Started
- [Quickstart](./src/getting-started/quickstart.md)
- [Installation](./src/getting-started/installation.md)
- [Docker](./src/getting-started/docker.md)
- [Your First File](./src/getting-started/first-file.md)

# API Reference
- [REST](./src/api/rest.md)
- [gRPC](./src/api/grpc.md)
- [Errors](./src/api/errors.md)
- [Authentication](./src/api/authentication.md)
- [Examples](./src/api/examples.md)

# Configuration
- [Reference](./src/configuration/reference.md)
- [Environment Variables](./src/configuration/environment.md)
- [Production](./src/configuration/production.md)
- [Secrets](./src/configuration/secrets.md)

# Deployment
- [Docker Compose](./src/deployment/docker-compose.md)
- [Kubernetes](./src/deployment/kubernetes.md)
- [Scaling](./src/deployment/scaling.md)
- [TLS](./src/deployment/tls.md)

# Security
- [Threat Model](./src/security/threat-model.md)
- [Authentication](./src/security/authentication.md)
- [Encryption at Rest](./src/security/encryption.md)
- [Tenants](./src/security/tenants.md)
- [Audit Log](./src/security/audit.md)
- [Hardening Checklist](./src/security/hardening.md)

# Operations
- [Monitoring](./src/operations/monitoring.md)
- [Backup & Restore](./src/operations/backup-restore.md)
- [GC Operations](./src/operations/gc-operations.md)
- [Replication Ops](./src/operations/replication-ops.md)
- [Troubleshooting](./src/operations/troubleshooting.md)
- [Runbook](./src/operations/runbook.md)

# Integration
- [Rust Client](./src/integration/rust-client.md)
- [Go Client](./src/integration/go-client.md)
- [Python Client](./src/integration/python-client.md)
- [CLI Recipes](./src/integration/cli.md)
- [SDK Design](./src/integration/sdk-design.md)

# Performance
- [Benchmarks](./src/performance/benchmarks.md)
- [Tuning](./src/performance/tuning.md)
- [Profiling](./src/performance/profiling.md)

# Migration
- [v0 → v1](./src/migration/v0-to-v1.md)
