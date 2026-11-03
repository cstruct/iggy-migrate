# The ledger

Every run of a migration is recorded as a JSON event in an append-only ledger:
applied, failed, baselines marked applied, and repairs. `status`, `plan`, `up`
and `drift` read it to tell what a server has applied. Events are never
changed or removed; repairs are new events.

## Where it is kept

By default the ledger is in Iggy itself, on the `migration/migration` topic
(one partition, never expires, unlimited size), created on the first `up`.
Migrations cannot touch the `migration` stream.

Or it is in a Postgres table, which also gives `up` a lock against
concurrent runners (see [Concurrent runners](safety.md#concurrent-runners)):

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
picks `disable`, `prefer` (the default) or `require`. Set the password in the
secrets file, as `ledger.password`.

### Moving from the Iggy ledger to Postgres

Configure Postgres and run `iggy-migrate ledger import` once: it copies the
Iggy ledger's events into the empty namespace, under the lock. Until then,
every command that reads the ledger refuses a server whose Iggy ledger has
history but whose namespace is empty, rather than treating it as new. The Iggy
ledger is left in place; delete the `migration` stream once `status` looks
right.

## Reading the ledger

`ledger history` lists every event, oldest first, with failed runs' errors and
repairs' reasons:

```console
$ iggy-migrate ledger history
2026-11-03T09:34:32Z  applied    20261101120000_create_orders  by ci
2026-11-03T09:34:32Z  applied    20261102090000_longer_expiry  by ci
2026-11-03T09:34:32Z  accepted   20261102090000_longer_expiry  by ci
                                 reason: expiry raised by hand during incident 42
2026-11-03T09:34:33Z  failed     20261103080000_add_audit  by ci
                                 error: topic orders/audit already exists with different config: partitions 3 (want 1)
```

`ledger show <id>` prints everything recorded for one migration: each event's
environment, runner, checksum, duration, tool version, reason or error, and
the ops it ran. Both take `--json`, and both read only the ledger, so they
work even when a migration file is invalid.

## Repairing the ledger

`status` and `up` refuse to continue when a migration is CHANGED (its file was
edited after it was applied) or MISSING (its file is gone). Usually the fix is
to restore the file and write a new migration for the change. When the edit or
the removal was intended, record that instead, with a reason that ends up in
the ledger:

```bash
iggy-migrate ledger accept 20261102090000_longer_expiry --reason "expiry raised by hand during incident 42"
iggy-migrate ledger forget 20261102090000_longer_expiry --reason "folded into create_orders"
```

`ledger accept <id>` records a migration as applied with the checksum its file
has now, without running it. Use it for:

- a CHANGED migration whose new contents match what the server has, for
  instance after the change was made by hand;
- a pending migration whose changes were already made by hand, so `up` should
  not run it;
- a PARTIAL baseline, when you have brought the server to the baseline's state
  some other way.

`ledger forget <id>` makes the ledger stop counting a migration as applied. A
migration whose file is gone is no longer MISSING; one whose file is still
there is pending again, so the next `up` runs it.

Both only write to the ledger: neither changes anything on the server. Check
the result with `status` and `drift`. In particular, a forgotten migration
that runs again may fail if later migrations have since changed what it
creates, because its ops find the resources with a different config.

Both use the current `--env`, take the Postgres lock like `up`, and refuse to
run against a server migrated as another environment. The ledger keeps the
events they replace, so `ledger show` still has the full story.

## Event format

Each event is a JSON object:

| Field | |
| --- | --- |
| `id` | the migration's id, its file name without `.star` |
| `status` | `applied`, `failed`, `baselined`, `accepted` or `forgotten` |
| `checksum` | `sha256:<hex>` of the evaluated ops |
| `env` | the environment it was run as |
| `ops` | the ops that ran |
| `error` | why it failed, for `failed` |
| `reason` | the `--reason` of a repair, if given |
| `replaces` | for a baseline, the ids of the migrations it replaces |
| `applied_at` | when it was recorded, RFC 3339 |
| `duration_ms` | how long the run took |
| `runner` | the `runner` setting, `$USER` by default |
| `tool_version` | the iggy-migrate version that wrote it |

A migration counts as applied from its first `applied` or `baselined` event,
so when two runners apply it at once the checksum that ran first is kept. An
`accepted` event replaces that, and a `forgotten` event removes it, until a
later event applies it again. `failed` events don't change anything.

iggy-migrate 0.1 cannot read `accepted` and `forgotten` events and refuses a
ledger that has them, so upgrade every runner before repairing a ledger.
