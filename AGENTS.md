# AGENTS — cloudiful/forgejo-cli fork

Independent fork of upstream `forgejo-contrib/forgejo-cli` (`0.7.0` baseline).
Do not present this fork as officially endorsed by upstream.

## Origins and remotes

- Actual origin is `ssh://git@forgejo.cloud1ful.com:2224/tools/forgejo-cli.git`.
  GitHub `cloudiful/forgejo-cli` is the user-managed mirror and release origin.
  Codeberg `forgejo-contrib/forgejo-cli` is the upstream project source, not a
  configured Git remote here. Do not change remotes in routine work.
- Sync from upstream without rewriting tags: existing upstream tags are
  immutable, never retag. A fork patch increments `N` in `X.Y.Z-cloudiful.N`;
  an upstream sync changes `X.Y.Z` and resets `N` to `1`
  (`0.7.0-cloudiful.1` → `0.7.0-cloudiful.2`;
  upstream `0.8.0` → `0.8.0-cloudiful.1`).
- Never submit AI-assisted work upstream as if it complied with upstream's
  no-LLM contribution policy. Fork PR policy below applies only to this fork.

## Licenses

- `fj` itself is `Apache-2.0 OR MIT` (`LICENSE-APACHE`, `LICENSE-MIT`).
  That is independent of the Forgejo server GPL. Do not replace or remove
  either license and do not assert blanket legal immunity.
- Retain applicable third-party licenses and `NOTICE` files when present.
- Mark fork modifications in docs and release notes; avoid upstream
  trademark or endorsement claims. Codeberg hosting terms and upstream
  contribution policies are distinct from code permission.
- Do not copy foreign sources or credentials without a license audit.

## Versions

Keep these consistent for every fork release:

- `Cargo.toml` `package.version`
- `Cargo.lock` root `forgejo-cli` entry only (no dependency drift)
- `flake.nix` `version` (and `homepage` points at the fork)
- Full tag `vX.Y.Z-cloudiful.N`
- `docs/ci-release.md` examples

Runtime code, dependencies, and binary name (`fj`) stay unchanged for this
version-maintenance task. This does not permanently ban future
user-authorized features (for example attachment work) or dependencies with
normal review.

## Releases

- GitHub mirror is user-managed; agents do not configure mirrors.
- Releases publish only on strict `vX.Y.Z-cloudiful.N` tags via
  `.github/workflows/release.yml` (tag-only, four native platforms,
  host-triple check, read-only jobs except `contents:write` publish).
- Default fork builds do not enable `update-check`: current `src/version.rs`
  still points at upstream Codeberg and would misrank the fork.
- No container, `.deb`/`.msi`/`.dmg`, or backport-tag publishing.

## Validation

Standard local gates (no live publish, no tag/release writes):

```sh
git diff --check
actionlint .github/workflows/ci.yml .github/workflows/release.yml
yq eval '.' .github/workflows/ci.yml
yq eval '.' .github/workflows/release.yml
cargo fmt --check
cargo check --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo run --locked -- completion bash
cargo build --release --locked
./target/release/fj version
mise exec cargo:typos-cli@1.51.1 -- typos
```

Use inline `uv run --no-project python -c` for read-only assertions;
disposable fixture archives only under `.cache/ai/ci-release`.
