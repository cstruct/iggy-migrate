# Changelog

All notable changes to iggy-migrate. Versions follow
[Semantic Versioning](https://semver.org).

## Unreleased

### Breaking

- `status` and `drift` exit 2 instead of 1 when they find something, like
  `plan --detailed-exitcode`, so CI can tell findings from errors. Invalid
  arguments exit 1 instead of 2.
- Ledgers with `accepted` or `forgotten` events, written by `ledger accept`
  and `ledger forget`, can't be read by 0.1. Upgrade every runner before
  repairing a ledger.

### Added

- Static Linux binaries, built with musl, for x86_64 and arm64. They run on
  any distribution, Alpine included.
- A Docker image, `ghcr.io/cstruct/iggy-migrate`, for amd64 and arm64, with
  the docker CLI for rehearsals. See `docs/docker.md`.
- `ledger history` lists every ledger event, failed runs and their errors
  included, and `ledger show <id>` prints everything recorded for one
  migration. Both take `--json`.
- `ledger accept <id>` records a migration as applied with its current
  checksum without running it, for CHANGED migrations whose edit was intended
  and pending ones already applied by hand. `ledger forget <id>` stops
  counting a migration as applied, for MISSING migrations whose file was
  removed on purpose. Both take `--reason`, recorded in the ledger.
- The MIT license.
- Documentation in `docs/`, a contributing guide and this changelog. Release
  archives include the license and changelog.

### Fixed

- An error reading the Iggy ledger, such as a lack of permissions, no longer
  reads as an empty ledger that makes every migration look pending.
- The refusal to apply deletes names `IGGY_MIGRATE__ALLOW_DELETES`, not the
  unsupported `IGGY_MIGRATE_ALLOW_DELETES`.
- `up` lines up its ops like `plan` does.

### Removed

- The error for `.dhall` files in the migrations directory, and the guide to
  moving from Dhall. No release supported Dhall; such files are now ignored
  like any other non-`.star` file.

## 0.1.1 - 2026-11-02

### Added

- `iggy.connect_timeout` (default `10s`): commands give up reaching the Iggy
  server after it, naming the URL, instead of hanging silently.

### Fixed

- `plan` showed the existing topics of a stream that a pending migration
  creates, though it already exists, as `apply`; `up` skipped them.

## 0.1.0 - 2026-11-02

The first release: forward-only Starlark migrations for Iggy streams, topics
and consumer groups, with `status`, `plan`, `up` (rehearsed in Docker),
`drift`, `verify`, `squash`, `eval`, `new` and a language server; a ledger in
Iggy or Postgres, with a lock for concurrent runners on Postgres; and
binaries for Linux and macOS.
