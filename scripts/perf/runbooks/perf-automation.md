# Perf automation — setup and first run

End-to-end guide for operators: lab prerequisites, `conf/paths.env`, a smoke run without CI,
reading `report.md` / `report.json`, updating baselines, and optional `--trace` investigation.

For cluster topology and NIC details, see [lab-setup.md](lab-setup.md). For the standard PUT
procedure and pass criteria, see [ab-test-runbook.md](ab-test-runbook.md).

---

## Prerequisites

### Software (runner / node1)

#### Rust toolchain

Perf builds use **`cargo`** on the machine that runs **`deploy.sh`** (typically node1 or your
dev machine if you build elsewhere). Install via **[rustup](https://rustup.rs/)** (the usual
Rust installer on Linux and macOS):

1. Follow the instructions at https://rustup.rs/ — typically:

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. Use the **default stable** toolchain unless the RustFS repo documents a specific version:

   ```bash
   rustup default stable
   rustup update
   ```

3. Ensure your shell loads Cargo’s bin directory (rustup prints this at install time). For
   example, add `source "$HOME/.cargo/env"` to `~/.bashrc` or `~/.zshrc`, or start a **new**
   terminal session.

4. Confirm:

   ```bash
   cargo --version
   rustc --version
   ```

Then from the RustFS checkout root, `cargo build --release -p rustfs` (what `deploy.sh` runs)
should succeed if OS packages required by **links** (for example OpenSSL dev headers on Linux)
are installed—see the RustFS repo **README** or **CONTRIBUTING** for distro-specific hints.

#### Other tools

- **OpenSSH client** — `ssh`, `scp` to peers listed in `PEER_NODES` (and optional
  `LOADGEN_HOST`).
- **sysstat** — `mpstat`, `iostat`, `sar` on every node that runs `lib/monitor.sh`
  (same package family as other perf scripts).
- **Python 3** — for `scripts/perf/analyze/analyze.py` and `check-pass.py`.

### Wasabi load generator

You need the **wasabi load-generator** binary and a JSON config (`local.cfg`). Set absolute
paths in `conf/paths.env`:

- `LOADGEN_BIN`
- `LOADGEN_CFG`

If your workspace symlinks `wasabi_main/` (see repo `AGENTS.md`), point `LOADGEN_BIN` at the
built binary there. There is no requirement that the binary be named `wasabi_main`; only the
paths you configure matter.

### Repository layout and `conf/paths.env`

`run-perf-test.sh` resolves lab config as:

```text
conf/paths.env   ← three directory levels above scripts/perf/
```

In the **rustfs-perf** workspace layout (outer repo + `rustfs/` submodule), that file is the
**workspace-root** `conf/paths.env`. Copy `conf/paths.env.example` → `conf/paths.env` and fill
in every required field (see table below). The example file lives next to `paths.env` at workspace
root in that layout.

If you maintain a standalone RustFS checkout without the outer workspace, place `paths.env`
under whatever directory is **three levels above** `scripts/perf/` per the script’s resolution,
or export the same variables in your shell before running (the script sources `paths.env` when
the file exists).

| Variable | Purpose |
|----------|---------|
| `TOPOLOGY_LABEL` | Baseline key (e.g. `two-node`, `three-node`) — must match `benchmarks/baseline.json` |
| `PEER_NODES` | Space-separated peers for SSH monitors / deploy (empty = single-node). Prefer omitting the machine that runs this script; if it appears under another DNS name, duplicate peer monitors/collection are skipped when `ssh` `hostname -s` on that peer matches this machine’s physical short name (`hostname -s`, or `PERF_ORCHESTRATOR_MACHINE`). Local telemetry goes under `$OUT/node-<report-label>/` (see below). |
| `PERF_ORCHESTRATOR_LABEL` | (Optional) RustFS-style DNS name for node1 in reports and artifact dirs (e.g. `rustfsnode1`). Defaults to `hostname -s`, or the first `PEER_NODES` entry whose `ssh hostname -s` matches this machine when label/host env vars are unset. Takes precedence over `PERF_ORCHESTRATOR_HOST` when both are set. |
| `PERF_ORCHESTRATOR_HOST` | Alias for `PERF_ORCHESTRATOR_LABEL` (backward compat). |
| `PERF_ORCHESTRATOR_MACHINE` | (Optional) Override the physical short hostname used only for same-host dedupe (`ssh` `hostname -s` on peers). Defaults to `hostname -s` on the orchestrator. |
| `RUSTFS_VOLUMES` | Full volumes string for deploy template |
| `DATA_DIRS` | Data dirs for `cleanup.sh` |
| `LOADGEN_BIN`, `LOADGEN_CFG` | Load generator binary and config — if `LOADGEN_HOST` is set, both paths are on that host (including for `cleanup.sh` / `--force-cleanup`) |
| `NIC_INTERFACES` | For SAR / ethtool (see [lab-setup.md](lab-setup.md)) |
| `RUSTFS_ACCESS_KEY`, `RUSTFS_SECRET_KEY` | Lab credentials (used by deploy) |

Optional: `LOADGEN_HOST` (SSH for remote loadgen), `RUSTFS_OBS_LOG_DIR` (defaults under
`/var/logs/rustfs` for `--trace`).

---

## Register a self-hosted GitHub Actions runner (node1)

Used when **Phase 6** (`lab-perf.yml`) runs against the lab. Adjust paths to your install location.

1. On **node1**, download the Actions runner package for your OS/arch from GitHub’s runner
   releases page (same major version as your organization’s supported runners).
2. Extract and configure:

```bash
mkdir -p ~/actions-runner && cd ~/actions-runner
# ./config.sh --url https://github.com/<ORG>/<REPO> --token <RUNNER_REGISTRATION_TOKEN>
```

Use a **registration token** from the repo **Settings → Actions → Runners → New runner**
(or org-level runners). Pick labels that match the workflow (e.g. `self-hosted`, `lab-perf`).

3. Install and start the service (Linux):

```bash
sudo ./svc.sh install
sudo ./svc.sh start
```

4. Confirm the runner appears online in GitHub and shows the expected labels.

Runner OS user must be able to run `cargo`, `ssh`/`scp` to peers, and invoke
`scripts/perf/run-perf-test.sh` (workflow passes `--no-deploy` and `--binary` when wired as in
the automation plan).

---

## GitHub secrets (lab CI)

Configure these in the **RustFS** repo (or org) secrets store for workflows that call the lab:

| Secret | Purpose |
|--------|---------|
| `LAB_NODE2_SSH_KEY` | Private key material for SSH from node1 to node2 (workflow installs under `~/.ssh`) |
| `RUSTFS_ACCESS_KEY` | S3 access key for perf runs |
| `RUSTFS_SECRET_KEY` | S3 secret key for perf runs |

Also ensure the runner environment exposes **`MULTINODE_REPO`** (or equivalent) if your workflow
invokes scripts via an absolute path to the checkout that contains `scripts/perf/`, per
`lab-perf.yml` in the automation plan.

---

## First local run (smoke test)

From the directory you normally use for perf (workspace root with rustfs-perf layout):

```bash
source conf/paths.env
bash rustfs/scripts/perf/run-perf-test.sh \
  --no-deploy \
  --duration 2m \
  --out /tmp/perf-smoke
```

`--no-deploy` assumes RustFS is already built, installed, and healthy on all nodes with the
binary you intend to test. Omit `--no-deploy` for a full build → ship → restart cycle via
`lib/deploy.sh`.

Artifacts appear under `--out`, including `run.log`, `loadgen.txt`, `meta.json`, per-node
telemetry, and after analysis `report.json` and `report.md`.

---

## Reading the report

### `report.md` (human-readable)

Typical sections:

| Section | Meaning |
|---------|---------|
| **Throughput** | Per-interval and mean MB/s from the load generator; `avgOpMs` / `maxOpMs` |
| **Network bandwidth** | Mean/peak RX and TX Gbit/s per interface from `sar -n DEV` (trimmed active window) |
| **CPU utilisation** | Mean active CPU % from `mpstat` |
| **Disk I/O** | Per-device write MB/s and `%util` from `iostat` |
| **Regression check** | Baseline MB/s, tolerance band, measured mean, Δ%, PASS/FAIL |

### `report.json` (machine-readable)

Same numbers in structured form under keys such as `loadgen`, `network`, `cpu`, `disk`, and
`regression`. CI gates often call `check-pass.py` on this file after upload.

### Regression gate and `delta_pct`

For topology key `TOPOLOGY_LABEL`, `analyze.py` loads `benchmarks/baseline.json`, compares mean
throughput to `throughput_MBs`, and applies `tolerance_pct` (default **10** if omitted).

`delta_pct` is:

```text
(mean_measured − baseline) / baseline × 100
```

The gate **passes** when throughput has not dropped more than the tolerance (implemented as
`delta_pct >= -tolerance_pct`). Large positive Δ means faster than baseline; negative Δ beyond
the band fails the gate. If the baseline entry is missing or `throughput_MBs` is `null`, the
gate is skipped (`check-pass.py` may report SKIP).

---

## Updating the baseline

After a run you accept as the new reference (stable lab, intentional improvement):

```bash
python3 rustfs/scripts/perf/analyze/analyze.py \
  --out /tmp/perf-smoke \
  --baseline rustfs/scripts/perf/benchmarks/baseline.json \
  --update-baseline
```

Use the same `--out` directory that contains `report.json` from that run. The script rewrites the
matching topology entry in `baseline.json` and prints a diff. Review before committing.

---

## Tracing mode (`--trace`)

Adds verbose `RUST_LOG` for lock/PUT tracing and collects observation JSONL from
`RUSTFS_OBS_LOG_DIR` (default `/var/logs/rustfs` on node1). After the run, `run-perf-test.sh`
invokes `analyze/obs_timing_from_jsonl.py` to produce:

- `$OUT/trace/timing_flat.csv`
- `$OUT/trace/timing_gantt.html`

These are **not** merged into `report.md`; open them alongside the report for lock-span analysis.
See `run-perf-test.sh` step 7 for exact flags (e.g. span filter around `get_write_lock`).

---

## Related scripts

| Script | Role |
|--------|------|
| `analyze/analyze.py` | Build `report.json` / `report.md`, regression exit code |
| `analyze/check-pass.py` | `PASS` / `FAIL` / `SKIP` from existing `report.json` (CI gate) |
| `lib/cleanup.sh` | Fast disk wipe + restart between runs |
| `runbooks/findings-summary.md` | Distilled lab conclusions for interpreting NIC vs lock limits |

---

## Troubleshooting

| Symptom | Check |
|---------|--------|
| `LOADGEN_BIN must be set` | `source conf/paths.env` before running; file path three levels above `scripts/perf/` |
| Empty peer telemetry | `PEER_NODES`, SSH keys, `scp` of `lib/*.sh` to peers; peer artifacts are pulled with **tar over ssh** (not `scp …/.`, which OpenSSH rejects) |
| `mkdir … Permission denied` on a peer under your `--out` path | Peers no longer reuse orchestrator `$OUT` on disk (avoids NFS `/tmp` or cross-user clashes). If you still see this, check that the SSH user can write under `/tmp` on peers. Hostnames like `rustfsnode1` vs `node1` only affect directory names, not this permission model. |
| Regression SKIP | Missing topology key or `null` throughput in `baseline.json` |
| No trace CSV/HTML | JSONL not produced (`RUSTFS_OBS_LOG_DIRECTORY`, permissions on obs dir), or `obs_timing_from_jsonl.py` stderr in `run.log` |

For failures after CI is enabled, download the workflow artifact bundle and re-run analysis only:

```bash
python3 rustfs/scripts/perf/analyze/analyze.py \
  --out /path/to/extracted/run \
  --baseline rustfs/scripts/perf/benchmarks/baseline.json
```
