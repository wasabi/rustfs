#!/usr/bin/env python3
"""analyze.py — parse perf run artifacts and produce report.json + report.md.

Usage:
    python3 analyze.py --out /tmp/perf-results/run1 --baseline benchmarks/baseline.json
    python3 analyze.py --out /tmp/perf-results/run1 --baseline benchmarks/baseline.json --update-baseline

Inputs read from --out DIR:
    loadgen.txt             per-minute PUT stats from the load generator
    node1/mpstat-node1.txt  mpstat -P ALL output for node1
    node1/sar-net-node1.txt sar -n DEV output for node1 (canonical active window when present)
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
import math
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

# When only the summary row is present (#<test>/AVG PUT: …), e.g. loadgen stdout was
# block-buffered to tee until SIGTERM dropped earlier interval rows from loadgen.txt.
_LOADGEN_SUMMARY_AVG_PAT = re.compile(
    r"#\d+/\s*AVG\s+"
    r"PUT:\s+\d+ objs,\s+\S+/obj,\s+(\S+)/sec,\s+(\d+) avgOpMs,\s+(\d+) maxOpMs"
)


def parse_loadgen(path: Path) -> list[dict]:
    """Return list of per-interval dicts with throughput_MBs, avg_op_ms, max_op_ms."""
    raw = path.read_text(errors="replace")
    intervals: list[dict] = []
    for line in raw.splitlines():
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
    if intervals:
        return intervals
    # Fallback: one aggregate row printed after PUT traffic (may be the only line left on disk).
    for line in raw.splitlines():
        sm = _LOADGEN_SUMMARY_AVG_PAT.search(line)
        if sm:
            throughput_str, avg_ms, max_ms = sm.groups()
            intervals.append(
                {
                    "id": "AVG",
                    "throughput_MBs": round(_bytefmt_to_mbs(throughput_str), 1),
                    "avg_op_ms": int(avg_ms),
                    "max_op_ms": int(max_ms),
                }
            )
            break
    return intervals


# ---------------------------------------------------------------------------
# Active-window trimming
# ---------------------------------------------------------------------------

def _active_slice(combined: list[float]) -> tuple[int, int]:
    """Return (start, end) indices of the active PUT window.

    Trims leading and trailing samples where the combined signal is below 10%
    of its peak value.  This removes the idle setup phase (bucket creation,
    connection warm-up) that precedes actual PUT traffic and would otherwise
    dilute per-second means for NIC, CPU, and disk metrics.

    If the signal is all-zero or all-equal the full range is returned unchanged.
    """
    if not combined:
        return 0, 0
    peak = max(combined)
    if peak == 0:
        return 0, len(combined)
    threshold = peak * 0.10
    active = [i for i, v in enumerate(combined) if v >= threshold]
    if not active:
        return 0, len(combined)
    return active[0], active[-1] + 1


# Canonical fractional window [start_frac, end_frac) mapped from SAR sample
# indices via start_frac=start/n, end_frac=end/n (end exclusive).


def _active_slice_fracs(combined: list[float]) -> tuple[float, float]:
    """Return fractional [sf, ef) corresponding to `_active_slice` on `combined`."""
    n = len(combined)
    if not n:
        return 0.0, 1.0
    start, end = _active_slice(combined)
    return start / n, end / n


def _indices_from_window_fracs(n: int, window_fracs: tuple[float, float]) -> tuple[int, int]:
    """Map canonical fractional window [sf, ef) onto a series of length n (exclusive end index)."""
    if n <= 0:
        return 0, 0
    sf, ef = window_fracs
    sf = max(0.0, min(1.0, float(sf)))
    ef = max(0.0, min(1.0, float(ef)))
    if ef <= sf:
        return 0, n
    lo = min(n, int(math.floor(sf * n + 1e-12)))
    hi = max(lo, min(n, int(math.ceil(ef * n - 1e-12))))
    return lo, hi


# Match data rows: optional AM/PM time, then interface name, then fields.
# sar -n DEV 1 columns: HH:MM:SS [AM|PM]  IFACE  rxpck/s  txpck/s  rxkB/s  txkB/s ...
_SAR_PAT = re.compile(
    r"^\d{2}:\d{2}:\d{2}(?:\s+[AP]M)?\s+"   # timestamp (with or without AM/PM)
    r"(\S+)\s+"                               # interface name
    r"\S+\s+\S+\s+"                           # rxpck/s  txpck/s
    r"(\S+)\s+(\S+)"                          # rxkB/s   txkB/s
)


def _sar_physical_combined_kbs(rx_by_iface: dict[str, list[float]],
                               tx_by_iface: dict[str, list[float]],
                               found: list[str]) -> tuple[list[float], int]:
    """Build per-tick combined RX+TX kB/s on physical NICs (exclude lo); return (combined, n)."""
    if not found:
        return [], 0
    n_len = min(len(rx_by_iface[i]) for i in found)
    trim_ifaces = [i for i in found if i != "lo"]
    if not trim_ifaces:
        trim_ifaces = list(found)
    combined = [
        sum(rx_by_iface[i][t] + tx_by_iface[i][t] for i in trim_ifaces)
        for t in range(n_len)
    ]
    return combined, n_len


def sar_window_fracs_from_file(path: Path, ifaces: list[str]) -> Optional[tuple[float, float]]:
    """Canonical [sf, ef) from node1 SAR: physical NICs combined RX+TX, `_active_slice` trim.

    Returns None if file missing or no usable SAR rows.
    """
    if not path.exists():
        return None
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

    found = [i for i in ifaces if rx_by_iface[i]]
    if not found:
        return None

    combined, _n = _sar_physical_combined_kbs(rx_by_iface, tx_by_iface, found)
    if not combined:
        return None
    return _active_slice_fracs(combined)


def _sar_iface_is_active(
    iface: str,
    mean_rx_gbps: float,
    peak_rx_gbps: float,
    mean_tx_gbps: float,
    peak_tx_gbps: float,
) -> bool:
    """Exclude interfaces with negligible RX/TX vs noise / idle rows (same rules for lo and NICs)."""
    # Sar output often prints all-zero lines for unused NICs rounded to 0.000.
    min_gbps = 0.002
    if max(mean_rx_gbps, peak_rx_gbps, mean_tx_gbps, peak_tx_gbps) >= min_gbps:
        return True
    return False


def parse_sar_net(
    path: Path,
    ifaces: list[str],
    window_fracs: Optional[tuple[float, float]] = None,
) -> Optional[dict]:
    """Parse sar -n DEV file; return per-interface Gbps stats.

    Returns {iface: {mean_rx_Gbps, peak_rx_Gbps, mean_tx_Gbps, peak_tx_Gbps}}
    for each requested interface that has data.

    Omits idle interfaces (negligible RX/TX, including inactive lo).

    When ``window_fracs`` is set (from node1 SAR), all interfaces use that
    fractional ``[sf, ef)`` mapped to this file's sample length so NIC, CPU,
    and disk summaries align.  When None, trim uses combined TX+RX on physical
    NICs only; ``lo`` additionally uses its own ``_active_slice`` on loopback
    RX+TX (localhost can be bursty vs the physical-NIC window).
    """
    if not path.exists():
        return None

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

    found = [i for i in ifaces if rx_by_iface[i]]
    if not found:
        return None

    n = min(len(rx_by_iface[i]) for i in found)
    if window_fracs is not None:
        start, end = _indices_from_window_fracs(n, window_fracs)
    else:
        trim_ifaces = [i for i in found if i != "lo"]
        if not trim_ifaces:
            trim_ifaces = list(found)
        combined = [
            sum(rx_by_iface[i][t] + tx_by_iface[i][t] for i in trim_ifaces)
            for t in range(n)
        ]
        start, end = _active_slice(combined)

    def _kbs_to_gbps(kbs: float) -> float:
        return kbs * 8 / 1_000_000  # kB/s × 8 bits × 1/1e6 = Gbit/s

    result = {}
    for iface in found:
        rx_full = rx_by_iface[iface][:n]
        tx_full = tx_by_iface[iface][:n]
        if window_fracs is None and iface == "lo":
            lo_combo = [rx_full[t] + tx_full[t] for t in range(n)]
            lo_start, lo_end = _active_slice(lo_combo)
            rx = rx_full[lo_start:lo_end]
            tx = tx_full[lo_start:lo_end]
        else:
            rx = rx_full[start:end]
            tx = tx_full[start:end]
        if not rx:
            continue
        n_active = len(rx)
        mean_rx_g = _kbs_to_gbps(sum(rx) / n_active)
        peak_rx_g = _kbs_to_gbps(max(rx))
        mean_tx_g = _kbs_to_gbps(sum(tx) / n_active)
        peak_tx_g = _kbs_to_gbps(max(tx))
        if not _sar_iface_is_active(iface, mean_rx_g, peak_rx_g, mean_tx_g, peak_tx_g):
            continue
        result[iface] = {
            "mean_rx_Gbps": round(mean_rx_g, 3),
            "peak_rx_Gbps": round(peak_rx_g, 3),
            "mean_tx_Gbps": round(mean_tx_g, 3),
            "peak_tx_Gbps": round(peak_tx_g, 3),
        }
    return result or None


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


def parse_mpstat(path: Path, window_fracs: Optional[tuple[float, float]] = None) -> Optional[dict]:
    """Parse mpstat -P ALL file; return mean active CPU % across all intervals.

    When ``window_fracs`` is set, trim idle rows with the same fractional window
    as SAR (node1).  Otherwise trim using active CPU% (_active_slice).
    """
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
    n = len(idle_pcts)
    if window_fracs is not None:
        start, end = _indices_from_window_fracs(n, window_fracs)
        trimmed = idle_pcts[start:end] or idle_pcts
    else:
        active_pcts = [100.0 - v for v in idle_pcts]
        start, end = _active_slice(active_pcts)
        trimmed = idle_pcts[start:end] or idle_pcts
    mean_idle = sum(trimmed) / len(trimmed)
    return {"mean_active_pct": round(100.0 - mean_idle, 1)}


# ---------------------------------------------------------------------------
# iostat parser
# ---------------------------------------------------------------------------

def _iostat_device_is_active(
    device: str,
    mean_w: float,
    peak_w: float,
    mean_u: float,
    peak_u: float,
) -> bool:
    """Exclude loop/ram disks and rows with no meaningful write or util."""
    # Pseudo-block devices typical on Linux hosts — not RustFS data disks.
    if device.startswith(("loop", "ram", "zram")):
        return False
    # Noise floor: rounded-to-zero churn from iostat floats.
    min_write_mbs = 0.05
    min_util_pct = 0.15
    if max(mean_w, peak_w) >= min_write_mbs:
        return True
    if max(mean_u, peak_u) >= min_util_pct:
        return True
    return False


def _iostat_batch_combined_write_mb_s(batch: dict[str, tuple[float, float]]) -> float:
    """Sum write MB/s for real block devices (exclude loop/ram/zram noise in trim signal)."""
    return sum(
        w for dev, (w, _) in batch.items()
        if not dev.startswith(("loop", "ram", "zram"))
    )


def _iostat_read_batches(path: Path) -> list[dict[str, tuple[float, float]]]:
    """Parse iostat -xz into one dict per reporting interval (between avg-cpu markers).

    iostat omits idle loop devices in later intervals; naively glob-appending device rows
    misaligns timelines and makes min(per-device counts)==1 when loop* only appear once,
    destroying combined_write and reported means/peaks.
    """
    batches: list[dict[str, tuple[float, float]]] = []
    current: dict[str, tuple[float, float]] = {}
    write_col: Optional[int] = None
    write_scale = 1.0
    util_col: Optional[int] = None

    with open(path) as f:
        for raw in f:
            line = raw.strip()
            if line.startswith("avg-cpu"):
                if write_col is not None:
                    batches.append(dict(current))
                current = {}
                continue

            cols = line.split()
            if "%util" in line and ("wMB/s" in cols or "wkB/s" in cols):
                if "wMB/s" in cols:
                    write_col = cols.index("wMB/s")
                    write_scale = 1.0
                elif "wkB/s" in cols:
                    write_col = cols.index("wkB/s")
                    write_scale = 1.0 / 1024.0
                util_col = cols.index("%util")
                current = {}
                continue

            if write_col is None or util_col is None:
                continue
            if not line:
                continue

            parts = line.split()
            if len(parts) <= max(write_col, util_col):
                continue
            device = parts[0]
            if not device or device.startswith("%") or device in ("Device", "Device:"):
                continue
            try:
                write_mbs = float(parts[write_col]) * write_scale
                util_pct = float(parts[util_col])
            except (ValueError, IndexError):
                continue
            current[device] = (write_mbs, util_pct)

    if write_col is not None and current:
        batches.append(dict(current))
    return batches


def parse_iostat(path: Path, window_fracs: Optional[tuple[float, float]] = None) -> Optional[dict]:
    """Parse iostat -xz output; return per-device write MB/s and %util summary.

    Intervals are aligned using avg-cpu / Device-section boundaries so short-lived rows
    (e.g. loop devices in the first snapshot only) cannot collapse the timeline.

    Handles both older (wkB/s) and newer (wMB/s) iostat column formats.
    Drops idle/non-storage rows (loop/ram/zram and negligible write/util) so
    reports stay readable — when ``window_fracs`` is None, active-window trimming
    uses combined write MB/s per interval; when set, the same fractional window
    as SAR (node1) is applied.

    Returns a dict with:
        devices: list of {device, mean_write_MBs, peak_write_MBs, mean_util_pct, peak_util_pct}
        total_mean_write_MBs: sum of mean_write_MBs across listed (active) devices
        total_peak_write_MBs: sum of peak_write_MBs across listed (active) devices
    """
    if not path.exists():
        return None

    batches = _iostat_read_batches(path)
    if not batches:
        return None

    nb = len(batches)

    combined_write = [_iostat_batch_combined_write_mb_s(batches[t]) for t in range(nb)]

    if window_fracs is not None:
        start, end = _indices_from_window_fracs(nb, window_fracs)
    else:
        start, end = _active_slice(combined_write)

    all_devs: set[str] = set()
    for t in range(start, end):
        all_devs.update(batches[t].keys())

    devices: list[dict] = []
    total_mean_write = 0.0
    total_peak_write = 0.0

    for dev in sorted(all_devs):
        writes: list[float] = []
        utils: list[float] = []
        for t in range(start, end):
            if dev in batches[t]:
                w, u = batches[t][dev]
                writes.append(w)
                utils.append(u)
        if not writes:
            continue
        mean_w = sum(writes) / len(writes)
        peak_w = max(writes)
        mean_u = sum(utils) / len(utils)
        peak_u = max(utils)
        if not _iostat_device_is_active(dev, mean_w, peak_w, mean_u, peak_u):
            continue
        total_mean_write += mean_w
        total_peak_write += peak_w
        devices.append({
            "device":           dev,
            "mean_write_MBs":   round(mean_w, 1),
            "peak_write_MBs":   round(peak_w, 1),
            "mean_util_pct":    round(mean_u, 1),
            "peak_util_pct":    round(peak_u, 1),
        })

    # All devices were filtered (e.g. only loopback devices) — no useful table rows.
    if not devices:
        return None

    return {
        "devices":               devices,
        "total_mean_write_MBs":  round(total_mean_write, 1),
        "total_peak_write_MBs":  round(total_peak_write, 1),
    }


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
            "iostat":  node1_dir / "iostat-node1.txt",
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
        label = d.name[5:] if d.name.startswith("node-") else d.name
        nodes[label] = {
            "mpstat":  mpstat_files[0],
            "sar_net": d / f"sar-net-node{node_id}.txt" if node_id else None,
            "iostat":  d / f"iostat-node{node_id}.txt" if node_id else None,
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
) -> dict:
    key = topology
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
# Markdown table helpers
# ---------------------------------------------------------------------------

def _md_table(headers: list[str], rows: list[list[str]]) -> list[str]:
    """Return markdown table lines with columns padded for alignment."""
    all_rows = [headers] + rows
    widths = [max(len(cell) for cell in col) for col in zip(*all_rows)]
    def fmt_row(cells: list[str]) -> str:
        return "| " + " | ".join(c.ljust(w) for c, w in zip(cells, widths)) + " |"
    sep = "|" + "|".join("-" * (w + 2) for w in widths) + "|"
    return [fmt_row(headers), sep] + [fmt_row(r) for r in rows]


# ---------------------------------------------------------------------------
# Report rendering
# ---------------------------------------------------------------------------

def render_report_md(data: dict) -> str:
    meta = data["meta"]
    lg = data["loadgen"]
    net = data["network"]
    cpu = data["cpu"]
    dsk = data.get("disk", {})
    reg = data["regression"]

    lines = [
        f"# Perf report — {meta['topology']} — {meta['utc']} ({meta['git_sha']})",
        "",
        "## Throughput (load generator)",
        "",
    ]
    tput_rows = [
        [iv['id'], f"{iv['throughput_MBs']:.1f}", str(iv['avg_op_ms']), str(iv['max_op_ms'])]
        for iv in lg["intervals"]
    ]
    tput_rows.append([
        "**Mean**",
        f"**{lg['mean_throughput_MBs']:.1f}**",
        f"**{lg['mean_avg_op_ms']:.0f}**",
        "—",
    ])
    lines += _md_table(["Interval", "MB/s", "avgOpMs", "maxOpMs"], tput_rows)
    lines.append("")

    if net:
        lines += ["## Network bandwidth (per interface)", ""]
        net_rows = []
        for node_label, iface_stats in sorted(net.items()):
            if not iface_stats:
                net_rows.append([node_label, "—", "—", "—", "—", "—"])
                continue
            for iface, stats in sorted(iface_stats.items()):
                net_rows.append([
                    node_label,
                    iface,
                    f"{stats['mean_tx_Gbps']:.3f}",
                    f"{stats['peak_tx_Gbps']:.3f}",
                    f"{stats['mean_rx_Gbps']:.3f}",
                    f"{stats['peak_rx_Gbps']:.3f}",
                ])
        if net_rows:
            lines += _md_table(
                ["Node", "Interface", "Mean TX Gbit/s", "Peak TX Gbit/s", "Mean RX Gbit/s", "Peak RX Gbit/s"],
                net_rows,
            )
        lines.append("")

    if cpu:
        lines += ["## CPU utilisation", ""]
        cpu_rows = []
        for node_label, stats in sorted(cpu.items()):
            cpu_rows.append([node_label, f"{stats['mean_active_pct']:.1f}" if stats else "—"])
        lines += _md_table(["Node", "Mean active %"], cpu_rows)
        lines.append("")

    if dsk:
        lines += ["## Disk I/O (write)", ""]
        disk_rows = []
        for node_label, stats in sorted(dsk.items()):
            if not stats:
                disk_rows.append([node_label, "—", "—", "—", "—", "—", "—"])
                continue
            for dev in stats["devices"]:
                disk_rows.append([
                    node_label,
                    dev["device"],
                    f"{dev['mean_write_MBs']:.1f}",
                    f"{dev['peak_write_MBs']:.1f}",
                    f"{dev['mean_util_pct']:.1f}",
                    f"{dev['peak_util_pct']:.1f}",
                ])
        if disk_rows:
            lines += _md_table(
                ["Node", "Device", "Mean Write MB/s", "Peak Write MB/s", "Mean %util", "Peak %util"],
                disk_rows,
            )
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
        help="Write measured mean back into baseline.json for this topology key",
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
    # sar -n DEV records every iface (incl. lo); meta lists physical NIC names only —
    # merge loopback so reports can show localhost traffic beside eno*.
    ifaces_for_sar = list(dict.fromkeys([*nic_interfaces, "lo"]))
    topology: str = meta.get("topology", "unknown")

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

    # One fractional [sf, ef) from node1 physical-NIC SAR trim; apply to all nodes' SAR/mpstat/iostat.
    window_fracs: Optional[tuple[float, float]] = None
    n1 = node_artifacts.get("node1", {})
    sar_node1 = n1.get("sar_net")
    if sar_node1 and Path(sar_node1).exists() and ifaces_for_sar:
        window_fracs = sar_window_fracs_from_file(Path(sar_node1), ifaces_for_sar)

    network: dict = {}
    cpu: dict = {}
    disk: dict = {}

    for label, paths in node_artifacts.items():
        sar_path    = paths.get("sar_net")
        mp_path     = paths.get("mpstat")
        iostat_path = paths.get("iostat")

        if sar_path and Path(sar_path).exists() and ifaces_for_sar:
            network[label] = parse_sar_net(Path(sar_path), ifaces_for_sar, window_fracs)
        else:
            network[label] = None

        if mp_path and Path(mp_path).exists():
            cpu[label] = parse_mpstat(Path(mp_path), window_fracs)
        else:
            cpu[label] = None

        if iostat_path and Path(iostat_path).exists():
            disk[label] = parse_iostat(Path(iostat_path), window_fracs)
        else:
            disk[label] = None

    # --- baseline + regression ---
    baseline: dict = {}
    baseline_path = Path(args.baseline) if args.baseline else None
    if baseline_path and baseline_path.exists():
        with open(baseline_path) as f:
            baseline = json.load(f)
    elif args.baseline:
        print(f"WARNING: baseline file not found: {args.baseline}", file=sys.stderr)

    regression = compute_regression(mean_throughput, baseline, topology)

    # --- assemble report.json ---
    report = {
        "meta": {
            "git_sha":        meta.get("git_sha", "unknown"),
            "topology":       topology,
            "duration":       meta.get("duration", "unknown"),
            "utc":            meta.get("utc", "unknown"),
            "nic_interfaces": nic_interfaces,
        },
        "loadgen":    loadgen_data,
        "network":    network,
        "cpu":        cpu,
        "disk":       disk,
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
        key = topology
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
