#!/usr/bin/env python3
"""Generate the server SDK clients (Node, Python, Go, C#) from api/openapi.json.

    python3 tools/openapi-sdk/generate.py            # write files
    python3 tools/openapi-sdk/generate.py --check    # fail if committed output is stale

Only the `generated/` parts of each SDK are produced here; transports, auth, retries,
webhook verification and SSE are hand-written in each SDK and never touched.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import emit_cs  # noqa: E402
import emit_go  # noqa: E402
import emit_py  # noqa: E402
import emit_ts  # noqa: E402
import ir  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
SPEC = ROOT / "api" / "openapi.json"
SDK = ROOT / "sdk" / "server"


def outputs(api: ir.Api) -> dict:
    return {
        SDK / "node" / "src" / "generated" / "types.ts": emit_ts.emit_types(api),
        SDK / "node" / "src" / "generated" / "client.ts": emit_ts.emit_client(api),
        SDK / "python" / "aurix_server" / "generated" / "types.py": emit_py.emit_types(api),
        SDK / "python" / "aurix_server" / "generated" / "client.py": emit_py.emit_client(api),
        SDK / "go" / "types_gen.go": emit_go.emit_types(api),
        SDK / "go" / "client_gen.go": emit_go.emit_client(api),
        SDK / "csharp" / "Aurix.Server" / "Generated" / "Types.cs": emit_cs.emit_types(api),
        SDK / "csharp" / "Aurix.Server" / "Generated" / "Client.cs": emit_cs.emit_client(api),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="verify committed output matches the spec")
    ap.add_argument("--only", choices=["node", "python", "go", "csharp"], help="limit to one SDK")
    args = ap.parse_args()
    api = ir.load(SPEC)
    stale = []
    for path, content in outputs(api).items():
        if args.only and args.only not in path.parts:
            continue
        if args.check:
            if not path.exists() or path.read_text() != content:
                stale.append(path)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
    summary = f"{len(api.operations)} operations, {len(api.models)} models, {len(api.enums)} enums"
    if api.skipped:
        summary += "; not generated: " + ", ".join(f"{op} ({why})" for op, why in api.skipped)
    if args.check:
        if stale:
            print("stale generated SDK files (run tools/openapi-sdk/generate.py):", file=sys.stderr)
            for p in stale:
                print(f"  {p.relative_to(ROOT)}", file=sys.stderr)
            return 1
        print(f"generated server SDKs are up to date ({summary})")
    else:
        print(f"wrote server SDKs ({summary})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
