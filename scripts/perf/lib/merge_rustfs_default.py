#!/usr/bin/env python3
"""Merge expanded rustfs.env.template output with an existing /etc/default/rustfs body.

Reads template expanded text from stdin.

Optional second CLI arg: path to a file holding the existing env file contents.
If omitted or file unreadable / empty: output is template only.

Rules:
  - Every line from the expanded template is written first, unchanged (template wins for keys).
  - Assignment lines KEY=value from the existing file are appended only when KEY does not appear
    in the template --- same key name matched case-sensitively (typical POSIX env semantics).
  - Recognises optional leading `export `. Standalone comments and blanks in the existing file are
    not preserved (simple merge).

Usage:
  ./merge_rustfs_default.py                           # stdin = template only
  ./merge_rustfs_default.py /path/to/old-snippet.txt
"""

from __future__ import annotations

import re
import sys

ASSIGN_RE = re.compile(
    r"^\s*(?:export\s+)?(?P<key>[A-Za-z_][A-Za-z0-9_]*)\s*=",
)


def assignment_keys(lines: list[str]) -> set[str]:
    keys: set[str] = set()
    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        m = ASSIGN_RE.match(raw)
        if m:
            keys.add(m.group("key"))
    return keys


def main() -> None:
    template = sys.stdin.read()
    if not template.endswith("\n"):
        template += "\n"

    tpl_lines = template.splitlines()
    template_keys = assignment_keys(tpl_lines)

    extra_path = sys.argv[1] if len(sys.argv) >= 2 else ""
    preserved: list[str] = []

    if extra_path:
        try:
            with open(extra_path, encoding="utf-8", errors="replace") as f:
                existing = f.read().splitlines()
        except OSError:
            existing = []

        seen = set()
        for raw in existing:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            m = ASSIGN_RE.match(raw)
            if not m:
                continue
            key = m.group("key")
            if key in template_keys:
                continue
            if key in seen:
                continue
            seen.add(key)
            preserved.append(raw.rstrip("\n"))

    sys.stdout.write(template)
    if preserved:
        sys.stdout.write(
            "\n# --- preserved from existing "
            "/etc/default/rustfs (keys not in perf template) ---\n"
        )
        for ln in preserved:
            sys.stdout.write(ln + "\n")


if __name__ == "__main__":
    main()
