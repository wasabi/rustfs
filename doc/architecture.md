# Architecture

RustFS is a high-performance, distributed object storage system built in Rust with full S3 compatibility. This document summarizes the in-repository layout and main components.

## Overview

- **Core binary**: The `rustfs` crate is the main application (API, console, storage orchestration).
- **Workspace**: All shared logic lives in crates under `crates/`; see the root [Cargo.toml](../Cargo.toml) `[workspace].members` for the full list.

## Main Components

| Area | Crates | Purpose |
|------|--------|---------|
| **Core** | `rustfs` | Main server binary, S3 API, admin console, storage coordination |
| **Config** | `rustfs-config` | Configuration and environment variable handling |
| **Storage** | `rustfs-ecstore`, `rustfs-filemeta`, `rustfs-targets` | Erasure coding, metadata, target backends |
| **Auth & IAM** | `rustfs-appauth`, `rustfs-iam`, `rustfs-credentials`, `rustfs-keystone` | Authentication, policies, Keystone integration |
| **Security** | `rustfs-crypto`, `rustfs-kms` | Cryptography and key management |
| **Operations** | `rustfs-heal`, `rustfs-scanner`, `rustfs-audit` | Healing, scanning, audit targets |
| **Observability** | `rustfs-metrics`, `rustfs-obs`, `rustfs-notify` | Metrics, tracing, event notifications |
| **Protocols** | `rustfs-protocols`, `rustfs-s3select-api`, `rustfs-s3select-query` | FTPS/SFTP, S3 Select |
| **Utilities** | `rustfs-common`, `rustfs-utils`, `rustfs-rio`, `rustfs-workers` | Shared types, I/O, worker pools |

The root [Cargo.toml](../Cargo.toml) is the source of truth for crate names and membership.

## Scoped Guidance

Path-specific rules for contributors and tooling are in `AGENTS.md` files:

- [.github/AGENTS.md](../.github/AGENTS.md)
- [crates/AGENTS.md](../crates/AGENTS.md)
- [crates/config/AGENTS.md](../crates/config/AGENTS.md)
- Additional scoped files under `crates/`, `rustfs/src/admin/`, and `rustfs/src/storage/`

See the root [AGENTS.md](../AGENTS.md) for precedence and mandatory pre-commit checks.
