# Configuration Overview

RustFS is configured via environment variables. This page gives an overview; the canonical source of truth is the `rustfs-config` crate.

## Environment Variables

- **Naming**: Use flat `RUSTFS_*` names (e.g. `RUSTFS_REGION`, `RUSTFS_ADDRESS`, `RUSTFS_VOLUMES`). Do not use module-segmented names like `RUSTFS_CONFIG_*`.
- **Constants and conventions**: See [crates/config/src/constants/](../crates/config/src/) and [crates/config/README.md](../crates/config/README.md) (including environment variable naming).

## Key Variables (examples)

| Variable | Purpose |
|----------|---------|
| `RUSTFS_REGION` | Deployment region |
| `RUSTFS_ADDRESS` | Server address |
| `RUSTFS_VOLUMES` | Storage volumes |
| `RUSTFS_SCANNER_ENABLED` | Enable scanner |
| `RUSTFS_HEAL_ENABLED` | Enable healing |

Deprecated aliases are documented in [crates/config/README.md](../crates/config/README.md); compatibility behavior is maintained there.

## Full Configuration Reference

For all options, defaults, and deployment guides (including TLS and Keystone), use the official [RustFS Documentation](https://docs.rustfs.com).
