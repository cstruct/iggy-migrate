# iggy-migrate

> [!WARNING]
> **This project is built with heavy use of LLMs.** Most of its code, tests and
> documentation were written by an AI coding assistant (Claude), directed by
> the maintainer. Review it as you would any unfamiliar dependency before you
> trust it with servers that hold data you care about, and please
> [report](https://github.com/cstruct/iggy-migrate/issues) anything that looks
> wrong.

Version-controlled, reviewable changes to [Apache Iggy](https://iggy.apache.org)
streams, topics and consumer groups.

When Iggy resources are created by hand, by application startup code or by
one-off scripts, every environment slowly becomes its own: staging has 3
partitions where prod has 12, nobody knows who changed an expiry or when, and
deleting a topic with all its messages is one typo away. iggy-migrate treats
them the way you treat a database schema. You write small migration files,
review them in pull requests, and apply them the same way to every
environment, with a record of what ran where.

## Why iggy-migrate

- **See it before it happens.** `plan` shows what `up` would do to the live
  server, and `plan --detailed-exitcode` turns that into a CI gate.
- **Rehearsed on a real server.** Before touching the target, `up` copies its
  streams, topics and consumer groups into a throwaway Iggy server in Docker,
  running the same version, and applies the migrations there first. The
  server's own validation catches mistakes before your data is at stake.
- **Deletes are guarded.** Deleting a stream, topic or consumer group drops
  its messages and offsets for good, so `up` refuses unless given
  `--allow-deletes`.
- **One file for every environment.** `per_env(dev = 1, staging = 3, prod = 12)`
  must name every environment, so a forgotten value fails in dev, not in prod.
- **Safe to rerun.** Each op checks the server first: what already exists
  with the same config is skipped, so a migration that failed halfway can be
  fixed and run again.
- **Tamper-evident history.** Every run is recorded in a ledger, in Iggy
  itself or in Postgres (which also locks out concurrent runners). Editing or
  deleting an applied migration is caught, and `ledger history` shows who
  applied what, when, including failures.
- **Drift detection.** `drift` finds what was created, changed or deleted on
  the server behind the migrations' back.
- **Sandboxed, typed migrations.** Migrations are
  [Starlark](https://github.com/bazelbuild/starlark), a small Python dialect,
  evaluated without access to files, the network or the environment. A
  language server gives your editor completion and errors.
- **History that stays small.** `squash` folds old migrations into one
  baseline, after proving in Docker that both produce the same server.

## A quick look

```python
# migrations/20261103100000_create_payments.star
ops = [
    create_stream(name = "payments"),
    create_topic(
        stream = "payments",
        name = "events",
        partitions = per_env(dev = 1, staging = 3, prod = 12),
        message_expiry = "7d",
    ),
    create_consumer_group(stream = "payments", topic = "events", name = "ledger"),
]
```

```console
$ iggy-migrate --env prod plan
20261103100000_create_payments
  apply                              CreateStream { name: "payments" }
  apply                              CreateTopic { stream: "payments", name: "events", partitions: 12, message_expiry: Some("7d"), max_size: None, compression: None, segment_size: None, durability: None }
  apply                              CreateConsumerGroup { stream: "payments", topic: "events", name: "ledger" }

$ iggy-migrate --env prod up
starting rehearsal server (apache/iggy:0.9.0)
rehearsal passed on a copy of 0 resource(s): 1 migration(s), 3 op(s) applied, 0 skipped

20261103100000_create_payments
  apply                              CreateStream { name: "payments" }
  ...

$ iggy-migrate --env prod status
applied   20261103100000_create_payments  (2026-11-03T09:35:49Z by ci)
```

## Install

Download a binary from the
[releases](https://github.com/cstruct/iggy-migrate/releases) for Linux
(x86_64 and arm64, glibc 2.35 or later) or macOS (arm64), and check it
against `SHA256SUMS`. Or build it with Rust:

```bash
cargo install --locked --git https://github.com/cstruct/iggy-migrate
```

`up`, `verify` and `squash` start throwaway Iggy servers, so they need
Docker. `up --skip-rehearsal` applies without it.

## Getting started

Point iggy-migrate at your server in `iggy-migrate.styx`, and put the password
in `iggy-migrate.secrets.styx` next to it, kept out of version control:

```styx
// iggy-migrate.styx
env dev
iggy {
  url "iggy://migrate@localhost:8090"
}
```

```styx
// iggy-migrate.secrets.styx
iggy {
  password hunter2
}
```

Then create a migration, check what it would do, and apply it:

```bash
iggy-migrate new create_payments   # migrations/<timestamp>_create_payments.star
iggy-migrate plan
iggy-migrate up
```

## Documentation

- [Writing migrations](docs/writing-migrations.md): the functions, topic
  settings, updates, the sandbox and editor support
- [Commands](docs/commands.md): every command, deletes, exit codes and JSON
  output for CI
- [Configuration](docs/configuration.md): files, secrets, environment
  variables and every setting
- [Safety](docs/safety.md): guarantees, rehearsals, drift and concurrent
  runners
- [The ledger](docs/ledger.md): Iggy or Postgres, history, and repairing it
- [Squashing](docs/squashing.md): replacing old migrations with a baseline

## Compatibility

Built against the `iggy` SDK 0.11.0 and tested with Iggy server 0.9.0.
See the [changelog](CHANGELOG.md) for what changed in each release.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
