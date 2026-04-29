# Standard perf test runbook

Procedure for running the standard PUT performance test. This is the reference runbook for
both manual runs and the CI pipeline (`lab-perf.yml`).

Perf runs use RustFS server defaults for PUT object-lock behaviour (the `auto` preflight
path from the Phase 2 fix). The harness does **not** expose the old
`RUSTFS_PUTOBJECT_EXISTING_OBJECT_LOCK_PREFLIGHT` knob.

---

## Standard run procedure

> **Note:** The steps below assume the Phase 1 harness (`run-perf-test.sh`, `lib/cleanup.sh`) is in
> place. For a manual run before the harness is built, restart RustFS with the same topology
> and run the loadgen directly against the endpoint.

### Step 0 — Set up conf/paths.env

Copy `conf/paths.env.example` to `conf/paths.env` and fill in all values. Confirm
`TOPOLOGY_LABEL` matches the cluster you are testing.

### Step 1 — Forced cleanup (recommended before each run)

```bash
bash rustfs/scripts/perf/lib/cleanup.sh
```

This kills any running load generator, stops RustFS on all nodes, wipes data directories,
and restarts RustFS cleanly. Faster than waiting for the load generator's own S3 DELETE
cleanup. See [Cleanup notes](#cleanup-notes) below.

### Step 2 — Run the perf test

```bash
bash rustfs/scripts/perf/run-perf-test.sh \
  --duration 5m \
  --out /tmp/perf-results/run1
```

Record results from `/tmp/perf-results/run1/report.md`.

---

## Pass criteria

A run **passes** the automated regression gate when throughput is within tolerance of the
baseline for the current `TOPOLOGY_LABEL` in `benchmarks/baseline.json` (see
`scripts/perf/analyze/analyze.py`). You should also verify:

1. **Errors:** no unexpected load-generator errors
2. **Object-lock smoke tests** (run against a bucket with OL enabled):

| Scenario | Expected |
|----------|----------|
| PUT overwrite of object with legal-hold ON | `403 AccessDenied` |
| PUT overwrite of object with COMPLIANCE mode + future retain-until | `403 AccessDenied` |
| PUT overwrite with GOVERNANCE mode, no bypass header | `403 AccessDenied` |
| PUT overwrite with GOVERNANCE mode + `x-amz-bypass-governance-retention: true` | `200 OK` |

---

## Metrics table (fill in after each run)

| Field | Value |
|-------|-------|
| Date (UTC) | |
| RustFS git SHA | |
| Topology | |
| Duration | |
| Throughput (MB/s) | |
| avgOpMs | |
| maxOpMs | |
| node1 TX peak (Gbit/s) | |
| Errors | |
| OL smoke: legal-hold | pass/fail |
| OL smoke: COMPLIANCE | pass/fail |
| OL smoke: GOVERNANCE bypass | pass/fail |

---

## Fail / inconclusive

If the regression gate fails or throughput looks wrong:

1. Record the raw numbers in the metrics table above.
2. Check the `rustfs_put_trace` span for `put_object.existing_object_lock_inline_check` —
   look for unexpected hold-time growth. Add `--trace` to the next run to collect span data.
3. If the issue is not explained by throughput + bandwidth alone, compare CPU, disk, and NIC
   counters before attributing to a single subsystem.

---

## Historical context: the Phase 2 preflight experiments

During development, lab A/B runs compared `always` (legacy Shared-lock preflight on every PUT),
`auto` (tiered skip when no OL config), and `never` (force-skip). Production behaviour is
now fixed on the `auto` path; the harness no longer rotates those modes. For the measured
numbers from that period, see `findings-summary.md`.

---

## Sign-off

After the run passes automated and manual checks:

1. Fill in the metrics table above with final numbers, git SHA, and date.
2. Record the result in your tracking doc or PR description.
3. Update `benchmarks/baseline.json` if the measured numbers differ from the seeded values
   (run `python3 scripts/perf/analyze/analyze.py --update-baseline`).

---

## Cleanup notes

The load generator cleans up its own buckets via S3 DELETE (`-deleteOnlyOurBuckets`) but
this is slow — it issues one DELETE per object through the full RustFS stack. For back-to-back
runs, the faster path is `cleanup.sh`, which:

1. Kills the load generator immediately
2. Stops RustFS on all nodes
3. Wipes data directories directly on disk (`rm -rf /mnt/rustfs-disk{1,2}/*`)
4. Restarts RustFS and waits for health

This bypasses S3 entirely. Safe in a test environment; never do this in production.

---

## Tracing mode

Add `--trace` to any `run-perf-test.sh` call to enable the `RUST_LOG` recipe that captures
PUT lock spans on node1. Results land in `$OUT/trace/`. Node1 collection alone is sufficient
for regression detection and initial analysis.

### RUST_LOG recipe (node1)

```bash
RUST_LOG='error,rustfs_put_trace=debug,rustfs_ecstore::set_disk::write=error,h2=warn,hyper=warn,tonic=warn,reqwest=warn,tower=warn'
```

To also capture per-locker acquire timings (shows local vs remote leg split), append
`,rustfs_lock_rpc=debug`. To additionally break down local `LocalClient` time into
`lock_manager.shard_acquire` / `lock_shard.*` child spans (higher volume), also append
`,rustfs_lock_acquire_detail=debug`.

**Add holder/wait diagnostics** (optional; append to the recipe above):

```
,rustfs_lock_holder=debug
```

This emits structured events: `wait_blocked` (sampled), `exclusive_released`, and
`shared_released` (when hold >= `RUSTFS_LOCK_HOLDER_MIN_HOLD_LOG_MS`, default 20 ms).
Tune volume with:

- `RUSTFS_LOCK_HOLDER_TRACE_SAMPLE=N` — log one in N `wait_blocked` events (default 32;
  set 1 for maximum detail)
- `RUSTFS_LOCK_HOLDER_MIN_HOLD_LOG_MS=N` — minimum hold duration to emit release events

Restart RustFS on node1 after any `RUST_LOG` change.

### What the spans mean (node1)

| Span name | What it measures |
|-----------|-----------------|
| `put_object.post_encode_get_write_lock` | Full distributed write-lock acquire time (dominant latency — mean ~103 ms, p90 ~155 ms in baseline) |
| `distributed_lock.lock_client_acquire` | Per-locker acquire time; index 0 = local FastLock, remaining indices = one per remote node |
| `put_object.existing_object_lock_inline_check` | Inline OL validation under the post-encode write lock |

### Collecting and analysing trace logs

Obs JSONL logs are written to `$RUSTFS_OBS_LOG_DIRECTORY` (default `/var/logs/rustfs`) on
node1 during a `--trace` run. After collecting the log locally, use
`scripts/perf/analyze/obs_timing_from_jsonl.py` to produce a flat CSV and an HTML Gantt:

```bash
python3 scripts/perf/analyze/obs_timing_from_jsonl.py \
  --input node1-rustfs.log \
  --csv timing_flat.csv \
  --html timing_gantt.html \
  --html-max-traces 60 \
  --html-span-contains get_write_lock
```

> **Disk space note:** `rustfs_put_trace=debug` on node1 produces large files at high PUT
> rates. For a 5-minute run, budget ~500 MB-1 GB on the obs log volume.

### Manual deep-dive: adding peer nodes

For a full cross-node analysis (e.g. when node1 spans alone do not explain a regression),
also enable on each peer node:

```bash
# Peer nodes (node2, node3, ...) -- incoming lock RPC spans
RUST_LOG='error,rustfs_lock_rpc=debug,h2=warn,hyper=warn,tonic=warn,reqwest=warn,tower=warn'
```

Add shard-level detail with `,rustfs_lock_acquire_detail=debug`. Collect each peer's log
and pass additional `--input nodeN-rustfs.log` flags to the analysis script. Logs are
joined on the `trace_id` field, which appears on both the node1
`post_encode_get_write_lock` span and the corresponding `lock_rpc.handle_lock` spans on
each peer.
