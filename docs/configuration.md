# Configuration

Settings come from, later ones winning:

1. `iggy-migrate.styx` in the working directory, or `--config <file>`
2. `iggy-migrate.secrets.styx` next to it, or `--secrets <file>`: the only
   place passwords and tokens may be set
3. environment variables `IGGY_MIGRATE__<PATH>`, with `__` between levels:
   `IGGY_MIGRATE__ENV=prod`, `IGGY_MIGRATE__LEDGER__NAMESPACE=prod-eu`
4. flags: `--url` (`iggy.url`), `--dir`, `--env`, `--environments`,
   `--rehearsal-image` (`rehearsal.image`), `--allow-deletes` on `up`

Both files are optional and written in Styx.

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

## Settings

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
| `iggy.connect_timeout` | `10s` | how long to try reaching the server before giving up |
| `ledger.type` | `iggy` | `iggy` or `postgres`, see [The ledger](ledger.md) |
| `ledger.url` | | Postgres connection string without the password |
| `ledger.password` | | secret |
| `ledger.namespace` | `env` | separates servers sharing one table |
| `ledger.table` | `iggy_migrate_events` | |
| `ledger.lock_timeout` | `10m` | how long `up` and repairs wait for another runner |
| `rehearsal.image` | target's version (`up`), `apache/iggy:0.9.0` (`verify`, `squash`) | |

Unknown settings, secrets set anywhere but the secrets file, and old
single-underscore `IGGY_MIGRATE_*` variables are errors, reported with where
each value came from.

## Secrets

Passwords and tokens may only be set in the secrets file, so they can't end
up in version control by way of the config file, or in process listings by
way of flags. In CI, write the secrets file from your secret store before
running the tool.

Iggy connection strings cannot escape `:` or `@`, so passwords containing
them are refused.
