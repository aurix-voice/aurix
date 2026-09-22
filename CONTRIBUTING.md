# Contributing

Aurix is a self-hosted voice platform for games: native voice, browser voice, spatial and team
channels, engine SDKs. Text chat is a helper for that, not a product of its own; changes that pull
the project towards a streaming, social or general chat product are out of scope
([limitations](docs/src/limitations.md)).

## Before you start

* Open an issue for anything beyond a small fix so the design is agreed before the code exists.
* Security problems go through [`SECURITY.md`](SECURITY.md), never through a public issue.
* Read [`LICENSING.md`](LICENSING.md): code under `crates/aurix-client`, `crates/aurix-common`,
  `crates/aurix-opus`, `sdk/`, `api/`, `tools/openapi-sdk`, `docs/`, `deploy/` and `configs/` is
  Apache-2.0; everything else is AGPL-3.0-only. A new crate or directory takes the licence of its
  parent unless you add it to `REUSE.toml` in the same change.

## Developer Certificate of Origin

Every commit must be signed off under the [Developer Certificate of Origin 1.1](https://developercertificate.org/):

```bash
git commit -s
```

adds `Signed-off-by: Your Name <you@example.com>` matching the commit author. By signing off you
certify that you wrote the change or have the right to submit it under the licence of the files
you touched. That is the whole agreement — there is no CLA and no copyright assignment; you keep
your copyright. CI (`tools/release/check_dco.py`) rejects pull requests with unsigned commits;
`git rebase --signoff <base>` fixes an existing branch.

Use a real name or a consistent pseudonym you are prepared to stand behind, and a working
e-mail address.

## The gate

Everything below runs in CI (`.github/workflows/ci.yml`); run what your change touches locally
first:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
reuse lint                                    # pip install reuse
python3 tools/openapi-sdk/generate.py --check # after any api/openapi.json change
(cd dashboard && npm run lint && npx tsc -p tsconfig.json --noEmit && npm test && npm run build)
(cd sdk/web && npm test)
```

Conventions worth knowing:

* `api/openapi.json` is the contract: change it first, regenerate the server SDKs, then the code.
* Database changes are additive migrations under `migrations/`; a fleet of N and N+1 nodes must
  be able to share a database during a rolling upgrade.
* Protocol changes to AURX or the WebSocket control plane get a fuzz seed under `fuzz/corpus` and
  a live E2E case (`crates/aurix-server/tests/e2e_live.rs`).
* SDKs stay at feature parity across Web, Unity, Unreal, Godot and the C ABI; a change to one
  usually means a change to all, or an explicit note in `docs/src/limitations.md`.
* User-visible behaviour goes into `CHANGELOG.md` under `[Unreleased]`.

## Pull requests

Small, focused commits with a subject that reads as a changelog line. Describe *why* in the body;
the diff already says *what*. Mark anything you could not test and how it could be tested.
