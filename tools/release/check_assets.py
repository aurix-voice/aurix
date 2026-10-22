#!/usr/bin/env python3
"""Release acceptance: every SDK/package asset contains what its consumers expect.

    python3 tools/release/check_assets.py <version> <dir-with-assets>

Run by the release workflow before checksums are written; catches packaging regressions
(missing README, missing prebuilt library for a platform, wrong folder layout) that the
build jobs themselves cannot see because each builds only its own piece.
"""
from __future__ import annotations

import sys
import tarfile
import zipfile
from pathlib import Path


def names_tgz(p: Path) -> set[str]:
    with tarfile.open(p, "r:gz") as t:
        return {m.name for m in t.getmembers()}


def names_zip(p: Path) -> set[str]:
    with zipfile.ZipFile(p) as z:
        return set(z.namelist())


def expect(found: set[str], required: list[str], asset: str, failures: list[str]) -> None:
    for pattern in required:
        if pattern.endswith("*"):
            ok = any(n.startswith(pattern[:-1]) for n in found)
        else:
            ok = pattern in found
        if not ok:
            failures.append(f"{asset}: missing {pattern}")


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    version, root = argv[1], Path(argv[2])
    failures: list[str] = []
    checks = [
        # (asset file name, reader, required entries; `*` = prefix match)
        (
            f"aurix-godot-addon-{version}.zip",
            names_zip,
            [
                "aurix_voice/LICENSE",
                "aurix_voice/web/*",
                "aurix_voice/aurix_voice.gdextension",
                "aurix_voice/README.md",
                "aurix_voice/bin/linux.x86_64/*",
                "aurix_voice/bin/linux.arm64/*",
                "aurix_voice/bin/windows.x86_64/*",
                "aurix_voice/bin/macos.universal/*",
                "aurix_voice/bin/android.arm64/*",
            ],
        ),
        (
            f"AurixVoice-unreal-{version}.zip",
            names_zip,
            [
                "AurixVoice/AurixVoice.uplugin",
                "AurixVoice/README.md",
                "AurixVoice/Source/ThirdParty/AurixClientLibrary/include/aurix_client.h",
                "AurixVoice/Source/ThirdParty/AurixClientLibrary/lib/Win64/aurix_client.dll",
                "AurixVoice/Source/ThirdParty/AurixClientLibrary/lib/Linux/libaurix_client.so",
                "AurixVoice/Source/ThirdParty/AurixClientLibrary/lib/Mac/libaurix_client.dylib",
                "AurixVoice/Source/ThirdParty/AurixClientLibrary/lib/Android/arm64-v8a/libaurix_client.so",
                "AurixVoice/Source/ThirdParty/AurixClientLibrary/lib/Android/armeabi-v7a/libaurix_client.so",
                "AurixVoice/Source/ThirdParty/AurixClientLibrary/lib/Android/x86_64/libaurix_client.so",
                "AurixVoice/Source/ThirdParty/AurixClientLibrary/AurixClientLibrary_APL.xml",
            ],
        ),
        (f"com.aurix.voice-{version}.tgz", names_tgz, ["package/package.json", "package/README.md", "package/Runtime/*", "package/Editor/*"]),
        (f"aurix-web-sdk-{version}.tgz", names_tgz, ["package/package.json", "package/README.md", "package/dist/*"]),
        (f"aurix-server-sdk-{version}.tgz", names_tgz, ["package/package.json", "package/README.md", "package/dist/*"]),
        (f"aurix_server_sdk-{version}-py3-none-any.whl", names_zip, ["aurix_server/__init__.py", f"aurix_server_sdk-{version}.dist-info/METADATA"]),
        (f"Aurix.Server.{version}.nupkg", names_zip, ["Aurix.Server.nuspec", "README.md", "lib/net8.0/Aurix.Server.dll"]),
        (
            f"aurix-client-{version}-android.tar.gz",
            names_tgz,
            [f"aurix-client-{version}-android/include/aurix_client.h"]
            + [f"aurix-client-{version}-android/lib/{abi}/libaurix_client.so" for abi in ("arm64-v8a", "armeabi-v7a", "x86_64")],
        ),
    ]
    for asset, reader, required in checks:
        p = root / asset
        if not p.exists():
            failures.append(f"{asset}: not found")
            continue
        try:
            found = reader(p)
        except Exception as e:  # noqa: BLE001
            failures.append(f"{asset}: unreadable ({e})")
            continue
        expect(found, required, asset, failures)
        if asset.endswith(".whl"):
            with zipfile.ZipFile(p) as z:
                meta = z.read(f"aurix_server_sdk-{version}.dist-info/METADATA").decode()
            if "Description-Content-Type" not in meta or len(meta.split("\n\n", 1)[-1].strip()) < 100:
                failures.append(f"{asset}: wheel has no long description (README)")
    if failures:
        print("release asset check failed:")
        for f in failures:
            print("  " + f)
        return 1
    print(f"release assets for {version}: {len(checks)} packages contain everything expected")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
