# Safety

## Guarantees

- **Safe reruns.** Each op checks current state first. Creating something that
  already exists with the same config, or deleting something already gone, is
  skipped; existing resources with different config are an error. A migration
  that failed halfway can be fixed and rerun.
- **Applied migrations are immutable.** The checksum is a hash of the evaluated
  ops, so reformatting or refactoring a file leaves it unchanged, but changing
  what it does changes it. Editing or deleting an applied migration makes
  `status` and `up` fail until it is restored, or the change is recorded
  deliberately with [`ledger accept` or `forget`](ledger.md#repairing-the-ledger).
- **Right server, right env.** The env is recorded per event; running with a
  different `--env` against the same server is refused.
- **The ledger is off limits.** Migrations cannot touch the `migration` stream.

## Rehearsals

Rehearsals need Docker.

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

## Concurrent runners

With the Postgres ledger, `up`, `ledger accept` and `ledger forget` hold a
session-level advisory lock on their namespace from before they read the
ledger until they exit. Other runners wait, saying who holds it
(`waiting for iggy-migrate (ci-4) from 10.0.0.7, …`), for up to
`ledger.lock_timeout`, then read the ledger fresh, so they apply only what is
left. The lock belongs to the database connection, so a runner that crashes
or is killed releases it at once. `status`, `plan`, `drift` and
`ledger history` and `show` don't take it.

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
