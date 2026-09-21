# Releases and versioning

Aurix is versioned as one product. The server, every client SDK (native core / C ABI, Web,
Unity, Unreal, Godot), the server SDKs (Node, Python, Go, C#), the `aurix` CLI, the OpenAPI
contract and the Helm chart `appVersion` carry the same version and are released together from
one git tag.

## Semantic versioning

`MAJOR.MINOR.PATCH[-rc.N]`, applied to the *interfaces a game integrates against*:

| Surface | Compatible change (MINOR/PATCH) | Breaking change (MAJOR) |
|---|---|---|
| REST API (`api/openapi.json`) | new endpoints, new optional fields, new enum values on outputs | removed/renamed fields or endpoints, changed semantics, new required inputs |
| WebSocket control plane | new message types, new optional fields | changed or removed messages; clients must tolerate unknown types |
| AURX / QUIC media | new packet flags with a negotiated capability | wire-format change without negotiation (protocol version bump) |
| C ABI (`aurix_client.h`) | new functions, new struct fields at the end with a size/version prefix | changed signatures or struct layouts |
| Web / Unity / Unreal / Godot / server SDKs | new API, deprecations kept for one MINOR | removal of deprecated API |
| Configuration (`configs/default.toml`, `AURIX__*`) | new keys with defaults | removed keys, changed defaults that alter security posture |
| Database | additive migrations | migrations that require a stop-the-world upgrade (documented in the release notes) |
| Webhook / SSE events | new event types, new fields | changed payload shapes |

Rules that follow from this:

* A node of version `X.Y` accepts clients built against any `X.*` SDK. Newer clients degrade
  gracefully against older nodes when the node advertises what it supports
  (`SessionInitAck` capabilities, `/health` version).
* Nodes in one fleet may differ by one MINOR during a rolling upgrade; cross-node failover and
  cascade relays stay wire-compatible inside a MAJOR.
* Deprecations are announced in `CHANGELOG.md` under *Deprecated* and removed no earlier than the
  next MAJOR.
* Security fixes are released as PATCH on the current and previous MINOR (see `SECURITY.md`).
* Pre-releases (`-rc.N`) are cut from the *Unreleased* changelog section, get a pre-release
  GitHub release, and never move the `latest` / `X.Y` / `X` image tags.

## Changelog

`CHANGELOG.md` follows *Keep a Changelog*. Every user-visible change lands with an entry under
`## [Unreleased]` in one of *Added / Changed / Deprecated / Removed / Fixed / Security*. The
release commit renames that section to `## [X.Y.Z] - YYYY-MM-DD` and adds the compare link at the
bottom.

## Cutting a release

1. Bump the version everywhere and check it:

   ```sh
   # Cargo.toml [workspace.package].version, api/openapi.json, docs/src/api/openapi.json,
   # Helm appVersion, sdk/web, sdk/unity (+ SdkVersion), Unreal uplugin, server SDKs (4 languages)
   python3 tools/release/check_versions.py 1.3.0
   ```

   The script lists every file that still carries the old version. `cargo update -w` refreshes
   `Cargo.lock`; `npm install` in `sdk/web` refreshes its lockfile.

2. Turn `## [Unreleased]` into `## [1.3.0] - <date>` in `CHANGELOG.md`.

3. Run the full gate (`cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`,
   `cargo deny check`, `python3 tools/openapi-sdk/generate.py --check`,
   `cargo test --manifest-path fuzz/Cargo.toml --release`) and commit `Release 1.3.0`.

4. Tag and push:

   ```sh
   git tag -a v1.3.0 -m "Aurix 1.3.0"
   git push origin main v1.3.0
   ```

The `release` workflow (`.github/workflows/release.yml`) does the rest. Run it manually via
*workflow_dispatch* for a dry run that builds and checks everything without publishing.

## What a release contains

| Asset | Built for | Notes |
|---|---|---|
| `aurix-server-<v>-<target>.tar.gz` | Linux x86_64, aarch64 | `bin/aurix-server`, `bin/aurix`, `bin/aurix-loadtest`, `configs/`, `migrations/`, docs |
| `aurix-cli-<v>-<target>.{tar.gz,zip}` | Linux x86_64/aarch64, Windows x64, macOS arm64/x86_64 | the `aurix` CLI alone |
| `aurix-client-<v>-<target>.{tar.gz,zip}` | same five targets | `libaurix_client` (shared + import lib on Windows) and `aurix_client.h` / `.hpp` |
| `aurix-web-sdk-<v>.tgz` | — | `npm pack` output of `@aurix/web-sdk`; `npm install ./aurix-web-sdk-<v>.tgz` |
| `com.aurix.voice-<v>.tgz` | — | Unity Package Manager tarball (*Add package from tarball*) |
| `AurixVoice-unreal-<v>.zip` | — | plugin with Win64 / Mac (universal) / Linux native libraries; drop into `Plugins/` |
| `aurix-godot-addon-<v>.zip` | — | addon **sources**; build the GDExtension per platform with `sdk/godot/scripts/build_native.sh` |
| `aurix-server-sdk-<v>.tgz`, `aurix_server_sdk-<v>.tar.gz` / `.whl`, `Aurix.Server.<v>.nupkg` | — | `@aurix/server-sdk` / `aurix-server-sdk` / `Aurix.Server`; Go is consumed as a module from the git tag |
| `aurix-<v>.source.cdx.json` | — | CycloneDX SBOM of the source tree (Cargo, npm, Python, Go, NuGet lockfiles) |
| `aurix-server-<v>.image.cdx.json` | — | CycloneDX SBOM of the container image |
| `SHA256SUMS`, `SHA256SUMS.sig`, `SHA256SUMS.pem`, `SHA256SUMS.sigstore.json` | — | checksums and their Sigstore signature |
| git tag `sdk/server/go/v<v>` | — | makes `go get …/sdk/server/go@v<v>` resolve |
| `ghcr.io/<owner>/aurix-server:<v>` | linux/amd64, linux/arm64 | also `:X.Y`, `:X` and `:latest` for stable releases; signed, with an attached SBOM attestation |

Packages are **not** pushed to npm, PyPI, NuGet or pkg.go.dev. Install from the release assets or
the git URL (UPM: `https://github.com/<owner>/aurix.git?path=/sdk/unity#v1.3.0`;
Go: `go get github.com/<owner>/aurix/sdk/server/go@v1.3.0` — the workflow also pushes the
`sdk/server/go/v1.3.0` tag that Go needs for a module in a subdirectory).

## Verifying a download

```sh
sha256sum -c --ignore-missing SHA256SUMS

# SHA256SUMS was signed keylessly by the release workflow; the certificate identity is the
# workflow file in this repository, the issuer is GitHub's OIDC provider.
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp '^https://github.com/<owner>/aurix/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

cosign verify ghcr.io/<owner>/aurix-server:1.3.0 \
  --certificate-identity-regexp '^https://github.com/<owner>/aurix/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
cosign verify-attestation --type cyclonedx ghcr.io/<owner>/aurix-server:1.3.0 \
  --certificate-identity-regexp '^https://github.com/<owner>/aurix/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com | jq -r .payload | base64 -d | jq .predicate.components[].name

# Every asset also has a GitHub build-provenance attestation:
gh attestation verify aurix-server-1.3.0-x86_64-unknown-linux-gnu.tar.gz --repo <owner>/aurix
```

## What the workflow needs

* **No stored secrets.** Image push uses the job's `GITHUB_TOKEN` (`packages: write`); Sigstore
  signing and provenance use the job's OIDC token (`id-token: write`, `attestations: write`).
  Keyless signing publishes the signing certificate — containing the repository and workflow
  path — to the public Rekor transparency log; if that is unacceptable, remove the `cosign`
  steps or switch them to a key stored in a secret.
* **Runners.** `ubuntu-22.04-arm` (aarch64 server/CLI/native core and the `linux/arm64` image —
  each image architecture is compiled on a runner of that architecture and the two are merged into
  one manifest list, no QEMU) is available for public repositories and for organisations with ARM
  runners enabled; drop those matrix rows otherwise. macOS and Windows runners build the native
  core and CLI.
* **GHCR.** The image is pushed to `ghcr.io/<repository owner>/aurix-server`; the package's
  visibility is set once, in the GitHub Packages UI.

## What is not automated

* Package registry publication (npm / PyPI / NuGet). The packages are built and attached; pushing
  them needs registry accounts and tokens the project does not hold.
* Godot native libraries (godot-cpp + SCons per platform, Android NDK, Xcode) and the Unreal
  `BuildPlugin` compile against a real engine (needs the Epic container secret, see the `unreal`
  CI job).
* Release notes beyond the changelog section — write them in `CHANGELOG.md` before tagging.
