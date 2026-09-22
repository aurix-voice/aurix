#!/usr/bin/env python3
"""Developer Certificate of Origin gate: every commit in a range carries a matching sign-off.

Usage: check_dco.py <base>..<head>

A commit passes when one of its `Signed-off-by: Name <email>` trailers matches the commit
author's name and e-mail (case-insensitive on the address). Merge commits are skipped.
"""

from __future__ import annotations

import re
import subprocess
import sys

SIGNOFF = re.compile(r"^Signed-off-by:\s*(?P<name>.+?)\s*<(?P<email>[^>]+)>\s*$", re.MULTILINE)
SEP = "\x1e"


def commits(rev_range: str) -> list[tuple[str, str, str, str]]:
    out = subprocess.run(
        ["git", "log", "--no-merges", f"--format=%H%x1f%an%x1f%ae%x1f%B{SEP}", rev_range],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    result = []
    for chunk in out.split(SEP):
        chunk = chunk.strip("\n")
        if not chunk:
            continue
        sha, name, email, body = chunk.split("\x1f", 3)
        result.append((sha, name, email, body))
    return result


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    failures = []
    checked = commits(sys.argv[1])
    for sha, name, email, body in checked:
        signoffs = [(m.group("name").strip(), m.group("email").strip()) for m in SIGNOFF.finditer(body)]
        ok = any(n == name and e.lower() == email.lower() for n, e in signoffs)
        if not ok:
            subject = body.splitlines()[0] if body else ""
            have = ", ".join(f"{n} <{e}>" for n, e in signoffs) or "none"
            failures.append(f"{sha[:12]} {subject!r}: expected 'Signed-off-by: {name} <{email}>', got {have}")
    if failures:
        print("DCO check failed — sign your commits with `git commit -s` (see CONTRIBUTING.md):")
        for line in failures:
            print("  " + line)
        return 1
    print(f"DCO ok: {len(checked)} commit(s) signed off")
    return 0


if __name__ == "__main__":
    sys.exit(main())
