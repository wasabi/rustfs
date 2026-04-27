# Performance findings summary

This doc distills the key conclusions from the two-node EC:1 PUT performance investigation
(Apr 2026). It is the primary context document for the `perf-interpret` Skill and for
anyone interpreting a perf report without having read the full investigation archive.

For deep context, code maps, and raw traces, see the RustFS performance investigation
archive (available separately).

---

## Measured baselines (same workload: 20 buckets, 100 threads, 100% PUT, 0–1500K objects)

| Configuration | PUT throughput | avg op time | Notes |
|---------------|---------------|-------------|-------|
| Single-node EC:1 (two local disks) | ~2.4–2.6 GB/s | ~27–28 ms | No inter-node traffic |
| Two-node EC:1, locks on (`always`) | ~660–670 MB/s | ~108–110 ms | ~4× slower than single-node |
| Two-node EC:1, locks off (control) | ~1.1 GB/s | ~64 ms | Lock wait confirmed as dominant cost |
| Two-node EC:1, preflight skip (`auto`/`never`) | ~1.1 GB/s | ~64 ms | TX-bandwidth-limited on NIC (eno1409/eno1419) |

The ~4× single-to-two-node regression is the central fact of this investigation.

---

## What we attribute the regression to

The dominant share of extra wall time on the two-node PUT critical path is **waiting on
the distributed post-encode namespace write lock** (`get_write_lock`), not CPU-bound
erasure encoding or rename, and not aggregate host CPU, disk queue, or TCP loss.

### Evidence

- **Tracing (node1):** `put_object.post_encode_get_write_lock` span shows mean `time.idle`
  of ~103 ms, p90 ~155 ms. This is the span where the server waits for the lock —
  `time.idle` in obs spans means the coroutine was parked (not on CPU).
- **Tracing (node2):** `lock_rpc.handle_lock` spans are **bimodal**: ~50% of acquisitions
  complete in <1 ms (fast grant); the remaining ~50% show 50–200 ms tail (peer's FastLock
  was held by another operation). This bimodal pattern aligns exactly with node1's
  `get_write_lock` tail.
- **Host telemetry:** Neither node is CPU-saturated (node1 peak ~13% usr + ~5% sys across
  80 CPUs). NVMe `w_await` stays <3 ms. No kernel softnet drops. This rules out hardware
  saturation as the primary cause.
- **Locks-off control:** Disabling distributed locking (`RUSTFS_LOCK_ENABLED=false`) raises
  client throughput from ~660 MB/s to ~1.1 GB/s — pointing directly at the lock path as
  the bottleneck.

### Lock path anatomy (two-node EC:1)

```
put_object (node1)
  └── post_encode_get_write_lock
        └── DistributedLock::lock_guard
              ├── LocalClient (node1 FastLock)        — fast, negligible idle
              └── RemoteClient → gRPC → node2 (via eno1409/eno1419)
                    └── handle_lock (node2 FastLock)  — bimodal: fast or ~100–200 ms
```

`DistributedLock` uses `join_all` — it waits for **all** lock clients before returning,
so wall time is at least the slowest peer each round. With write quorum = 2 (both nodes
must grant), every POST-encode lock wait includes a full remote round-trip.

---

## What we tried that did NOT help

| Lever | Result | Why |
|-------|--------|-----|
| Shorter write-lock hold time (rename+commit window only) | No measurable delta | Lock wait is in **acquire** (idle/waiting), not in hold time (busy under guard) |
| `join_all` prototype (stop after quorum) | No measurable delta at N=2 lockers | With only 2 lockers and quorum=2, stopping early never fires |

---

## The Phase 2 preflight fix

**What it does:** removes a redundant distributed Shared-lock preflight that happened
*before* encode on every PUT. Controlled by
`RUSTFS_PUTOBJECT_EXISTING_OBJECT_LOCK_PREFLIGHT=auto|always|never`.

**Why it helps:** `auto` mode skips the Shared-lock preflight for buckets with no
object-lock configuration (the common case). Object-lock validation is moved inline under
the post-encode Exclusive lock that already exists. This eliminates one full distributed
lock round-trip per PUT on the common path.

**Result:** throughput jumps from ~660 MB/s (`always`) to ~1.1 GB/s (`auto`/`never`),
matching the locks-off control. The remaining bottleneck at ~1.1 GB/s is NIC TX bandwidth
(~5 Gbit/s observed on eno1409/eno1419 under RustFS PUT load).

---

## How to read a perf report: signal interpretation guide

### Throughput vs baselines

| What you see | Interpretation |
|---|---|
| `auto` ≈ `never` ≈ 1.1 GB/s, `always` ≈ 665 MB/s | Normal — preflight fix working as designed |
| `auto` drops toward `always` (~665 MB/s) | Regression in the preflight skip — check `put_object.existing_object_lock_inline_check` span hold time; run `--trace` |
| All variants drop below `always` baseline | Broader regression — check CPU, disk, NIC counters; may be a lock-path or EC-path change |
| All variants above `always` but below `auto` baseline | Partial regression or noise — compare node1 TX; if near 5 Gbit/s, may be bandwidth-limited |

### NIC bandwidth (eno1409 / eno1419)

| What you see | Interpretation |
|---|---|
| node1 TX ~5 Gbit/s, throughput ~1.1 GB/s | Bandwidth-limited — this is expected for `auto`/`never` |
| node1 TX well below ~5 Gbit/s, throughput low | Software-path bottleneck (lock, EC, RPC) — not network-limited |
| node1 TX near 0, but throughput also low | Check whether RustFS is running and healthy on all nodes |

### CPU

- Neither node should be aggregate CPU-saturated at these workloads (<~20% usr on 80 CPUs
  is normal). If either node is pegging CPU, that is a new signal worth investigating.

### When to enable tracing (`--trace`)

- A regression is confirmed but the cause is not obvious from throughput + bandwidth alone
- You want to see per-span lock wait latency distributions
- You are running an A/B on a new code change and want to verify the lock path behavior

---

## Caveats (do not drop when interpreting)

1. **Network ceiling:** inter-node traffic runs over eno1409 and eno1419 directly (no
   bonding). Each is a 10 Gbit/s link; the measured iperf3 ceiling is ~9.41 Gbit/s per
   interface. When node1 TX approaches ~5 Gbit/s during a PUT run, the bottleneck is
   shifting toward the network path. See `lab-setup.md` for details.
2. **Causality label:** we proved *where* time accumulates on the traced path; we did not
   fully prove a single root label ("bug" vs "intentional serialization" vs "tuning only").
3. **Unique-key workload:** the standard loadgen issues unique keys per PUT. The bimodal
   lock wait pattern is not explained by two concurrent PUTs to the same key — that was
   explicitly verified.
4. **Three-node baselines:** not yet measured. The two-node numbers above do not apply
   directly. Seed `benchmarks/baseline.json` after the first three-node run.
