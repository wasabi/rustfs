#!/usr/bin/env python3
"""check-pass.py — CI regression gate for a RustFS perf run.

Reads report.json produced by analyze.py, checks the regression result,
and exits 0 (pass) or 1 (fail) with a one-line message.  Designed to run
after artifacts are uploaded so the job fails with a clear message but the
full report is still available for download.

Usage:
    python3 check-pass.py <report.json>

Exit codes:
    0   pass (or no baseline available — gate skipped)
    1   regression detected
    2   usage error or report.json unreadable
"""

import json
import sys


def main() -> int:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <report.json>", file=sys.stderr)
        return 2

    path = sys.argv[1]
    try:
        with open(path) as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        print(f"ERROR: cannot read {path}: {e}", file=sys.stderr)
        return 2

    lg  = data.get("loadgen", {})
    reg = data.get("regression", {})
    meta = data.get("meta", {})

    measured  = lg.get("mean_throughput_MBs")
    passed    = reg.get("pass")
    delta_pct = reg.get("delta_pct")
    baseline  = reg.get("baseline_throughput_MBs")
    tolerance = reg.get("tolerance_pct")
    key       = reg.get("baseline_key", str(meta.get("topology", "?")))

    if passed is None:
        # No baseline for this topology — gate is skipped, not a failure
        note = reg.get("note", "no baseline available")
        print(f"SKIP: {key} — {note}")
        return 0

    if passed:
        sign = "+" if delta_pct >= 0 else ""
        print(
            f"PASS: {measured:.0f} MB/s  ({sign}{delta_pct:.1f}% vs baseline "
            f"{baseline:.0f} MB/s ± {tolerance:.0f}%)  [{key}]"
        )
        return 0
    else:
        sign = "+" if delta_pct >= 0 else ""
        print(
            f"FAIL: {measured:.0f} MB/s  ({sign}{delta_pct:.1f}% vs baseline "
            f"{baseline:.0f} MB/s ± {tolerance:.0f}%)  [{key}]",
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    sys.exit(main())
