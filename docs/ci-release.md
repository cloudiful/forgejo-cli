# CI and release contract (GitHub-native)

This fork retains only GitHub Actions for checks and binary releases.
Woodpecker pipelines are removed. No Forgejo workflow, Docker publishing,
mirror-configuration job, or automatic latest prerelease exists.

Release source and destination: `https://github.com/cloudiful/forgejo-cli`.
`Cargo.toml` `package.repository` points there. The user manages the existing
Forgejo push mirror to that URL; no agent mirror operation is part of this task.

Current fork version: `0.7.0-cloudiful.2` (upstream `0.7.0` baseline plus
cloudiful revision `2`). Next fork patch is `0.7.0-cloudiful.3`; an upstream
`0.8.0` sync becomes `0.8.0-cloudiful.1`. There is no independent evolving
major scheme. Existing upstream tags are immutable and never retagged.

## Workflows

- `.github/workflows/ci.yml`: push to any branch (`branches: ["**"]`),
  `pull_request`, and `workflow_call` for reuse from tag releases.
  Single `checks` job covers `cargo check`, `cargo fmt --check`,
  `cargo clippy --all-targets --locked -- -D warnings`, `cargo test`,
  `cargo run --locked -- completion bash`, and pinned `typos`.
  Top-level and job permissions are `contents: read` only.
- `.github/workflows/release.yml`: triggered only by pushed `v*` tags.
  Validation accepts only strict `vX.Y.Z-cloudiful.N`. No `pull_request`,
  branch push, `workflow_dispatch`, or `schedule` trigger.
  No `pull_request_target`. No Forgejo action references.
  All external actions are pinned to version tags (`v1`/`v2`/`v3`/`v4`),
  never `@main`.

## Native platforms

| Runner | Target | Asset |
|---|---|---|
| `ubuntu-24.04` | `x86_64-unknown-linux-gnu` | `forgejo-cli-x86_64-linux.tar.gz` |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` | `forgejo-cli-aarch64-linux.tar.gz` |
| `windows-2022` | `x86_64-pc-windows-msvc` | `forgejo-cli-x86_64-windows.zip` |
| `macos-14` | `aarch64-apple-darwin` | `forgejo-cli-aarch64-macos.tar.gz` |

Runners are GitHub-hosted images with the documented CPU architectures.
Each build verifies `rustc -vV` host equals the matrix target and fails
rather than silently cross-compiling. No Linux cross toolchains, mingw
(`-gnu` Windows), or macOS SDK cross-build workarounds are installed.
Windows is MSVC, not GNU. macOS is native arm64.

Build command for every target (no `update-check` feature):

```sh
cargo build --locked --release --target <target>
```

Linux/macOS packaging uses explicit `bash` + `tar`:

```sh
tar -czf "<asset>" -C "target/<target>/release" fj -C "$GITHUB_WORKSPACE" LICENSE-MIT LICENSE-APACHE
```

Windows packaging uses explicit `pwsh` + `Compress-Archive` with explicit
paths for `fj.exe`, `LICENSE-MIT`, and `LICENSE-APACHE`.
Every archive has the binary (`fj` or `fj.exe`) at root plus both licenses.
Asset names are unchanged from P1; download URLs carry the full fork suffix.

## Gates and publication

1. `validate`: only `^vX.Y.Z-cloudiful.N$`
   (`^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-cloudiful\.([1-9][0-9]*)$`).
   Rejects bare `v0.7.0`, other fork suffixes, `cloudiful.0`,
   leading-zero `cloudiful.01`, leading-zero version components,
   malformed tags, other prerelease forms, and build metadata
   (for example `v0.7.0+build`). Compares the full tag version to
   `Cargo.toml` `package.version` via `cargo metadata` and to `flake.nix`
   `version` before any build or publish. Mismatch fails the run.
   Example: `v0.7.0-cloudiful.2` matches `0.7.0-cloudiful.2`;
   `v0.7.0-cloudiful.3` would mismatch current files.
2. `checks`: reusable call to `ci.yml`, so tag commits run the same gates
   without duplicated check definitions.
3. `build`: four native jobs (fail-fast). Each uploads its named asset via
   `actions/upload-artifact@v4` with `if-no-files-found: error`.
4. `collect` (read-only, `contents: read`): downloads all `forgejo-cli-*`
   artifacts merged into `dist/`, verifies exactly the four expected names
   and count, generates `SHA256SUMS` over the four archives only, verifies
   with `sha256sum -c`, and uploads `release-assets` (four archives +
   `SHA256SUMS`). A missing or failed build cannot produce a partial release.
5. `publish` (the only `contents: write` job, `needs` all gates):
   downloads `release-assets` and publishes with
   `softprops/action-gh-release@v3`, automatic `GITHUB_TOKEN`,
   `files: release-dist/*`, `fail_on_unmatched_files: true`,
   `draft: false`, `prerelease: false`, `make_latest: true`,
   `generate_release_notes: true`. `make_latest: true` is GitHub
   distribution policy for approved fork mainline tags, not a SemVer claim.
   No private token extraction or custom secret. Backport `latest` policies
   are out of scope; do not generate backport tags.

## cargo-binstall

Base metadata resolves Linux/Windows from the confirmed repository:

```toml
[package.metadata.binstall]
pkg-url = "{ repo }/releases/download/v{ version }/{ name }-{ target-arch }-{ target-family }{ archive-suffix }"
bin-dir = "{ bin }{ binary-ext }"
```

- `x86_64-unknown-linux-gnu` → `forgejo-cli-x86_64-linux.tar.gz`
- `aarch64-unknown-linux-gnu` → `forgejo-cli-aarch64-linux.tar.gz`
- `x86_64-pc-windows-msvc` → `forgejo-cli-x86_64-windows.zip` (via `pkg-fmt = "zip"`)

`target-family` for `aarch64-apple-darwin` is `darwin` (from
`target_lexicon::OperatingSystem`), not `macos`, so the base template alone
would not match the fixed macOS asset. Explicit override:

```toml
[package.metadata.binstall.overrides.aarch64-apple-darwin]
pkg-url = "{ repo }/releases/download/v{ version }/{ name }-aarch64-macos{ archive-suffix }"
```

→ `forgejo-cli-aarch64-macos.tar.gz`. Verified URLs for `0.7.0-cloudiful.1`:

- `https://github.com/cloudiful/forgejo-cli/releases/download/v0.7.0-cloudiful.1/forgejo-cli-x86_64-linux.tar.gz`
- `https://github.com/cloudiful/forgejo-cli/releases/download/v0.7.0-cloudiful.1/forgejo-cli-aarch64-linux.tar.gz`
- `https://github.com/cloudiful/forgejo-cli/releases/download/v0.7.0-cloudiful.1/forgejo-cli-x86_64-windows.zip`
- `https://github.com/cloudiful/forgejo-cli/releases/download/v0.7.0-cloudiful.1/forgejo-cli-aarch64-macos.tar.gz`

No dependency or binary-name changes were made for this.

## Version parity

Full fork version must be equal in `Cargo.toml` (`0.7.0-cloudiful.2`),
`Cargo.lock` root `forgejo-cli` entry (`0.7.0-cloudiful.2`,
dependencies unchanged), `flake.nix` (`0.7.0-cloudiful.2`), the full tag
(`v0.7.0-cloudiful.2`), and docs. Each fork patch increments `N`;
an upstream sync changes `X.Y.Z` and resets `N` to `1`. SemVer treats the
`-cloudiful.N` suffix as prerelease; that is documented honestly and does
not change the GitHub `prerelease: false` distribution setting above.

## What is not released

- No container image: `Dockerfile` stays as a supported source artifact,
  not active CI. No Kaniko/publish-container step.
- No `.deb`/`.msi`/`.dmg`: apt-at-home handles Debian packaging separately.
- No Nix CI publish: `flake.nix` stays as source; only its version and
  homepage are maintained for parity.
- Existing `.forgejo` issue templates remain except the PR template, which
  now carries this fork's independent contribution policy.

## Limitations

- Default fork releases do not enable the `update-check` feature because the
  current `src/version.rs` checks upstream (`https://codeberg.org/`,
  `forgejo-contrib/forgejo-cli` at upstream `0.7.0`) and would falsely rank
  the fork as older. Source feature and implementation are untouched; there
  is no new fork updater to claim. `fj version` without `--check` reports
  the fork version locally with no network call.
- Local validation covers native Linux build, packaging layout, checksums,
  YAML lint, and static matrix/trigger/permission assertions. Remote
  Windows/ARM/macOS builds are covered by runner/host-tool contracts and
  require authorized remote CI to execute.
- `typos` runs in CI via pinned `crate-ci/typos@v1` using `.typos.toml`;
  if no managed `typos` binary exists locally, that local gate is reported
  as a prerequisite rather than editing the source dictionary to hide errors.
