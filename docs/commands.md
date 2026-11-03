# Commands

```bash
iggy-migrate new create_orders   # migrations/20261101120000_create_orders.star
iggy-migrate status              # applied / pending / CHANGED / PARTIAL / MISSING
iggy-migrate plan                # what `up` would do, without changing anything
iggy-migrate up                  # rehearse, then apply pending migrations
iggy-migrate drift               # compare the server with the applied migrations
iggy-migrate verify --all-envs   # replay all migrations on an empty server
iggy-migrate squash              # replace all migrations with one baseline
iggy-migrate eval <file>         # print a file's ops, without connecting
iggy-migrate lsp                 # language server for editors
iggy-migrate ledger history      # every recorded run, failures included
iggy-migrate ledger show <id>    # everything recorded for one migration
iggy-migrate ledger accept <id>  # record a migration as applied, without running it
iggy-migrate ledger forget <id>  # forget that a migration was applied
iggy-migrate ledger import       # copy the Iggy ledger into Postgres, once
```

Every command takes `--help`, and the global flags `--config`, `--secrets`,
`--url`, `--dir`, `--env`, `--environments` and `--rehearsal-image`; see
[Configuration](configuration.md).

| Command | Does |
| --- | --- |
| `new <name>` | Creates an empty migration named with the current time. |
| `status` | Lists each migration as applied, pending, CHANGED (edited since it was applied), PARTIAL (a baseline this server is partway through) or MISSING (applied, but the file is gone). |
| `plan` | Dry-runs the pending migrations against the server and shows each op as `apply`, `skip` (with why) or `DELETE`. |
| `up` | Rehearses the pending migrations in Docker, then applies them. `--skip-rehearsal` applies without Docker; `--allow-deletes` allows deletes. |
| `drift` | Compares the server with what the applied migrations should have produced. See [Safety](safety.md#drift). |
| `verify` | Replays every migration onto an empty server in Docker, for `--env` or, with `--all-envs`, every environment. Needs no target server. |
| `squash` | Replaces migrations with a verified baseline. See [Squashing](squashing.md). |
| `eval <file>` | Prints a file's ops and checksum for `--env`, without connecting. |
| `lsp` | Runs a language server on stdin and stdout. See [Writing migrations](writing-migrations.md#editor-support). |
| `ledger …` | Reads and repairs the ledger. See [The ledger](ledger.md). |

`status`, `plan`, `up`, `drift` and `ledger accept` and `forget` refuse to
continue when any migration file is invalid. `ledger history` and `show` only
read the ledger, so they work regardless.

## Deletes

`up` refuses to run pending migrations that delete a stream, topic or
consumer group unless given `--allow-deletes` (or `allow_deletes true` in the
config, or `IGGY_MIGRATE__ALLOW_DELETES=true`), because deleting drops its
messages and consumer offsets for good. `plan` shows those ops as `DELETE`.
Shortening `message_expiry` or lowering `max_size` can also drop messages
and is not guarded.

## Exit codes

Every command exits 0 on success and 1 on an error, including invalid
arguments. Exit code 2 means a command ran and found something:

| Command | Exit code 2 |
| --- | --- |
| `status` | a migration is CHANGED, PARTIAL or MISSING |
| `plan --detailed-exitcode` | there is something to apply |
| `drift` | the server differs from the applied migrations |

`plan` and `up` exit 1 rather than 2 when a migration is CHANGED, PARTIAL or
MISSING, since they refuse to continue.

## JSON output

`status`, `plan`, `drift`, `ledger history` and `ledger show` take `--json` for
machine-readable output on stdout. Errors and refusals still go to stderr as
text.

## In CI

A typical pipeline checks every pull request and applies on merge:

```bash
# On every pull request: the history applies from scratch, in every environment
iggy-migrate verify --all-envs

# Before deploying: nothing was edited, and the server matches its history
iggy-migrate --env prod status
iggy-migrate --env prod drift
iggy-migrate --env prod plan

# Deploy
iggy-migrate --env prod up
```

Write the secrets file from your secret store before running the tool, and run
`up` from one place per server; see [Concurrent runners](safety.md#concurrent-runners).
