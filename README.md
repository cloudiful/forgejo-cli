## `fj`, a CLI client for Forgejo

[<img src="https://repology.org/badge/vertical-allrepos/forgejo-cli.svg" alt="Packaging status" align="right">](https://repology.org/project/forgejo-cli/versions)

... akin to `gh`, `glab`, or `tea`!

> Independent fork maintained at [cloudiful/forgejo-cli](https://github.com/cloudiful/forgejo-cli),
> based on upstream [forgejo-contrib/forgejo-cli](https://codeberg.org/forgejo-contrib/forgejo-cli) `0.7.0`.
> Fork releases use `vX.Y.Z-cloudiful.N` (current `0.7.0-cloudiful.4`); see
> [docs/ci-release.md](docs/ci-release.md). Not officially endorsed by upstream.

[![Matrix chat badge](https://img.shields.io/badge/chat-on_matrix-brightgreen)](https://matrix.to/#/#fj-cli:cartoon-aa.xyz)
[![Liberapay receiving badge](https://img.shields.io/liberapay/receives/forgejo-cli)](https://liberapay.com/forgejo-cli/)

You can...

- Open, edit, comment on, close issues
- Create and merge pull requests
  - Easily create AGit pull requests, no need to fork!
- Create, star, watch, and edit repositories
- Manage organizations and teams
- Publish new releases
- List workflow runs and read their CI logs

all from the command line! Check out [the wiki](https://codeberg.org/forgejo-contrib/forgejo-cli/wiki) for more details and explanations.

See [docs/attachments.md](docs/attachments.md) for PR attachment uploads, with
`fj pr create --attach` and `fj pr attach`, including local references in the
body.

See [docs/actions.md](docs/actions.md) for Actions run listing and CI logs.

`fj` doesn't try to replace your usage of `git`, it's meant to work alongside it. 
It handles all the Forgejo-specific things that `git` doesn't.

[Matrix Chat](https://matrix.to/#/#forgejo-cli:cartoon-aa.xyz)

### Installation

Pre-built binaries are available for the four native platforms on the
[releases tab](https://github.com/cloudiful/forgejo-cli/releases/latest).

See [docs/ci-release.md](docs/ci-release.md) for exact assets, triggers and verification.

See [the wiki page on installation](https://codeberg.org/forgejo-contrib/forgejo-cli/wiki/Installation) for more options.

### Licensing

This project is licensed under either
[Apache License Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT)
at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

