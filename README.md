# iggy-migrate

Forward-only migrations for Apache Iggy streams, topics and consumer groups.

Migrations are [Dhall](https://dhall-lang.org) files that evaluate to a list of
ops. They are evaluated with imports disabled, so they cannot read files, URLs
or environment variables, and every pending migration is evaluated and
validated before anything is applied.

Applied migrations are recorded as JSON events in Iggy itself, on the
`migration/migration` topic (one partition, never expires, unlimited size),
which the tool creates on first `up`.

## Usage

```bash
iggy-migrate new create_orders   # migrations/20261101120000_create_orders.dhall
iggy-migrate status              # applied / pending / CHANGED / MISSING
iggy-migrate plan                # what `up` would do, without changing anything
iggy-migrate up                  # rehearse, then apply pending migrations
iggy-migrate verify --all-envs   # replay all migrations on an empty server
iggy-migrate eval <file>         # print a file's ops, without connecting
```

| Flag             | Env var                     | Default                           |
| ---------------- | --------------------------- | --------------------------------- |
| `--url`          | `IGGY_MIGRATE_URL`          | `iggy://iggy:iggy@localhost:8090` |
| `--dir`          | `IGGY_MIGRATE_DIR`          | `migrations`                      |
| `--env`          | `IGGY_MIGRATE_ENV`          | `dev`                             |
| `--environments` | `IGGY_MIGRATE_ENVIRONMENTS` | `dev,staging,prod`                |
| `--rehearsal-image` | `IGGY_MIGRATE_REHEARSAL_IMAGE` | target's version (`up`), `apache/iggy:0.9.0` (`verify`) |

## Rehearsals (requires Docker)

`up` first applies the pending migrations to a throwaway Iggy server in
Docker, running the same version as the target, after copying the target's
current streams, topics and consumer groups into it. Only if that succeeds is
anything applied to the target. This catches what `plan` cannot: the server's
own validation, version-specific behaviour, and bugs in this tool. Use
`--skip-rehearsal` to apply without Docker.

`verify` replays the whole history onto an empty server, without connecting to
a target. It checks that an environment can be built from migrations alone, so
it fails for migrations that depend on resources created by hand; run it in CI.

Rehearsals copy metadata, not messages, and cannot catch differences in server
config (values left to the server default), permissions or capacity.
Containers are labelled `iggy-migrate=rehearsal` and removed afterwards,
including on Ctrl-C.

## Writing migrations

```dhall
let topic =
      \(name : Text) ->
        Op.CreateTopic
          Topic::{
          , stream = "orders"
          , name
          , partitions = merge { dev = 1, staging = 3, prod = 12 } env.kind
          , message_expiry = Some "7d"
          }

in  [ Op.CreateStream { name = "orders" } ]
  # map Text Op topic [ "created", "paid", "shipped" ]
```

Available in every file:

- `Op`: `CreateStream`, `DeleteStream`, `CreateTopic`, `DeleteTopic`,
  `CreateConsumerGroup`, `DeleteConsumerGroup`.
- `Topic::{ stream, name }`: a topic with defaults (`partitions = 1`, server
  default expiry and max size).
- `map`, `concat`: list helpers (the Dhall Prelude is not available).
- `env.name : Text` and `env.kind : Env`, a union of `--environments`. Dhall
  cannot compare `Text`, so branch on `env.kind` with `merge`; leaving out an
  environment is a type error.

## Guarantees

- **Safe reruns.** Each op checks current state first. Creating something that
  already exists with the same config, or deleting something already gone, is
  skipped; existing resources with different config are an error. A migration
  that failed halfway can be fixed and rerun.
- **Applied migrations are immutable.** The checksum is a hash of the evaluated
  ops. Editing or deleting an applied migration makes `status` and `up` fail.
- **Right server, right env.** The env is recorded per event; running with a
  different `--env` against the same server is refused.
- **The ledger is off limits.** Migrations cannot touch the `migration` stream.

Not handled: concurrent runners. Run `up` from one place (e.g. a single CI job).

## Local server

The same settings the rehearsal containers use:

```bash
docker run -d --name iggy --security-opt seccomp=unconfined -p 8090:8090 \
  -e IGGY_TCP_ADDRESS=0.0.0.0:8090 -e IGGY_NODE_ADVERTISED_ADDRESS=127.0.0.1 \
  -e IGGY_SHARDING_CPU_ALLOCATION=2 -e IGGY_SHARDING_PIN_CORES=false \
  -e IGGY_ROOT_USERNAME=iggy -e IGGY_ROOT_PASSWORD=iggy apache/iggy:0.9.0
```

The server needs io_uring (hence `seccomp=unconfined`) and cannot pin cores
under Docker Desktop. Built against `iggy` SDK 0.11.0 / server 0.9.0.
