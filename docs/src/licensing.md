# Licensing

Aurix uses two licences along one boundary: **what an operator runs is AGPL-3.0-only, what a game
links or ships is Apache-2.0.** The authoritative map is [`REUSE.toml`](https://github.com/aurix-voice/aurix/blob/main/REUSE.toml)
at the repository root, checked by `reuse lint` in CI; [`LICENSING.md`](https://github.com/aurix-voice/aurix/blob/main/LICENSING.md)
lists it path by path.

| AGPL-3.0-only | Apache-2.0 |
| --- | --- |
| server node (`aurix-server` and the media, TURN, control, database, auth, moderation, recording and metrics crates) | native client core and C ABI (`aurix-client`) |
| operator dashboard | `aurix-common`, `aurix-opus` |
| `aurix` CLI, load tester | Web, Unity, Unreal, Godot SDKs |
| fuzzing, chaos and netem harnesses, release tooling, CI | server SDKs (Node, Python, Go, C#), token-server examples, SDK generator |
| migrations, Dockerfiles | OpenAPI contract, documentation, deployment examples, sample configs |

## For a game studio

Your game links an Apache-2.0 SDK and talks to a node over the network. The SDKs contain no AGPL
code and a network client is not a derivative of the server, so the AGPL places no obligation on
the game — closed source, commercial, on any platform. Ship the SDK's `LICENSE` and `NOTICE` as
for any Apache-2.0 dependency. If you improve an SDK (an engine port, a console backend) you may
keep that improvement to yourself; contributing it back is welcome, not required.

## For an operator

Run Aurix for your studio, your players or your customers, at any scale, for free. The single
extra rule of AGPL-3.0 compared with GPL-3.0 (section 13): if you **modify** the server or the
dashboard and let users interact with the modified version over a network, you must offer those
users the source of your modifications. Running an unmodified release, or private changes that
only your own staff reach, adds nothing beyond the ordinary GPL terms. A link from the dashboard
login page or from your game's settings screen to the repository of your modified server is the
usual way to satisfy it.

## For a fork or a hosted offering

The server stays open: a fork remains AGPL-3.0, and a publicly deployed fork publishes its changes.
The name does not come with the code — "Aurix" and its logo identify this project, and a fork or
hosted service must not present itself as Aurix or as endorsed by it.

## For a contributor

A change is accepted under the licence of the paths it touches, with a Developer Certificate of
Origin sign-off (`git commit -s`). There is no contributor licence agreement and no copyright
assignment; see [Contributing](https://github.com/aurix-voice/aurix/blob/main/CONTRIBUTING.md).

## How the boundary is enforced

* Every Cargo crate, npm package, NuGet package and Python project carries its own `license`
  field and a `LICENSE` file, so downstream tooling (SBOM, `cargo deny`, `license-checker`) sees
  the right licence without reading this page.
* The Apache-2.0 client crates never depend on an AGPL crate; the AGPL server may depend on the
  Apache-2.0 crates. `cargo deny` runs against the whole workspace.
* `reuse lint` fails CI if a file is not covered by `REUSE.toml`, so a new directory has to be
  assigned explicitly.
* Release archives carry the licence of their content: the server bundle and CLI ship the AGPL
  text plus `LICENSING.md`; the client core, Godot, Unreal, Unity and server-SDK packages ship
  Apache-2.0 and `NOTICE`. Container images are labelled `AGPL-3.0-only`.

## History

Releases up to and including `v1.3.0` were published entirely under Apache-2.0 and remain
available under that licence. AGPL-3.0-only applies to the server-side parts from the first
commit after `v1.3.0`.

This page explains the layout; it is not legal advice.
