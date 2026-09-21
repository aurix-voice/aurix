# Packaging the plugin (Fab / Marketplace, installed-engine builds)

`AurixVoice` follows the code-plugin layout Epic's packaging tooling expects:

| Path | Purpose |
| --- | --- |
| `AurixVoice.uplugin` | `EngineVersion` 5.3.0, `SupportedTargetPlatforms` Win64 / Linux / Mac, two runtime modules, `AudioCapture` dependency, docs/support URLs. Bump `VersionName` together with the repository version; `Version` is the integer marketplace counter. |
| `Config/FilterPlugin.ini` | Extra files copied by `BuildPlugin` (the `Docs/` folder). |
| `Resources/Icon128.png` | 128×128 plugin icon shown in the Plugins browser. |
| `Source/AurixVoice` | Runtime module: subsystem, sound waves, types. `PublicDependencyModuleNames` only `Core` / `CoreUObject` / `Engine`; audio/HTTP/plugin-manager deps are private. |
| `Source/AurixVoiceSamples` | Runtime module with Blueprint-ready components and a function library; depends only on `AurixVoice` + engine. Removable. |
| `Source/ThirdParty/AurixClientLibrary` | External module: header + prebuilt `aurix_client` library per platform, `RuntimeDependencies` for the shared library. Fails the build with a clear message when unstaged. |
| `Docs/` | Quick start and this file. |

Content (`Content/`) is intentionally absent (`CanContainContent: false`); a future Content
sample would flip that flag and add `Content/...` to the filter.

## Build with `RunUAT BuildPlugin`

```bash
# Stage the native library for every platform you package for:
sdk/unreal/scripts/build_native.sh                     # Linux and/or macOS host
sdk\unreal\scripts\build_native.ps1                    # Windows host (MSVC .lib + .dll)

# Then, from an installed or source engine:
Engine/Build/BatchFiles/RunUAT.sh BuildPlugin \
  -Plugin=/abs/path/sdk/unreal/AurixVoice/AurixVoice.uplugin \
  -Package=/abs/path/out/AurixVoice \
  -TargetPlatforms=Linux -StrictIncludes -Rocket
```

Windows: `RunUAT.bat BuildPlugin -Plugin=... -Package=... -TargetPlatforms=Win64 -StrictIncludes -Rocket`.

* `-Rocket` compiles the way an installed (launcher) engine does, which is what Fab requires.
* `-StrictIncludes` catches missing includes that a unity build would hide.
* The `lib/<Platform>` folder for every `-TargetPlatforms` entry must be staged first; the
  ThirdParty module throws at UBT time otherwise.
* The output folder is what you upload or drop into another project's `Plugins/`; it contains
  `Binaries/`, `Intermediate/Build/.../Inc` (generated headers) and the copied sources.

## CI

`.github/workflows/ci.yml` job `unreal`:

1. **Always:** `python3 sdk/unreal/scripts/check_plugin.py` — engine-free checks: `.uplugin`
   schema/URLs/version parity with the repository, `FilterPlugin.ini`, icon size, one
   `<Module>.Build.cs` per declared module, header conventions (`#pragma once`, `GENERATED_BODY`
   in every `UCLASS`/`USTRUCT`, the `*.generated.h` include last and matching the file name,
   `AURIXVOICE_API` / `AURIXVOICESAMPLES_API` on exported types, `UFUNCTION()` on every
   `AddDynamic` handler, dependency direction between the modules, sample module free of
   `aurix_*` calls). The Rust test `unreal_plugin_uses_only_existing_abi` (job `native core`)
   verifies every `aurix_*` / `AURIX_*` / `aurix::` reference against the committed headers.
2. **Gated:** when the repository has the `UE_GHCR_TOKEN` secret (a GitHub token of an account
   in the Epic Games organisation, which is what `ghcr.io/epicgames/unreal-engine` requires),
   the job stages the native library, logs in to GHCR, runs `RunUAT BuildPlugin` for Linux inside
   the `dev-slim-5.3` image and uploads the packaged plugin as an artifact. Without the secret
   the steps are skipped and the job reports that the engine build did not run — nothing in this
   repository claims a UE compile it did not perform.

## Fab / Marketplace checklist

* Fill `MarketplaceURL` once the listing exists; `DocsURL` and `SupportURL` point at the
  repository.
* Ship the prebuilt `aurix_client` library for each platform in the listing (the source build
  needs a Rust toolchain, which Fab reviewers do not run).
* Third-party notices: `aurix_client` statically links Opus (BSD-3) and the Rust crates listed
  by `cargo deny` / `cargo about` in the repository; include them in the listing's notices.
* Test the packaged folder in a clean Blueprint-only project on every platform you list — that
  exercises the delay-loaded DLL path on Windows and `RuntimeDependencies` staging.
