# Summary

[Introduction](./README.md)

# Architecture
- [Overview](./architecture/overview.md)
- [Data Flow](./architecture/data-flow.md)
- [Storage Layout](./architecture/storage-layout.md)
- [Chunking & Dedup](./architecture/chunking.md)
- [Caching](./architecture/caching.md)
- [Replication](./architecture/replication.md)
- [Garbage Collection](./architecture/gc.md)
- [Security](./architecture/security.md)
- [ADR 0001 — CAS block store](./architecture/decisions/0001-cas-over-block-store.md)
  - [ADR 0002 — FastCDC](./architecture/decisions/0002-fastcdc-over-fixed-chunking.md)
  - [ADR 0003 — redb](./architecture/decisions/0003-redb-over-sled.md)
  - [ADR 0004 — moka cache hierarchy](./architecture/decisions/0004-moka-cache-hierarchy.md)
  - [ADR 0005 — AES-GCM per-tenant](./architecture/decisions/0005-aes-gcm-per-tenant-key.md)
  - [ADR 0006 — WAL shipping](./architecture/decisions/0006-wal-shipping-replication.md)
  - [ADR 0007 — Encrypted dedup key](./architecture/decisions/0007-encrypted-dedup-key-derivation.md)
  - [ADR 0008 — gRPC alongside REST](./architecture/decisions/0008-grpc-alongside-rest.md)

# Getting Started
- [Quickstart](./getting-started/quickstart.md)
- [Installation](./getting-started/installation.md)
- [Docker](./getting-started/docker.md)
- [Your First File](./getting-started/first-file.md)

# API Reference
- [REST](./api/rest.md)
- [gRPC](./api/grpc.md)
- [Errors](./api/errors.md)
- [Authentication](./api/authentication.md)
- [Examples](./api/examples.md)

# Configuration
- [Reference](./configuration/reference.md)
- [Environment Variables](./configuration/environment.md)
- [Production](./configuration/production.md)
- [Secrets](./configuration/secrets.md)

# Deployment
- [Docker Compose](./deployment/docker-compose.md)
- [Kubernetes](./deployment/kubernetes.md)
- [Scaling](./deployment/scaling.md)
- [TLS](./deployment/tls.md)

# Security
- [Threat Model](./security/threat-model.md)
- [Authentication](./security/authentication.md)
- [Encryption at Rest](./security/encryption.md)
- [Tenants](./security/tenants.md)
- [Audit Log](./security/audit.md)
- [Hardening Checklist](./security/hardening.md)

# Operations
- [Monitoring](./operations/monitoring.md)
- [Backup & Restore](./operations/backup-restore.md)
- [GC Operations](./operations/gc-operations.md)
- [Replication Ops](./operations/replication-ops.md)
- [Troubleshooting](./operations/troubleshooting.md)
- [Runbook](./operations/runbook.md)

# Integration
- [Rust Client](./integration/rust-client.md)
- [Go Client](./integration/go-client.md)
- [Python Client](./integration/python-client.md)
- [CLI Recipes](./integration/cli.md)
- [SDK Design](./integration/sdk-design.md)

# Performance
- [Benchmarks](./performance/benchmarks.md)
- [Tuning](./performance/tuning.md)
- [Profiling](./performance/profiling.md)

# Migration
- [v0 → v1](./migration/v0-to-v1.md)
