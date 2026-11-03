# Contributing

Bug reports, ideas and pull requests are welcome. For anything larger than a
fix, open an issue first so we can agree on the approach.

## Setup

The toolchain is pinned in `mise.toml`. With [mise](https://mise.jdx.dev):

```bash
mise install
```

This installs the Rust version CI uses, with rustfmt and clippy,
cargo-deny and actionlint. Docker is needed for rehearsals, `verify` and `squash`.

## Checks

CI runs these on every pull request, and again on a release's tagged commit,
but not on pushes to `main`; run them before pushing:

```bash
mise run lint   # formatting, cargo-deny, clippy and actionlint
mise run test   # the tests, and the example migrations in every environment
```

cargo-deny checks the dependencies for security advisories, licenses
(permissive only) and sources against `deny.toml`. A new advisory that doesn't
apply goes in its `ignore` list, with why.

## Local server

The same settings the rehearsal containers use:

```bash
docker run -d --name iggy --security-opt seccomp=unconfined -p 8090:8090 \
  -e IGGY_TCP_ADDRESS=0.0.0.0:8090 -e IGGY_NODE_ADVERTISED_ADDRESS=127.0.0.1 \
  -e IGGY_SHARDING_CPU_ALLOCATION=2 -e IGGY_SHARDING_PIN_CORES=false \
  -e IGGY_ROOT_USERNAME=iggy -e IGGY_ROOT_PASSWORD=iggy apache/iggy:0.9.0
```

The server needs io_uring (hence `seccomp=unconfined`) and cannot pin cores
under Docker Desktop. Then, with `iggy { password iggy }` in
`iggy-migrate.secrets.styx`:

```bash
cargo run -- status
```

For the Postgres ledger:

```bash
docker run -d --name iggy-migrate-pg -p 5432:5432 -e POSTGRES_USER=migrate \
  -e POSTGRES_PASSWORD=pg -e POSTGRES_DB=ops postgres:17
```

with `ledger { type postgres url "postgres://migrate@localhost/ops?sslmode=disable" }`
in the config and `ledger { password pg }` in the secrets file.

## Editor

This repository's `.zed/settings.json` runs `iggy-migrate lsp` for the
migrations in `migrations/`, in place of starpls, once `iggy-migrate` is on
your `PATH` (`cargo install --path .`).

## Compatibility rules

Some things must not change, or servers migrated by earlier versions break:

- **Checksums.** They hash the JSON of the evaluated ops, so the serialization
  of `Op` is frozen: a new field must be left out of the JSON when unset
  (`skip_serializing_if`), and existing fields and variants keep their names.
  `checksums_are_stable` in `src/star.rs` guards this.
- **Ledger events.** New fields must be optional, so older events still parse.
  A new status or required field means older versions can't read the ledger;
  say so in the changelog.

## Changelog

Every user-visible change gets a line in [CHANGELOG.md](CHANGELOG.md), under
`## Unreleased`, in the same pull request. Group entries under `### Breaking`,
`### Added`, `### Changed`, `### Fixed` or `### Removed`, and write them for
users: what changed for them, not how it was done. Changes that need users to
do something, such as a different exit code or a new ledger event, go under
Breaking.

## Releasing

From a clean checkout of `main`:

```bash
mise run release:bump 0.2.0
```

This pulls `main`, sets the version in `Cargo.toml` and `Cargo.lock`, moves
the changelog's `## Unreleased` entries under `## 0.2.0 - <today>`, commits
"Release 0.2.0" on a new `release/v0.2.0` branch, pushes it, opens a pull
request for it and switches back to `main`. On any other branch it commits
"Release 0.2.0" there instead and pushes nothing, so the bump goes out with
that branch's pull request. Once the pull request with the bump is merged:

```bash
mise run release:tag
```

This pulls `main` and tags it `v0.2.0`, the version in `Cargo.toml`, and
pushes the tag. It refuses if that version is already tagged, as it is until
the pull request is merged. The tag starts the release workflow, which runs CI on the tagged commit, builds
binaries for Linux (x86_64 and arm64, glibc 2.35 and later, and static musl)
and macOS (arm64), and publishes them with `SHA256SUMS` as a GitHub release,
with the version's changelog entries as its notes. It also pushes the Docker
image, with the musl binaries copied in, to `ghcr.io/cstruct/iggy-migrate`
as `<version>`, and for releases also as `<major>.<minor>` and `latest`.

`docker build .` builds the image from source, as CI does on every pull
request.

A version with a suffix, such as `0.2.0-rc.1`, is published as a prerelease.
It leaves the changelog alone, and its release notes are generated from the
pull requests since the last release. The task refuses a dirty working tree,
a detached `HEAD`, anything that isn't a version, and a release with nothing
under `## Unreleased`.
