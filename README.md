# iggy-migrate

Forward-only migrations for Apache Iggy streams, topics and consumer groups.

Migrations are [Starlark](https://github.com/bazelbuild/starlark) files that
evaluate to a list of ops. They are evaluated in a sandbox, so they cannot
read files, URLs or environment variables, and every pending migration is
evaluated and validated before anything is applied.

Applied migrations are recorded as JSON events in a ledger: by default in
Iggy itself, on the `migration/migration` topic (one partition, never
expires, unlimited size), or in a Postgres table, which also gives `up` a
lock against concurrent runners.

## Usage

```bash
iggy-migrate new create_orders   # migrations/20261101120000_create_orders.star
iggy-migrate status              # applied / pending / CHANGED / MISSING
iggy-migrate plan                # what `up` would do, without changing anything
iggy-migrate up                  # rehearse, then apply pending migrations
iggy-migrate drift               # compare the server with the applied migrations
iggy-migrate verify --all-envs   # replay all migrations on an empty server
iggy-migrate squash              # replace all migrations with one baseline
iggy-migrate eval <file>         # print a file's ops, without connecting
```

## Configuration

Settings come from, later ones winning:

1. `iggy-migrate.styx` in the working directory, or `--config <file>`
2. `iggy-migrate.secrets.styx` next to it, or `--secrets <file>`: the only
   place passwords and tokens may be set
3. environment variables `IGGY_MIGRATE__<PATH>`, with `__` between levels:
   `IGGY_MIGRATE__ENV=prod`, `IGGY_MIGRATE__LEDGER__NAMESPACE=prod-eu`
4. flags: `--url` (`iggy.url`), `--dir`, `--env`, `--environments`,
   `--rehearsal-image` (`rehearsal.image`), `--allow-deletes` on `up`

```styx
// iggy-migrate.styx
env prod
environments "dev,staging,prod"
iggy {
  url "iggy://migrate@iggy.internal:8090"
}
ledger {
  type postgres
  url "postgres://migrate@db.internal/ops?sslmode=require"
}
```

```styx
// iggy-migrate.secrets.styx (keep out of version control)
iggy {
  password hunter2
}
ledger {
  password s3cret
}
```

| Setting | Default | |
| --- | --- | --- |
| `dir` | `migrations` | directory of migration files |
| `env` | `dev` | environment being migrated, `env` in migrations |
| `environments` | `dev,staging,prod` | comma separated; `per_env` requires all of them |
| `runner` | `$USER` | recorded in the ledger as who applied each migration |
| `allow_deletes` | `false` | let `up` apply deletes |
| `iggy.url` | `iggy://iggy@localhost:8090` | connection string without the password |
| `iggy.password` | | secret: the password for the user in `iggy.url` |
| `iggy.token` | | secret: a personal access token instead (`iggy://host:port`) |
| `ledger.type` | `iggy` | `iggy` or `postgres` |
| `ledger.url` | | Postgres connection string without the password |
| `ledger.password` | | secret |
| `ledger.namespace` | `env` | separates servers sharing one table |
| `ledger.table` | `iggy_migrate_events` | |
| `ledger.lock_timeout` | `10m` | how long `up` waits for another runner |
| `rehearsal.image` | target's version (`up`), `apache/iggy:0.9.0` (`verify`) | |

Unknown settings, secrets set anywhere but the secrets file, and old
`IGGY_MIGRATE_*` variables are errors. In CI, write the secrets file from
your secret store before running the tool. Iggy connection strings cannot
escape `:` or `@`, so passwords containing them are refused.

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

## Postgres ledger

```styx
ledger {
  type postgres
  url "postgres://migrate@db.internal/ops?sslmode=require"
}
```

Events are stored as JSON in one table (`ledger.table`, created on first
`up`), one row per event, keyed by `ledger.namespace`, which defaults to
`env`. Give servers that share a database and an environment name their own
namespace. TLS uses the system's root certificates; `sslmode` in the URL
picks `disable`, `prefer` (the default) or `require`.

To move a server from the Iggy ledger, configure Postgres and run
`iggy-migrate ledger import` once: it copies the Iggy ledger's events into
the empty namespace, under the lock. Until then, `status` and `up` refuse a
server whose Iggy ledger has history but whose namespace is empty, rather
than treating it as new. The Iggy ledger is left in place; delete the
`migration` stream once `status` looks right.

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
including on Ctrl-C (but not when the process is killed with `kill -9`).

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
(all of them by default) with one `<timestamp>_baseline.star` that creates
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

```python
ops = [create_stream(name = "orders")] + [
    create_topic(
        stream = "orders",
        name = name,
        partitions = per_env(dev = 1, staging = 3, prod = 12),
        message_expiry = "7d",
    )
    for name in ["created", "paid", "shipped"]
] + [create_consumer_group(stream = "orders", topic = "paid", name = "billing")]
```

A file assigns its ops to `ops`. Available in every file:

- `create_stream`, `delete_stream`, `create_topic`, `update_topic`,
  `delete_topic`, `create_consumer_group`, `delete_consumer_group`. They
  take keyword arguments only, named like the settings below, and are the
  only way to make an op.
- `env`: the environment being migrated, as a string.
- `per_env(dev = .., staging = .., prod = ..)`: the value for `env`. Every
  environment in `--environments` must be given, so a missing one is an
  error in all of them, not just when that environment is migrated.

Files run in a sandbox: `load` is refused and the standard library has no
access to files, environment variables, time or randomness (and no
`print`). Starlark has no `while` and limits recursion depth, and `for`
loops must be inside a `def` or written as comprehensions. Type annotations
are checked. Mistakes are reported at the call in your file, e.g.
``Found `partitons` extra named parameter(s) for call to create_topic``.

Starlark does not guarantee that evaluation finishes, so each
file is limited to 1 MiB of source, 10 million steps, 64 MiB of heap and 5
seconds. A single built-in call such as `"x" * 10**9` cannot be interrupted
and may take a few seconds and a lot of memory before a limit stops it.

### Editor support

Generic Starlark tooling doesn't know the functions above, so it flags
`create_topic` and `env` as undefined. `iggy-migrate lsp` is a language
server that does: it completes and documents the functions, and shows the
problems `up` would refuse, at the line they happen, evaluated in every
environment (`in prod: Key "prod" was not found`). Point your editor's
Starlark language server at `iggy-migrate lsp`; it takes `--environments`
like every command.

For Zed, this repository's `.zed/settings.json` runs it in place of starpls
once `iggy-migrate` is on your PATH (`cargo install --path .`).

### Topic settings

Keyword arguments of `create_topic`, all optional (`update_topic` takes the
updatable ones):

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

### Updating topics

```python
ops = [update_topic(stream = "orders", name = "events",
                    partitions = per_env(dev = 2, staging = 6, prod = 24),
                    message_expiry = "14d")]
```

`update_topic` changes a topic in place, keeping its messages. Settings are
targets, not deltas: `partitions = 24` adds partitions until there are 24, and an update that already holds is skipped. Partitions can only grow,
because Iggy removes partitions by deleting them and their messages; to
shrink, delete and recreate the topic. `max_size` must be at least the
`segment_size` (1 GiB by default; `"1 GB"` is smaller than that).

Each topic may be created or updated once per migration, so a migration that
failed halfway can be rerun; put all changes to a topic in one call.

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

With the Postgres ledger, `up` holds a session-level advisory lock on its
namespace from before it reads the ledger until it exits. Other runners wait,
saying who holds it (`waiting for iggy-migrate (ci-4) from 10.0.0.7, …`), for
up to `ledger.lock_timeout`, then read the ledger fresh, so they apply only
what is left. The lock belongs to the database connection, so a runner that
crashes or is killed releases it at once. `status`, `plan` and `drift` don't
take it.

With the Iggy ledger there is no lock: Iggy has nothing to build one on that a
crashed runner could not leave held. Run `up` from one place per server, e.g.
one CI job with a concurrency group (`concurrency:` in GitHub Actions). If two
runners do overlap on the same migrations, both see them pending and
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

## Moving from Dhall

Dhall migrations are no longer supported, and `.dhall` files in the
migrations directory are an error. Checksums are computed from the ops a file
evaluates to, not its source, so rewrite each one as a `.star` file with the
same id that produces the same ops in every environment; it stays applied.
Check with `status` (a mistake shows as CHANGED), or compare `eval` output
against the checksums in the ledger. Generated baselines can be rewritten the
same way, with `replaces` as a list of `{"id", "checksum", "parent"}` dicts.
