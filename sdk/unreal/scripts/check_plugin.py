#!/usr/bin/env python3
"""Engine-free consistency checks for sdk/unreal/AurixVoice.

Unreal Engine is not available on the CI hosts, so this validates what can be validated without
it: the .uplugin descriptor, packaging files (FilterPlugin.ini, icon), module layout, the UHT
conventions every reflected header must follow, and the dependency direction between modules.
It does NOT compile anything — `RunUAT BuildPlugin` runs separately when engine images are
available (see Docs/Packaging.md). ABI drift against the C headers is covered by the Rust test
`unreal_plugin_uses_only_existing_abi`.
"""

from __future__ import annotations

import json
import re
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
PLUGIN = ROOT / "sdk" / "unreal" / "AurixVoice"

errors: list[str] = []


def fail(msg: str) -> None:
    errors.append(msg)


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def workspace_version() -> str:
    text = read(ROOT / "Cargo.toml")
    m = re.search(r"^\[workspace\.package\][^\[]*?^version\s*=\s*\"([^\"]+)\"", text, re.M | re.S)
    if not m:
        fail("Cargo.toml: workspace.package.version not found")
        return ""
    return m.group(1)


def check_uplugin() -> list[str]:
    path = PLUGIN / "AurixVoice.uplugin"
    try:
        desc = json.loads(read(path))
    except (OSError, json.JSONDecodeError) as e:
        fail(f"{path.name}: {e}")
        return []

    for key in ("FileVersion", "Version", "VersionName", "FriendlyName", "Description", "Category",
                "CreatedBy", "EngineVersion", "Modules", "SupportedTargetPlatforms"):
        if key not in desc:
            fail(f"{path.name}: missing {key}")
    if desc.get("FileVersion") != 3:
        fail(f"{path.name}: FileVersion must be 3")
    if not isinstance(desc.get("Version"), int) or desc["Version"] < 1:
        fail(f"{path.name}: Version must be a positive integer")
    version = workspace_version()
    if version and desc.get("VersionName") != version:
        fail(f"{path.name}: VersionName {desc.get('VersionName')!r} != workspace version {version!r}")
    if not re.fullmatch(r"\d+\.\d+\.\d+", str(desc.get("EngineVersion", ""))):
        fail(f"{path.name}: EngineVersion must look like 5.3.0")
    if len(str(desc.get("Description", ""))) > 2000:
        fail(f"{path.name}: Description too long")
    for key in ("CreatedByURL", "DocsURL", "SupportURL", "MarketplaceURL"):
        url = desc.get(key, "")
        if url and not re.match(r"https://[^\s]+$", url):
            fail(f"{path.name}: {key} must be an https URL or empty")
    if not desc.get("DocsURL"):
        fail(f"{path.name}: DocsURL is empty")
    if not desc.get("SupportURL"):
        fail(f"{path.name}: SupportURL is empty")
    docs_url = desc.get("DocsURL", "")
    m = re.search(r"/sdk/unreal/AurixVoice/(Docs/[^\s#?]+)", docs_url)
    if m and not (PLUGIN / m.group(1)).is_file():
        fail(f"{path.name}: DocsURL points at missing file {m.group(1)}")
    if desc.get("CanContainContent") and not (PLUGIN / "Content").is_dir():
        fail(f"{path.name}: CanContainContent is true but there is no Content/ folder")
    if not desc.get("CanContainContent") and (PLUGIN / "Content").is_dir():
        fail(f"{path.name}: Content/ exists but CanContainContent is false")

    platforms = set(desc.get("SupportedTargetPlatforms", []))
    if not platforms:
        fail(f"{path.name}: SupportedTargetPlatforms is empty")

    names = []
    for module in desc.get("Modules", []):
        name = module.get("Name")
        if not name:
            fail(f"{path.name}: module without Name")
            continue
        names.append(name)
        if module.get("Type") not in ("Runtime", "RuntimeNoCommandlet", "Editor", "Developer", "UncookedOnly"):
            fail(f"{path.name}: module {name} has unexpected Type {module.get('Type')!r}")
        if module.get("LoadingPhase") not in ("Default", "PreDefault", "PostDefault", "PostEngineInit", "PreLoadingScreen"):
            fail(f"{path.name}: module {name} has unexpected LoadingPhase")
        allow = set(module.get("PlatformAllowList", []))
        if allow and not allow <= platforms:
            fail(f"{path.name}: module {name} PlatformAllowList {sorted(allow - platforms)} not in SupportedTargetPlatforms")
        build_cs = PLUGIN / "Source" / name / f"{name}.Build.cs"
        if not build_cs.is_file():
            fail(f"missing {build_cs.relative_to(ROOT)}")
        elif not re.search(rf"public class {name}\s*:\s*ModuleRules", read(build_cs)):
            fail(f"{build_cs.relative_to(ROOT)}: does not declare `public class {name} : ModuleRules`")
    if len(names) != len(set(names)):
        fail(f"{path.name}: duplicate module names")

    for dep in desc.get("Plugins", []):
        if not dep.get("Name") or "Enabled" not in dep:
            fail(f"{path.name}: plugin dependency needs Name and Enabled")

    # Source module folders that the descriptor does not list would silently not be built.
    for module_dir in (PLUGIN / "Source").iterdir():
        if module_dir.name == "ThirdParty" or not module_dir.is_dir():
            continue
        if module_dir.name not in names:
            fail(f"Source/{module_dir.name} exists but is not listed in {path.name}")
    return names


def check_packaging() -> None:
    ini = PLUGIN / "Config" / "FilterPlugin.ini"
    if not ini.is_file():
        fail("missing Config/FilterPlugin.ini")
    else:
        text = read(ini)
        if "[FilterPlugin]" not in text:
            fail("Config/FilterPlugin.ini: missing [FilterPlugin] section")
        for line in text.splitlines():
            line = line.strip()
            if not line or line.startswith(";") or line.startswith("["):
                continue
            if not line.startswith("/"):
                fail(f"Config/FilterPlugin.ini: entry {line!r} must start with /")
                continue
            base = line[1:].split("...")[0].split("*")[0].split("?")[0].rstrip("/")
            if base and not (PLUGIN / base).exists():
                fail(f"Config/FilterPlugin.ini: {line!r} matches nothing")

    icon = PLUGIN / "Resources" / "Icon128.png"
    if not icon.is_file():
        fail("missing Resources/Icon128.png")
    else:
        data = icon.read_bytes()
        if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
            fail("Resources/Icon128.png is not a PNG")
        else:
            w, h = struct.unpack(">II", data[16:24])
            if (w, h) != (128, 128):
                fail(f"Resources/Icon128.png is {w}x{h}, expected 128x128")

    for doc in ("Docs/QuickStart.md", "Docs/Packaging.md"):
        if not (PLUGIN / doc).is_file():
            fail(f"missing {doc}")

    for stray in ("Binaries", "Intermediate", "Saved"):
        if (PLUGIN / stray).exists():
            # Ignored by git, but a stale build output would end up in an archive.
            print(f"note: {stray}/ present locally (git-ignored build output)")


GENERATED_INCLUDE = re.compile(r'#include\s+"([A-Za-z0-9_]+)\.generated\.h"')
REFLECTED = re.compile(r"^\s*(UCLASS|USTRUCT|UENUM|UINTERFACE)\s*\(", re.M)


def module_api_macro(module: str) -> str:
    return f"{module.upper()}_API"


def check_headers(modules: list[str]) -> None:
    for module in modules:
        api = module_api_macro(module)
        for header in sorted((PLUGIN / "Source" / module).rglob("*.h")):
            rel = header.relative_to(ROOT)
            text = read(header)
            pragma = text.find("#pragma once")
            first_include = text.find("#include")
            if pragma < 0 or (first_include >= 0 and pragma > first_include):
                fail(f"{rel}: #pragma once must precede the first #include")
            includes = re.findall(r'^\s*#include\s+[<"]([^>"]+)[>"]', text, re.M)
            gen = GENERATED_INCLUDE.findall(text)
            reflected = REFLECTED.findall(text)
            if reflected:
                if not gen:
                    fail(f"{rel}: has {'/'.join(sorted(set(reflected)))} but no .generated.h include")
                elif gen != [header.stem]:
                    fail(f"{rel}: generated include must be \"{header.stem}.generated.h\", found {gen}")
                elif not includes[-1].endswith(f"{header.stem}.generated.h"):
                    fail(f"{rel}: the .generated.h include must be the last #include")
                if header.parent.name != "Public" and "Private" not in header.parts:
                    fail(f"{rel}: reflected headers belong in Public/ or Private/")
            elif gen:
                fail(f"{rel}: includes .generated.h without any reflected type")

            # UCLASS/USTRUCT bodies need GENERATED_BODY(); enums do not.
            n_bodies = len(re.findall(r"GENERATED_(?:UCLASS_|USTRUCT_)?BODY\s*\(", text))
            n_types = len(re.findall(r"^\s*(?:UCLASS|USTRUCT|UINTERFACE)\s*\(", text, re.M))
            if n_bodies != n_types:
                fail(f"{rel}: {n_types} UCLASS/USTRUCT/UINTERFACE but {n_bodies} GENERATED_BODY()")

            # Exported types in Public/ carry the module API macro; Private/ types must not.
            for m in re.finditer(r"^\s*(?:class|struct)\s+((?:[A-Z_]+_API)\s+)?([AUFE][A-Za-z0-9_]+)\s*(?::|\{|$)", text, re.M):
                macro, name = (m.group(1) or "").strip(), m.group(2)
                if header.parent.name == "Public":
                    if macro and macro != api:
                        fail(f"{rel}: {name} exported with {macro}, expected {api}")
                elif macro:
                    fail(f"{rel}: private type {name} must not be exported ({macro})")
            # Every UCLASS in Public/ must be exported, otherwise game modules cannot link to it.
            for m in re.finditer(r"^\s*UCLASS\(.*\)\s*\n\s*class\s+(\w+)(?:\s+(\w+))?", text, re.M):
                if header.parent.name == "Public" and m.group(1) != api:
                    fail(f"{rel}: UCLASS {m.group(1)} is not exported with {api}")

        # Dynamic-delegate handlers must be UFUNCTIONs (AddDynamic on a non-UFUNCTION silently fails).
        headers_text = {h: read(h) for h in (PLUGIN / "Source" / module).rglob("*.h")}
        for cpp in sorted((PLUGIN / "Source" / module).rglob("*.cpp")):
            text = read(cpp)
            for cls, handler in set(re.findall(r"AddDynamic\(this,\s*&(\w+)::(\w+)\)", text)):
                declared = False
                for h, htext in headers_text.items():
                    if f"class {api} {cls}" in htext or f"class {cls}" in htext:
                        if re.search(rf"UFUNCTION\([^)]*\)\s*(?:virtual\s+)?\w[\w<>:&* ]*\s+{handler}\s*\(", htext):
                            declared = True
                if not declared:
                    fail(f"{cpp.relative_to(ROOT)}: {cls}::{handler} is bound with AddDynamic but is not a UFUNCTION")


def check_dependencies(modules: list[str]) -> None:
    deps: dict[str, set[str]] = {}
    for module in modules:
        text = read(PLUGIN / "Source" / module / f"{module}.Build.cs")
        deps[module] = set(re.findall(r'"([A-Za-z0-9_]+)"', text)) & (set(modules) | {"AurixClientLibrary"})
    if "AurixVoice" in deps and "AurixClientLibrary" not in deps["AurixVoice"]:
        fail("AurixVoice.Build.cs must depend on AurixClientLibrary")
    for module in modules:
        if module == "AurixVoice":
            continue
        if "AurixVoice" not in deps[module]:
            fail(f"{module}.Build.cs must depend on AurixVoice")
        if "AurixClientLibrary" in deps[module]:
            fail(f"{module}.Build.cs must not depend on AurixClientLibrary directly (go through AurixVoice)")
        if module in deps.get("AurixVoice", set()):
            fail(f"AurixVoice.Build.cs must not depend on {module} (cycle)")
        for src in (PLUGIN / "Source" / module).rglob("*.[ch]*"):
            text = read(src)
            if re.search(r"\baurix_[a-z_]+\s*\(", text) or "aurix_client.h" in text or "aurix::" in text:
                fail(f"{src.relative_to(ROOT)}: {module} must use only the UAurixVoiceSubsystem API, not the C ABI")


def check_thirdparty() -> None:
    tp = PLUGIN / "Source" / "ThirdParty" / "AurixClientLibrary"
    build_cs = tp / "AurixClientLibrary.Build.cs"
    if not build_cs.is_file():
        fail("missing Source/ThirdParty/AurixClientLibrary/AurixClientLibrary.Build.cs")
        return
    text = read(build_cs)
    if "ModuleType.External" not in text:
        fail("AurixClientLibrary.Build.cs: must be Type = ModuleType.External")
    if "BuildException" not in text:
        fail("AurixClientLibrary.Build.cs: must throw BuildException when the native library is unstaged")
    if "RuntimeDependencies" not in text:
        fail("AurixClientLibrary.Build.cs: must register the shared library in RuntimeDependencies")
    gitignore = read(ROOT / ".gitignore")
    for staged in ("include/", "lib/"):
        if f"sdk/unreal/AurixVoice/Source/ThirdParty/AurixClientLibrary/{staged}" not in gitignore:
            fail(f".gitignore: staged {staged} under AurixClientLibrary must be ignored (built per host)")


def main() -> int:
    modules = check_uplugin()
    check_packaging()
    if modules:
        check_headers(modules)
        check_dependencies(modules)
    check_thirdparty()
    if errors:
        for e in errors:
            print(f"error: {e}")
        print(f"unreal plugin checks: {len(errors)} problem(s)")
        return 1
    print(f"unreal plugin checks: ok ({', '.join(modules)}; engine build not performed here)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
