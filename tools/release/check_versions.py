#!/usr/bin/env python3
"""Assert that every artifact Aurix ships carries the workspace version.

    python3 tools/release/check_versions.py            # compare against Cargo.toml
    python3 tools/release/check_versions.py 1.3.0      # compare against an explicit version
    python3 tools/release/check_versions.py --tag v1.3.0

Also checks that CHANGELOG.md has a released section for that version (any `-rc.N` suffix
is allowed to point at an `Unreleased` section instead). Exit code 1 lists every mismatch.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

SEMVER = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
    r"(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
)


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def regex(path: str, pattern: str) -> str:
    match = re.search(pattern, read(path), re.MULTILINE)
    if not match:
        raise SystemExit(f"{path}: pattern {pattern!r} not found")
    return match.group(1)


def json_key(path: str, *keys: str) -> str:
    value = json.loads(read(path))
    for key in keys:
        value = value[key]
    return str(value)


def workspace_version(path: str) -> str:
    return regex(path, r'^\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"')


# (label, path, extractor). One row per user-visible version string in the repository.
SOURCES = [
    ("workspace", "Cargo.toml", workspace_version),
    ("OpenAPI", "api/openapi.json", lambda p: json_key(p, "info", "version")),
    ("OpenAPI (docs copy)", "docs/src/api/openapi.json", lambda p: json_key(p, "info", "version")),
    ("Helm appVersion", "deploy/helm/aurix/Chart.yaml", lambda p: regex(p, r'^appVersion:\s*"?([^"\s]+)"?')),
    ("Web SDK", "sdk/web/package.json", lambda p: json_key(p, "version")),
    ("Unity UPM", "sdk/unity/package.json", lambda p: json_key(p, "version")),
    ("Unity SdkVersion", "sdk/unity/Runtime/AurixVoiceClient.cs", lambda p: regex(p, r'SdkVersion\s*=\s*"([^"]+)"')),
    ("Unreal uplugin", "sdk/unreal/AurixVoice/AurixVoice.uplugin", lambda p: json_key(p, "VersionName")),
    ("Node server SDK", "sdk/server/node/package.json", lambda p: json_key(p, "version")),
    ("Node SDK_VERSION", "sdk/server/node/src/http.ts", lambda p: regex(p, r'SDK_VERSION\s*=\s*"([^"]+)"')),
    ("Python server SDK", "sdk/server/python/pyproject.toml", lambda p: regex(p, r'^version\s*=\s*"([^"]+)"')),
    ("Python SDK_VERSION", "sdk/server/python/aurix_server/_http.py", lambda p: regex(p, r'SDK_VERSION\s*=\s*"([^"]+)"')),
    ("Go SDKVersion", "sdk/server/go/client.go", lambda p: regex(p, r'SDKVersion\s*=\s*"([^"]+)"')),
    ("C# csproj", "sdk/server/csharp/Aurix.Server/Aurix.Server.csproj", lambda p: regex(p, r"<Version>([^<]+)</Version>")),
    ("C# SdkVersion", "sdk/server/csharp/Aurix.Server/AurixHttp.cs", lambda p: regex(p, r'SdkVersion\s*=\s*"([^"]+)"')),
]


def changelog_has(version: str) -> bool:
    text = read("CHANGELOG.md")
    if re.search(rf"^## \[{re.escape(version)}\]", text, re.MULTILINE):
        return True
    # Pre-releases may be cut from the Unreleased section.
    return "-" in version and re.search(r"^## \[Unreleased\]", text, re.MULTILINE) is not None


def main(argv: list[str]) -> int:
    expected: str | None = None
    args = list(argv)
    if args[:1] == ["--tag"]:
        if len(args) != 2 or not args[1].startswith("v"):
            print("usage: check_versions.py --tag vX.Y.Z", file=sys.stderr)
            return 2
        expected = args[1][1:]
    elif len(args) == 1:
        expected = args[0]
    elif args:
        print(__doc__, file=sys.stderr)
        return 2

    found = {label: extractor(path) for label, path, extractor in SOURCES}
    if expected is None:
        expected = found["workspace"]
    if not SEMVER.match(expected):
        print(f"'{expected}' is not a semantic version", file=sys.stderr)
        return 1

    failures = [f"  {label:22} {path}: {value}" for (label, path, _), value in zip(SOURCES, found.values()) if value != expected]
    if not changelog_has(expected):
        failures.append(f"  {'CHANGELOG':22} CHANGELOG.md: no '## [{expected}]' section")
    if failures:
        print(f"version mismatch, expected {expected}:")
        print("\n".join(failures))
        return 1
    print(f"all {len(SOURCES)} version strings and CHANGELOG agree on {expected}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
