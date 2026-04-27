#!/usr/bin/env python3
"""analyze.py — parse perf run artifacts and produce report.json + report.md.

Usage:
    python3 analyze.py --out /tmp/perf-results/A1 --baseline benchmarks/baseline.json
    python3 analyze.py --out /tmp/perf-results/A1 --baseline benchmarks/baseline.json --update-baseline

Inputs read from --out DIR:
    loadgen.txt             per-minute PUT stats from the load generator
    node1/mpstat-node1.txt  mpstat -P ALL output for node1
    node1/sar-net-node1.txt sar -n DEV output for node1
    node-*/mpstat-nodeN.txt same for each peer node (N = 2, 3, ...)
    node-*/sar-net-nodeN.txt
    meta.json               run metadata written by run-perf-test.sh

Outputs written to --out DIR:
    report.json             machine-readable summary + regression result
    report.md               human-readable tables + pass/fail section
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Optional


# ---------------------------------------------------------------------------
# Loadgen parser
# ---------------------------------------------------------------------------

# bytefmt suffixes → bytes-per-second multipliers → MB/s
_BYTEFMT_TO_MBS: dict[str, float] = {
    "B": 1 / (1024 * 1024),
    "K": 1 / 1024,
    "M": 1.0,
    "G": 1024.0,
    "T": 1024.0 * 1024.0,
}

def _bytefmt_to_mbs(s: str) -> float:
    """Convert a bytefmt string like '659.6M' or '1.1G' to MB/s."""
    s = s.strip()
    for suffix, mult in _BYTEFMT_TO_MBS.items():
        if s.endswith(suffix):
            return float(s[:-1]) * mult
    return float(s) / (1024 * 1024)  # bare bytes


# Match per-interval lines: #<testNum>/<loopNum>  PUT: ... <throughput>/sec, <avg> avgOpMs, <max> maxOpMs
# The %-4d format left-pads the loop number; we allow whitespace around it.
_LOADGEN_PAT = re.compile(
    r"#\d+/\s*(\d+)\s+"
    r"PUT:\s+\d+ objs,\s+\S+/obj,\s+(\S+)/sec,\s+(\d+) avgOpMs,\s+(\d+) maxOpMs"
)


def parse_loadgen(path: Path) -> list[dict]:
    """Return list of per-interval dicts with throughput_MBs, avg_op_ms, max_op_ms."""
    intervals = []
    with open(path) as f:
        for line in f:
            m = _LOADGEN_PAT.search(line)
            if m:
                loop, throughput_str, avg_ms, max_ms = m.groups()
                intervals.append(
                    {
                        "id": loop,
                        "throughput_MBs": round(_bytefmt_to_mbs(throughput_str), 1),
                        "avg_op_ms": int(avg_ms),
                        "max_op_ms": int(max_ms),
                    }
                )
    return intervals


# ---------------------------------------------------------------------------
# sar -n DEV parser
# ---------------------------------------------------------------------------

# Match data rows: optional AM/PM time, then interface name, then fields.
# sar -n DEV 1 columns: HH:MM:SS [AM|PM]  IFACE  rxpck/s  txpck/s  rxkB/s  txkB/s ...
_SAR_PAT = re.compile(
    r"^\d{2}:\d{2}:\d{2}(?:\s+[AP]M)?\s+"   # timestamp (with or without AM/PM)
    r"(\S+)\s+"                               # interface name
    r"\S+\s+\S+\s+"                           # rxpck/s  txpck/s
    r"(\S+)\s+(\S+)"                          # rxkB/s   txkB/s
)


def parse_sar_net(path: Path, ifaces: list[str]) -> Optional[dict]:
    """Parse sar -n DEV file; aggregate rxkB/s + txkB/s across ifaces; return Gbps stats."""
    if not path.exists():
        return None

    # Collect per-sample totals across all requested interfaces
    # key: sample_index → (rx_kbs_total, tx_kbs_total)
    samples: list[tuple[float, float]] = []
    # buffer per-interface per-timestamp to aggregate
    buf: dict[str, tuple[float, float]] = {}  # iface → (rx, tx) for current second
    prev_ts: str = ""

    rx_by_iface: dict[str, list[float]] = {i: [] for i in ifaces}
    tx_by_iface: dict[str, list[float]] = {i: [] for i in ifaces}

    with open(path) as f:
        for line in f:
            m = _SAR_PAT.match(line.strip())
            if not m:
                continue
            iface, rx_kbs, tx_kbs = m.groups()
            if iface in rx_by_iface:
                try:
                    rx_by_iface[iface].append(float(rx_kbs))
                    tx_by_iface[iface].append(float(tx_kbs))
                except ValueError:
                    pass

    # Require at least one interface with data
    found = [i for i in ifaces if rx_by_iface[i]]
    if not found:
        return None

    # Aggregate per sample across all found interfaces (zip to shortest)
    n = min(len(rx_by_iface[i]) for i in found)
    rx_totals = [sum(rx_by_iface[i][t] for i in found) for t in range(n)]
    tx_totals = [sum(tx_by_iface[i][t] for i in found) for t in range(n)]

    def _kbs_to_gbps(kbs: float) -> float:
        return kbs * 8 / 1_000_000  # kB/s × 8 bits × 1/1e6 = Gbit/s

    return {
        "mean_rx_Gbps": round(_kbs_to_gbps(sum(rx_totals) / n), 3),
        "peak_rx_Gbps": round(_kbs_to_gbps(max(rx_totals)), 3),
        "mean_tx_Gbps": round(_kbs_to_gbps(sum(tx_totals) / n), 3),
        "peak_tx_Gbps": round(_kbs_to_gbps(max(tx_totals)), 3),
    }


# ---------------------------------------------------------------------------
# mpstat parser
# ---------------------------------------------------------------------------

# Match "all" CPU rows: timestamp [AM|PM]  all  usr  nice  sys  iowait  irq  soft  steal  guest  gnice  idle
# idle is the last numeric field on the line.
_MPSTAT_PAT = re.compile(
    r"\ball\b"          # the word "all" somewhere on the line
    r".*?"
    r"(\d+\.\d+)\s*$"  # last float = idle%
)


def parse_mpstat(path: Path) -> Optional[dict]:
    """Parse mpstat -P ALL file; return mean active CPU % across all intervals."""
    if not path.exists():
        return None
    idle_pcts: list[float] = []
    with open(path) as f:
        for line in f:
            m = _MPSTAT_PAT.search(line.rstrip())
            if m:
                try:
                    idle_pcts.append(float(m.group(1)))
                except ValueError:
                    pass
    if not idle_pcts:
        return None
    mean_idle = sum(idle_pcts) / len(idle_pcts)
    return {"mean_active_pct": round(100.0 - mean_idle, 1)}


# ---------------------------------------------------------------------------
# Artifact discovery
# ---------------------------------------------------------------------------

def find_node_artifacts(out_dir: Path) -> dict[str, dict[str, Path]]:
    """Return {node_label: {mpstat: path, sar_net: path}} for all node dirs found."""
    nodes: dict[str, dict[str, Path]] = {}

    # node1 lives directly under out_dir/node1/
    node1_dir = out_dir / "node1"
    if node1_dir.is_dir():
        nodes["node1"] = {
            "mpstat":  node1_dir / "mpstat-node1.txt",
            "sar_net": node1_dir / "sar-net-node1.txt",
            "node_id": 1,
        }

    # peers live under out_dir/node-<hostname>/
    for d in sorted(out_dir.glob("node-*")):
        if not d.is_dir():
            continue
        # Extract node_id from the mpstat filename (mpstat-nodeN.txt)
        mpstat_files = list(d.glob("mpstat-node*.txt"))
        if not mpstat_files:
            continue
        m = re.search(r"mpstat-node(\d+)\.txt", mpstat_files[0].name)
        node_id = int(m.group(1)) if m else None
        label = d.name  # e.g. "node-node2"
        nodes[label] = {
            "mpstat":  mpstat_files[0],
            "sar_net": d / f"sar-net-node{node_id}.txt" if node_id else None,
            "node_id": node_id,
        }

    return nodes


# ---------------------------------------------------------------------------
# Regression check
# ---------------------------------------------------------------------------

def compute_regression(
    mean_throughput: float,
    baseline: dict,
    topology: str,
    variant: str,
) -> dict:
    key = f"{topology}/{variant}"
    entry = baseline.get(key)
    if entry is None or entry.get("throughput_MBs") is None:
        return {
            "baseline_key": key,
            "baseline_throughput_MBs": None,
            "delta_pct": None,
            "tolerance_pct": None,
            "pass": None,
            "note": f"No baseline for '{key}' — skipping regression gate",
        }

    base = float(entry["throughput_MBs"])
    tol = float(entry.get("tolerance_pct", 10))
    delta_pct = round((mean_throughput - base) / base * 100, 1)
    passed = delta_pct >= -tol

    return {
        "baseline_key": key,
        "baseline_throughput_MBs": base,
        "delta_pct": delta_pct,
        "tolerance_pct": tol,
        "pass": passed,
    }


# ---------------------------------------------------------------------------
# Report rendering
# ---------------------------------------------------------------------------

def render_report_md(data: dict) -> str:
    meta = data["meta"]
    lg = data["loadgen"]
    net = data["network"]
    cpu = data["cpu"]
    reg = data["regression"]

    lines = [
        f"# Perf report — {meta['variant']} / {meta['topology']} — {meta['utc']} ({meta['git_sha']})",
        "",
        "## Throughput (load generator)",
        "",
        "| Interval | MB/s | avgOpMs | maxOpMs |",
        "|----------|------|---------|---------|",
    ]
    for iv in lg["intervals"]:
        lines.append(f"| {iv['id']} | {iv['throughput_MBs']:.1f} | {iv['avg_op_ms']} | {iv['max_op_ms']} |")
    lines.append(f"| **Mean** | **{lg['mean_throughput_MBs']:.1f}** | **{lg['mean_avg_op_ms']:.0f}** | — |")
    lines.append("")

    if net:
        lines += [
            "## Network bandwidth (NIC aggregate)",
            "",
            f"Interfaces: {', '.join(meta.get('nic_interfaces', []) or ['(unknown)'])}",
            "",
            "| Node | Mean TX Gbit/s | Peak TX Gbit/s | Mean RX Gbit/s | Peak RX Gbit/s |",
            "|------|---------------|---------------|---------------|---------------|",
        ]
        for node_label, stats in sorted(net.items()):
            if stats:
                lines.append(
                    f"| {node_label} | {stats['mean_tx_Gbps']:.3f} | {stats['peak_tx_Gbps']:.3f}"
                    f" | {stats['mean_rx_Gbps']:.3f} | {stats['peak_rx_Gbps']:.3f} |"
                )
            else:
                lines.append(f"| {node_label} | — | — | — | — |")
        lines.append("")

    if cpu:
        lines += [
            "## CPU utilisation",
            "",
            "| Node | Mean active % |",
            "|------|--------------|",
        ]
        for node_label, stats in sorted(cpu.items()):
            if stats:
                lines.append(f"| {node_label} | {stats['mean_active_pct']:.1f} |")
            else:
                lines.append(f"| {node_label} | — |")
        lines.append("")

    lines.append("## Regression check")
    lines.append("")
    if reg["baseline_throughput_MBs"] is None:
        lines.append(f"Baseline: {reg.get('note', 'no baseline available — gate skipped')}")
    else:
        status = "PASS" if reg["pass"] else "FAIL"
        sign = "+" if reg["delta_pct"] >= 0 else ""
        lines.append(
            f"Baseline ({reg['baseline_key']}): {reg['baseline_throughput_MBs']:.0f} MB/s "
            f"± {reg['tolerance_pct']:.0f}%"
        )
        lines.append(
            f"Measured: {lg['mean_throughput_MBs']:.1f} MB/s  →  "
            f"Δ = {sign}{reg['delta_pct']:.1f}%  →  **{status}**"
        )
    lines.append("")

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out",      required=True,  help="Run output directory")
    parser.add_argument("--baseline", required=False, help="Path to benchmarks/baseline.json")
    parser.add_argument(
        "--update-baseline",
        action="store_true",
        help="Write measured mean back into baseline.json for this topology/variant key",
    )
    args = parser.parse_args()

    out_dir = Path(args.out)
    if not out_dir.is_dir():
        print(f"ERROR: --out directory does not exist: {out_dir}", file=sys.stderr)
        return 1

    # --- meta ---
    meta_path = out_dir / "meta.json"
    meta: dict = {}
    if meta_path.exists():
        with open(meta_path) as f:
            meta = json.load(f)
    else:
        print("WARNING: meta.json not found — report headers will be incomplete", file=sys.stderr)

    nic_interfaces: list[str] = meta.get("nic_interfaces") or []
    topology: str = meta.get("topology", "unknown")
    variant: str = meta.get("variant", "unknown")

    # --- loadgen ---
    loadgen_path = out_dir / "loadgen.txt"
    if not loadgen_path.exists():
        print(f"ERROR: loadgen.txt not found in {out_dir}", file=sys.stderr)
        return 1

    intervals = parse_loadgen(loadgen_path)
    if not intervals:
        print("WARNING: no per-interval lines found in loadgen.txt", file=sys.stderr)

    mean_throughput = (
        sum(iv["throughput_MBs"] for iv in intervals) / len(intervals) if intervals else 0.0
    )
    mean_avg_op_ms = (
        sum(iv["avg_op_ms"] for iv in intervals) / len(intervals) if intervals else 0.0
    )

    loadgen_data = {
        "intervals": intervals,
        "mean_throughput_MBs": round(mean_throughput, 1),
        "mean_avg_op_ms": round(mean_avg_op_ms, 1),
    }

    # --- per-node telemetry ---
    node_artifacts = find_node_artifacts(out_dir)

    network: dict = {}
    cpu: dict = {}

    for label, paths in node_artifacts.items():
        sar_path = paths.get("sar_net")
        mp_path  = paths.get("mpstat")

        if sar_path and Path(sar_path).exists() and nic_interfaces:
            network[label] = parse_sar_net(Path(sar_path), nic_interfaces)
        else:
            network[label] = None

        if mp_path and Path(mp_path).exists():
            cpu[label] = parse_mpstat(Path(mp_path))
        else:
            cpu[label] = None

    # --- baseline + regression ---
    baseline: dict = {}
    baseline_path = Path(args.baseline) if args.baseline else None
    if baseline_path and baseline_path.exists():
        with open(baseline_path) as f:
            baseline = json.load(f)
    elif args.baseline:
        print(f"WARNING: baseline file not found: {args.baseline}", file=sys.stderr)

    regression = compute_regression(mean_throughput, baseline, topology, variant)

    # --- assemble report.json ---
    report = {
        "meta": {
            "git_sha":        meta.get("git_sha", "unknown"),
            "variant":        variant,
            "topology":       topology,
            "duration":       meta.get("duration", "unknown"),
            "utc":            meta.get("utc", "unknown"),
            "nic_interfaces": nic_interfaces,
        },
        "loadgen":    loadgen_data,
        "network":    network,
        "cpu":        cpu,
        "regression": regression,
    }

    report_json_path = out_dir / "report.json"
    with open(report_json_path, "w") as f:
        json.dump(report, f, indent=2)
    print(f"Written: {report_json_path}")

    # --- assemble report.md ---
    report_md = render_report_md(report)
    report_md_path = out_dir / "report.md"
    with open(report_md_path, "w") as f:
        f.write(report_md)
    print(f"Written: {report_md_path}")

    # --- update-baseline ---
    if args.update_baseline and baseline_path:
        key = f"{topology}/{variant}"
        if key not in baseline:
            baseline[key] = {}
        old_val = baseline[key].get("throughput_MBs")
        baseline[key]["throughput_MBs"] = round(mean_throughput, 1)
        baseline[key]["avg_op_ms"] = round(mean_avg_op_ms, 1)
        if "tolerance_pct" not in baseline[key]:
            baseline[key]["tolerance_pct"] = 10
        baseline[key]["source"] = f"updated by analyze.py from {out_dir.name} ({meta.get('utc', 'unknown')})"
        with open(baseline_path, "w") as f:
            json.dump(baseline, f, indent=2)
        print(f"Baseline updated: {key}: {old_val} → {mean_throughput:.1f} MB/s")

    # Exit non-zero if regression gate fired (pass=False, not None)
    if regression["pass"] is False:
        print(
            f"REGRESSION: {regression['delta_pct']:+.1f}% vs baseline "
            f"({regression['baseline_throughput_MBs']:.0f} MB/s ± {regression['tolerance_pct']:.0f}%)",
            file=sys.stderr,
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
