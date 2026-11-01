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
iggy-migrate drift               # compare the server with the applied migrations
iggy-migrate verify --all-envs   # replay all migrations on an empty server
iggy-migrate squash              # replace all migrations with one baseline
iggy-migrate eval <file>         # print a file's ops, without connecting
```

| Flag             | Env var                     | Default                           |
| ---------------- | --------------------------- | --------------------------------- |
| `--url`          | `IGGY_MIGRATE_URL`          | `iggy://iggy:iggy@localhost:8090` |
| `--dir`          | `IGGY_MIGRATE_DIR`          | `migrations`                      |
| `--env`          | `IGGY_MIGRATE_ENV`          | `dev`                             |
| `--environments` | `IGGY_MIGRATE_ENVIRONMENTS` | `dev,staging,prod`                |
| `--rehearsal-image` | `IGGY_MIGRATE_REHEARSAL_IMAGE` | target's version (`up`), `apache/iggy:0.9.0` (`verify`) |
| `--allow-deletes` (`up`) | `IGGY_MIGRATE_ALLOW_DELETES` | off |

### Deletes

`up` refuses to run pending migrations that delete a stream, topic or
consumer group unless given `--allow-deletes`, because deleting drops its
messages and consumer offsets for good. `plan` shows those ops as `DELETE`.
Shortening `message_expiry` or lowering `max_size` can also drop messages
and is not guarded.

### CI

| Command | Exit code |
| --- | --- |
| `status` | 1 if any migration is CHANGED, PARTIAL or MISSING |
| `plan --detailed-exitcode` | 2 if there is something to apply, 0 if not |
| `drift` | 1 if the server differs from the applied migrations |

`status`, `plan` and `drift` take `--json` for machine-readable output on
stdout; errors and refusals still go to stderr as text.

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

## Drift

`drift` folds the applied migrations into the state they should have
produced (as `squash` does) and compares it with the server:

```
unmanaged  stream legacy
changed    topic orders/audit: partitions 3 (want 1)
missing    consumer_group orders/events/billing
```

`unmanaged` is on the server but created by no migration, `changed` has a
config the migrations did not set, and `missing` was created by a migration
but is gone. Like `up`, it only compares settings a migration set: a topic
left to the default expiry is not reported when its expiry changes. It
counts only applied migrations, so run it after `up` to check the whole
picture; a failed migration's partial work shows up as drift.

## Squashing (baselines)

`squash [--through <id>]` replaces every migration up to and including `<id>`
(all of them by default) with one `<timestamp>_baseline.dhall` that creates
their final state, and deletes the replaced files. For each environment it
folds the history (deletes drop what they remove, recreated resources keep
their last config), then proves in Docker that the history and the baseline
produce identical streams, topics and groups before writing anything.

The baseline records each replaced migration's checksum. On `up`, a server
that has:

- **applied all of them** marks the baseline applied without running it;
- **applied none of them** runs the baseline like any migration;
- **applied only some** is refused (`PARTIAL` in `status`). Apply the rest
  from the commit before the squash, e.g. by pointing `--dir` at a checkout
  of it, then run `up` again.

So before squashing, run `up` on every long-lived server if you want them all
to take the fast path. Baselines can themselves be squashed later: the new
baseline carries the old one's replaced migrations along, so servers that are
several squashes behind are still recognised. Don't edit generated baselines.

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

- `Op`: `CreateStream`, `DeleteStream`, `CreateTopic`, `UpdateTopic`,
  `DeleteTopic`, `CreateConsumerGroup`, `DeleteConsumerGroup`.
- `Topic::{ stream, name }`: a topic with defaults (`partitions = 1`, server
  default expiry and max size).
- `TopicUpdate::{ stream, name }`: an update that changes nothing until you
  set `partitions`, `message_expiry`, `max_size` or `compression` (see
  below).

`Topic` settings, all optional except `partitions`:

| Field | Values | Default | Updatable |
| --- | --- | --- | --- |
| `partitions` | 1 or more | 1 | grow only |
| `message_expiry` | `"7d"`, `"1h 30m"`, `"none"` | never expire | yes |
| `max_size` | `"2 GiB"`, `"unlimited"` | unlimited | yes |
| `compression` | `"none"`, `"gzip"` | none | yes |
| `segment_size` | `"256 MiB"` | 1 GiB | no |
| `durability` | `"replicated"`, `"persisted"` | replicated | no |

Iggy only allows changing expiry, max size and compression after creation,
so changing `segment_size` or `durability` means recreating the topic.
`compression` is recorded on the topic, but Iggy 0.9 does not compress
payloads yet.
- `map`, `concat`: list helpers (the Dhall Prelude is not available).
- `env.name : Text` and `env.kind : Env`, a union of `--environments`. Dhall
  cannot compare `Text`, so branch on `env.kind` with `merge`; leaving out an
  environment is a type error.

### Updating topics

```dhall
[ Op.UpdateTopic
    TopicUpdate::{
    , stream = "orders"
    , name = "events"
    , partitions = Some (merge { dev = 2, staging = 6, prod = 24 } env.kind)
    , message_expiry = Some "14d"
    }
]
```

`UpdateTopic` changes a topic in place, keeping its messages. Fields are
targets, not deltas: `partitions = Some 24` adds partitions until there are
24, and an update that already holds is skipped. Partitions can only grow,
because Iggy removes partitions by deleting them and their messages; to
shrink, delete and recreate the topic. `max_size` must be at least the
`segment_size` (1 GiB by default; `"1 GB"` is smaller than that).

Each topic may be created or updated once per migration, so a migration that
failed halfway can be rerun; put all changes to a topic in one op.

Adding partitions changes which partition a key hashes to, so per-key
ordering does not hold across the change.

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

### Concurrent runners

There is no lock: Iggy has nothing to build one on that a crashed runner
could not leave held. Run `up` from one place per server, e.g. one CI job
with a concurrency group (`concurrency:` in GitHub Actions).

If two runners do overlap on the same migrations, both see them pending and
start applying. Per op, one runner's check passes and creates the resource;
the other's create then fails with "already exists", so it stops and records
a failed event, which `status` ignores. Running `up` again reports nothing to
apply. Worse cases:

- Runners on different commits can interleave ops from different
  migrations, or apply a file one of them has since changed. The ledger keeps
  the first applied checksum, so `status` then reports CHANGED for the other.
- A delete in one runner can race a create in another, leaving the server in
  a state neither intended. `drift` shows it.
- Two runners can both mark the same baseline, which is harmless.

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
