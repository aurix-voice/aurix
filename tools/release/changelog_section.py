#!/usr/bin/env python3
"""Print the CHANGELOG.md section for one version (release-notes body).

    python3 tools/release/changelog_section.py 1.3.0
    python3 tools/release/changelog_section.py --tag v1.3.0-rc.1   # falls back to Unreleased
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def section(text: str, heading: str) -> str | None:
    match = re.search(rf"^## \[{re.escape(heading)}\][^\n]*\n(.*?)(?=^## \[|^\[[^\]]+\]: |\Z)", text, re.MULTILINE | re.DOTALL)
    return match.group(1).strip() if match else None


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[0] == "--tag":
        version = argv[1].removeprefix("v")
    elif len(argv) == 1:
        version = argv[0]
    else:
        print(__doc__, file=sys.stderr)
        return 2
    text = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    body = section(text, version)
    if body is None and "-" in version:
        body = section(text, "Unreleased")
    if body is None:
        print(f"CHANGELOG.md has no section for {version}", file=sys.stderr)
        return 1
    print(body)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
