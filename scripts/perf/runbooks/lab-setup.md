# Lab setup reference

Reference doc for the physical lab used for RustFS performance testing. Read this before
running any perf script or interpreting results.

---

## Cluster topologies

Two cluster sizes are supported. Topology is selected via `TOPOLOGY_LABEL` in
`conf/paths.env` (see `conf/paths.env.example`).

### Two-node (EC:1)

| Item | Value |
|------|-------|
| Nodes | node1, node2 |
| Disks per node | 2 (`/mnt/rustfs-disk1`, `/mnt/rustfs-disk2`) |
| `RUSTFS_VOLUMES` | `http://node{1...2}:9000/mnt/rustfs-disk{1...2}` |
| Erasure code | EC:1 |
| Load generator | node1 → `http://127.0.0.1:9000` (or remote) |
| Inter-node traffic | eno1409 and eno1419 (direct, no bond) |
| `TOPOLOGY_LABEL` | `two-node` |

### Three-node (EC:1)

| Item | Value |
|------|-------|
| Nodes | node1, node2, node3 |
| Disks per node | 2 (`/mnt/rustfs-disk1`, `/mnt/rustfs-disk2`) |
| `RUSTFS_VOLUMES` | `http://node{1...3}:9000/mnt/rustfs-disk{1...2}` |
| Erasure code | EC:1 |
| Load generator | node1 → `http://127.0.0.1:9000` (or remote) |
| Inter-node traffic | eno1409 and eno1419 (direct, no bond) |
| `TOPOLOGY_LABEL` | `three-node` |

---

## RustFS environment (all nodes)

These variables are the same on every node. `deploy.sh` expands
`conf/rustfs.env.template` with `RUSTFS_VOLUMES` and `RUST_LOG` from the current run.

```bash
RUSTFS_ADDRESS=":9000"
RUSTFS_CONSOLE_ENABLE=true
RUSTFS_STORAGE_CLASS_STANDARD=EC:1
RUSTFS_RUNTIME_WORKER_THREADS=64
RUSTFS_RUNTIME_MAX_BLOCKING_THREADS=4096
RUSTFS_OBS_LOG_DIRECTORY=/var/logs/rustfs   # server obs JSONL output dir
RUSTFS_OBS_ENVIRONMENT=production           # emits span-close records only (smaller logs)
RUST_LOG=error                              # raised to debug recipe when --trace is set
```

Credentials (`RUSTFS_ACCESS_KEY`, `RUSTFS_SECRET_KEY`) are injected from environment
variables at deploy time. Never store real values in `rustfs.env.template`.

---

## Standard load-generator workload

The reference workload for all A/B comparisons (same parameters used throughout the
two-node investigation):

```
-errorLimit 0
-z 0-1500K        object sizes 0–1500 KB (mixed)
-b 20             20 buckets
-t 100            100 concurrent threads
-put 100          100% PUT, no GET/LIST/DELETE
-awschunked 0
-dur 5m           (5 min for CI; 10 min for manual deep runs)
-deleteOnlyOurBuckets
```

Server: `$LOADGEN_ENDPOINT` (default `http://127.0.0.1:9000`).
Config file: `$LOADGEN_CFG` (your `local.cfg`).

---

## Network

**Interfaces:** eno1409 and eno1419 are used directly for inter-node RustFS traffic on all
nodes (no bonding). Each is a 10 Gbit/s link.

**Measured TCP ceiling (iperf3):** ~9.41 Gbit/s receiver in each direction between node1
and node2 (measured per interface).

**Effective ceiling in practice:** RustFS PUT traffic saturates at roughly ~5 Gbit/s TX on
node1 when the bottleneck shifts to the network path (e.g. with the Phase 2 preflight fix in effect).

To verify the ceiling before a test run:

```bash
# On node2 (receiver):
iperf3 -s

# On node1 (sender):
iperf3 -c node2 -t 10 -P 4
```

---

## Pre-flight checklist (quick)

Run through before any test you intend to compare against a baseline:

- [ ] Both nodes reachable via SSH from the runner
- [ ] `conf/paths.env` filled in (copy from `paths.env.example`)
- [ ] `RUSTFS_VOLUMES` matches intended topology
- [ ] No other heavy workload running on any cluster node
- [ ] Clock sync verified (NTP/chrony) if correlating logs across nodes

For a more comprehensive checklist (LB, client parity, NIC verification), see
`lab-setup.md` in the RustFS performance investigation archive (available separately).
