#!/usr/bin/env python3
"""
Parse RustFS obs JSONL (span close lines) into CSV and optional HTML Gantt charts.

Handles rustfs_put_trace span closes with optional trace_id / bucket / object fields.
Can also process rustfs_lock_rpc closes from peer nodes when passed as additional inputs.

Usage:
  python3 obs_timing_from_jsonl.py \\
    --input node1:/path/to/node1-rustfs.log \\
    --csv timing.csv \\
    --html timing_gantt.html \\
    --html-max-traces 50 \\
    --html-span-contains get_write_lock

  # Multi-node (manual deep-dive):
  python3 obs_timing_from_jsonl.py \\
    --input node1:/path/to/node1-rustfs.log \\
    --input node2:/path/to/node2-rustfs.log \\
    --csv timing.csv \\
    --html timing_gantt.html

If --html is omitted, only --csv (or stdout) is produced. Durations in CSV are in seconds.
"""

from __future__ import annotations

import argparse
import csv
import html
import json
import re
import sys
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable


_DURATION_RE = re.compile(
    r"^\s*([\d.]+)\s*(µs|us|μs|ms|s|ns)\s*$",
    re.IGNORECASE,
)


def parse_duration_seconds(s: str) -> float:
    """Parse tracing-style duration strings (e.g. 103ms, 750µs, 1.2s) to seconds."""
    s = s.strip()
    if not s:
        return 0.0
    m = _DURATION_RE.match(s)
    if not m:
        # fallback: plain number as ms
        try:
            return float(s) / 1000.0
        except ValueError:
            return 0.0
    val = float(m.group(1))
    unit = m.group(2).lower()
    if unit in ("ns",):
        return val / 1e9
    if unit in ("µs", "μs", "us"):
        return val / 1e6
    if unit == "ms":
        return val / 1000.0
    if unit == "s":
        return val
    return 0.0


def parse_ts(iso: str) -> datetime:
    """Parse Rust / RFC3339 timestamps; trim sub-microsecond digits for Python fromisoformat."""
    # e.g. 2026-04-18T01:24:08.129624712Z (9 fractional digits)
    s = iso.strip()
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    if "." in s:
        head, rest = s.split(".", 1)
        tz = ""
        for sep in ("+", "-"):
            idx = rest.rfind(sep)
            if idx > 0:
                frac, tz = rest[:idx], rest[idx:]
                break
        else:
            frac, tz = rest, ""
        frac_us = (frac + "000000")[:6]
        s = f"{head}.{frac_us}{tz}"
    return datetime.fromisoformat(s)


@dataclass
class CloseRow:
    source_label: str
    timestamp: datetime
    span_name: str
    target: str
    busy_s: float
    idle_s: float
    trace_id: str
    bucket: str
    object: str
    resource: str
    raw: dict[str, Any]


def iter_closes(path: Path, label: str) -> Iterable[CloseRow]:
    with path.open("r", encoding="utf-8", errors="replace") as f:
        for line_no, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            fields = rec.get("fields") or {}
            if fields.get("message") != "close":
                continue
            span = rec.get("span") or {}
            name = span.get("name") or ""
            busy_s = parse_duration_seconds(str(fields.get("time.busy", "0")))
            idle_s = parse_duration_seconds(str(fields.get("time.idle", "0")))
            ts_raw = rec.get("timestamp") or ""
            try:
                ts = parse_ts(ts_raw)
            except (ValueError, TypeError):
                continue
            yield CloseRow(
                source_label=label,
                timestamp=ts,
                span_name=name,
                target=str(rec.get("target") or ""),
                busy_s=busy_s,
                idle_s=idle_s,
                trace_id=str(span.get("trace_id") or ""),
                bucket=str(span.get("bucket") or ""),
                object=str(span.get("object") or ""),
                resource=str(span.get("resource") or ""),
                raw=rec,
            )


def write_csv(rows: list[CloseRow], out: Path | None) -> None:
    fieldnames = [
        "source",
        "timestamp",
        "span_name",
        "target",
        "busy_s",
        "idle_s",
        "wall_s",
        "trace_id",
        "bucket",
        "object",
        "resource",
    ]
    wall = lambda r: r.busy_s + r.idle_s
    if out:
        f = out.open("w", newline="", encoding="utf-8")
    else:
        f = sys.stdout
    try:
        w = csv.DictWriter(f, fieldnames=fieldnames)
        w.writeheader()
        for r in rows:
            w.writerow(
                {
                    "source": r.source_label,
                    "timestamp": r.timestamp.isoformat(),
                    "span_name": r.span_name,
                    "target": r.target,
                    "busy_s": f"{r.busy_s:.9f}",
                    "idle_s": f"{r.idle_s:.9f}",
                    "wall_s": f"{wall(r):.9f}",
                    "trace_id": r.trace_id,
                    "bucket": r.bucket,
                    "object": r.object,
                    "resource": r.resource,
                }
            )
    finally:
        if out:
            f.close()


def aggregate_by_span(rows: list[CloseRow]) -> list[tuple[str, int, float, float, float]]:
    """Return (span_name, count, mean_wall_s, p50_wall_s, p90_wall_s) sorted by mean desc."""
    by_span: dict[str, list[float]] = defaultdict(list)
    wall = lambda r: r.busy_s + r.idle_s
    for r in rows:
        by_span[r.span_name].append(wall(r))

    def pct(xs: list[float], p: float) -> float:
        if not xs:
            return 0.0
        ys = sorted(xs)
        k = min(len(ys) - 1, int(p * (len(ys) - 1)))
        return ys[k]

    out = []
    for name, xs in by_span.items():
        n = len(xs)
        mean = sum(xs) / n
        out.append((name, n, mean, pct(xs, 0.5), pct(xs, 0.9)))
    out.sort(key=lambda t: -t[2])
    return out


def write_html_gantt(
    rows: list[CloseRow],
    out: Path,
    max_traces: int,
    span_filter: str | None,
) -> None:
    """One section per trace_id with horizontal bars (inferred start = ts - wall)."""
    wall = lambda r: r.busy_s + r.idle_s
    by_tid: dict[str, list[CloseRow]] = defaultdict(list)
    for r in rows:
        tid = r.trace_id.strip()
        if not tid:
            continue
        if span_filter and span_filter not in r.span_name:
            continue
        by_tid[tid].append(r)

    # longest wall first as proxy for "interesting" traces
    scored = sorted(by_tid.items(), key=lambda kv: -max(wall(x) for x in kv[1]))
    scored = scored[:max_traces]

    parts: list[str] = []
    parts.append("<!DOCTYPE html><html><head><meta charset='utf-8'><title>Obs timing Gantt</title>")
    parts.append(
        "<style>"
        "body{font-family:system-ui,sans-serif;margin:16px;}"
        "table{border-collapse:collapse;width:100%;margin-bottom:32px;}"
        "td,th{border:1px solid #ccc;padding:4px 8px;font-size:12px;}"
        ".lane{position:relative;height:22px;background:#f4f4f4;margin:2px 0;}"
        ".bar{position:absolute;top:2px;height:18px;border-radius:3px;opacity:0.9;}"
        ".legend span{margin-right:12px;}"
        "</style></head><body>"
    )
    parts.append("<h1>Obs span timing (close-only inference)</h1>")
    parts.append(
        "<p>Each bar: <code>end = timestamp</code>, <code>start = end - (busy+idle)</code> from span close. "
        "Parent/child ordering within a trace is not guaranteed in HTML output.</p>"
    )

    colors = ["#1f77b4", "#ff7f0e", "#2ca02c", "#d62728", "#9467bd", "#8c564b", "#e377c2"]

    for tid, evs in scored:
        evs = sorted(evs, key=lambda r: r.timestamp)
        t0 = min(r.timestamp for r in evs)
        t1 = max(r.timestamp for r in evs)
        span_sec = max((t1 - t0).total_seconds(), 1e-6)

        parts.append(f"<h2 id='{html.escape(tid, quote=True)}'>trace_id: {html.escape(tid)}</h2>")
        parts.append("<table><tr><th>source</th><th>span</th><th>end (UTC)</th><th>wall ms</th><th>busy ms</th><th>idle ms</th></tr>")
        for r in evs:
            w = wall(r) * 1000
            parts.append(
                "<tr>"
                f"<td>{html.escape(r.source_label)}</td>"
                f"<td>{html.escape(r.span_name)}</td>"
                f"<td>{html.escape(r.timestamp.isoformat())}</td>"
                f"<td>{w:.3f}</td>"
                f"<td>{r.busy_s*1000:.3f}</td>"
                f"<td>{r.idle_s*1000:.3f}</td>"
                "</tr>"
            )
        parts.append("</table>")

        parts.append("<div class='lane' style='min-width:640px;'>")
        for i, r in enumerate(evs):
            end = (r.timestamp - t0).total_seconds()
            w = wall(r)
            start = end - w
            start = max(0.0, start)
            left_pct = 100.0 * start / span_sec
            width_pct = max(100.0 * w / span_sec, 0.2)
            color = colors[i % len(colors)]
            title = f"{r.source_label} {r.span_name} wall={w*1000:.2f}ms"
            parts.append(
                f"<div class='bar' style='left:{left_pct:.2f}%;width:{width_pct:.2f}%;background:{color};' "
                f"title=\"{html.escape(title, quote=True)}\"></div>"
            )
        parts.append("</div>")
        parts.append("<p class='legend'>")
        for i, r in enumerate(evs):
            color = colors[i % len(colors)]
            parts.append(f"<span style='color:{color}'>&#9632;</span> {html.escape(r.span_name)} ")
        parts.append("</p>")

    parts.append("<h2>Aggregate mean wall (s) by span name</h2><table><tr><th>span</th><th>n</th><th>mean</th><th>p50</th><th>p90</th></tr>")
    for name, n, mean, p50, p90 in aggregate_by_span(rows):
        parts.append(
            f"<tr><td>{html.escape(name)}</td><td>{n}</td><td>{mean:.6f}</td><td>{p50:.6f}</td><td>{p90:.6f}</td></tr>"
        )
    parts.append("</table></body></html>")

    out.write_text("".join(parts), encoding="utf-8")


def main() -> int:
    ap = argparse.ArgumentParser(description="RustFS obs JSONL -> CSV / HTML Gantt")
    ap.add_argument(
        "--input",
        action="append",
        metavar="LABEL:PATH",
        help="Labeled log file (repeatable), e.g. node1:./node1-rustfs.log",
    )
    ap.add_argument("--csv", type=Path, help="Write flattened close rows to CSV")
    ap.add_argument("--html", type=Path, help="Write HTML Gantt for traces with trace_id")
    ap.add_argument(
        "--html-max-traces",
        type=int,
        default=40,
        help="Max number of distinct trace_ids in HTML (default 40)",
    )
    ap.add_argument(
        "--html-span-contains",
        default="",
        help="If set, only include rows whose span name contains this substring in HTML trace groups",
    )
    args = ap.parse_args()

    if not args.input:
        ap.error("at least one --input LABEL:PATH is required")

    rows: list[CloseRow] = []
    for spec in args.input:
        if ":" not in spec:
            ap.error(f"invalid --input (need LABEL:PATH): {spec}")
        label, path_s = spec.split(":", 1)
        path = Path(path_s)
        if not path.is_file():
            print(f"warning: skip missing file: {path}", file=sys.stderr)
            continue
        rows.extend(iter_closes(path, label))

    rows.sort(key=lambda r: r.timestamp)

    if args.csv:
        write_csv(rows, args.csv)
    else:
        if not args.html:
            write_csv(rows, None)

    if args.html:
        write_html_gantt(
            rows,
            args.html,
            max_traces=args.html_max_traces,
            span_filter=args.html_span_contains or None,
        )

    print(f"processed {len(rows)} close rows", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
