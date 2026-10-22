# Support matrix

What each SDK runs on, which engine/runtime versions it targets and what the release ships for it.
Everything here is what the release pipeline actually builds and tests (`.github/workflows/release.yml`);
platforms that only exist as build scripts are marked as such.

## Client SDKs

| SDK | Minimum version | Platforms shipped prebuilt | Media path | Release artifact |
| --- | --- | --- | --- | --- |
| **Unity** (`com.aurix.voice`) | Unity 2021.3 LTS, netstandard2.1, IL2CPP-safe | Managed C#: every Unity platform (Windows, macOS, Linux, Android, iOS, consoles via [porting guide](consoles.md)); WebGL via the Web SDK bridge | AURX over QUIC/UDP, WebSocket tunnel fallback; WebGL: WebRTC | `com.aurix.voice-<v>.tgz` (UPM), `upm` branch / tag `upm/v<v>`, OpenUPM |
| **Unreal** (`AurixVoice` plugin) | UE 5.3+ | Win64, Linux x86_64, macOS universal, Android (arm64-v8a / armeabi-v7a / x86_64) | AURX over QUIC/UDP, WebSocket tunnel fallback | `AurixVoice-unreal-<v>.zip` (source plugin + prebuilt core); engine-compiled Linux zip when the Fab job is enabled |
| **Godot** (`aurix_voice` GDExtension) | Godot 4.3+ | Linux x86_64 / arm64, Windows x86_64, macOS universal, Android arm64 (prebuilt); Android arm32/x86_64 and iOS via `scripts/build_native.sh` | AURX over QUIC/UDP, WebSocket tunnel fallback; Web exports: `AurixWebVoiceClient` (WebRTC) | `aurix-godot-addon-<v>.zip` (prebuilt), `-source.zip` |
| **Web** (`@aurix/web-sdk`) | ES2020 browsers with WebRTC (Chrome/Edge, Firefox, Safari); WebCodecs path used when available | Any browser; also Unity WebGL and Godot Web through the bridges | WebRTC (Opus), WebSocket control channel | npm `@aurix/web-sdk`, `aurix-web-sdk-<v>.tgz` |
| **Native core** (`aurix-client`, C/C++ ABI) | Rust 1.88 toolchain to build; C99 / C++17 headers | Linux x86_64 / arm64, Windows x86_64 (MSVC), macOS x86_64 / arm64, Android (3 ABIs) | AURX over QUIC/UDP, WebSocket tunnel fallback | `aurix-client-<v>-<target>.{tar.gz,zip}`, `aurix-client-<v>-android.tar.gz` |
| **Flutter / React Native** | see [guide](mobile-frameworks.md) | wraps the native core (Android) / Web SDK | — | — |

Not shipped prebuilt (needs the platform toolchain and, for consoles, the vendor SDK): iOS/tvOS,
consoles, Linux on other architectures. The native core is plain Rust + libopus, so these are build
targets rather than ports — see [Porting to consoles](consoles.md) and `sdk/godot/scripts/build_native.sh`.

## Server SDKs

| SDK | Runtime | Registry | Notes |
| --- | --- | --- | --- |
| `@aurix/server-sdk` | Node 18+ (global `fetch`), TypeScript types included | npm | generated from `api/openapi.json`; webhooks, SSE |
| `aurix-server-sdk` | Python 3.9+, sync + async clients, no dependencies | PyPI (when enabled), release wheel/sdist | same |
| `Aurix.Server` | .NET 8 | NuGet | same |
| `github.com/aurix-voice/aurix/sdk/server/go` | Go 1.22+ | Go module proxy (tag `sdk/server/go/v<v>`) | same |

Token-server examples for all four live in `sdk/server/examples/token-server`.

## Server and tooling

| Component | Platforms | Artifact |
| --- | --- | --- |
| `aurix-server` | Linux x86_64 / arm64 (glibc); container images `linux/amd64`, `linux/arm64` | `aurix-server-<v>-<target>.tar.gz`, `ghcr.io/aurix-voice/aurix-server:<v>` |
| `aurix` CLI (admin + `voice_cli` test client) | Linux x86_64 / arm64, Windows x86_64, macOS x86_64 / arm64 | `aurix-cli-<v>-<target>.{tar.gz,zip}` |
| Dashboard | static files behind Caddy, multi-arch image | `ghcr.io/aurix-voice/aurix-dashboard:<v>` |
| Helm chart | Kubernetes | `deploy/helm/aurix` (`appVersion` tracks the release) |

## Versioning

All SDKs, the server and the OpenAPI document share one version number (`tools/release/check_versions.py`
enforces it). Client SDKs talk to any server of the same major version; new server features show up as
optional fields, and a client that is older than the server simply does not see them. The wire protocol
is documented in [Native core and Unreal SDK](native.md) and [Web SDK](web.md).
