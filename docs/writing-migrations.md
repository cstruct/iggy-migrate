# Writing migrations

A migration is a [Starlark](https://github.com/bazelbuild/starlark) file in the
migrations directory (`migrations` by default), named
`<YYYYMMDDHHMMSS>_<snake_case_name>.star`. `iggy-migrate new <name>` creates
one with the current time. Migrations are applied in timestamp order.

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
  environment in `environments` must be given, so a missing one is an
  error in all of them, not just when that environment is migrated.

Every migration file is evaluated and validated before anything is applied,
so a mistake in any of them stops the run before the server is touched. `iggy-migrate eval <file>` prints a file's ops and
checksum without connecting to a server.

Migrations are forward-only: to undo one, write a new migration that does the
opposite. Once applied, a migration must not change; see
[Safety](safety.md#guarantees).

## The sandbox

Files run in a sandbox: `load` is refused and the standard library has no
access to files, environment variables, time or randomness (and no
`print`). Starlark has no `while` and limits recursion depth, and `for`
loops must be inside a `def` or written as comprehensions. Type annotations
are checked. Mistakes are reported at the call in your file, e.g.
``Found `partitons` extra named parameter(s) for call to create_topic``.

Starlark does not guarantee that evaluation finishes, so each file is limited
to 1 MiB of source, 10 million steps, 64 MiB of heap and 5 seconds. A single
built-in call such as `"x" * 10**9` cannot be interrupted and may take a few
seconds and a lot of memory before a limit stops it.

## Topic settings

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

## Updating topics

```python
ops = [update_topic(stream = "orders", name = "events",
                    partitions = per_env(dev = 2, staging = 6, prod = 24),
                    message_expiry = "14d")]
```

`update_topic` changes a topic in place, keeping its messages. Settings are
targets, not deltas: `partitions = 24` adds partitions until there are 24, and
an update that already holds is skipped. Partitions can only grow, because
Iggy removes partitions by deleting them and their messages; to shrink,
delete and recreate the topic. `max_size` must be at least the
`segment_size` (1 GiB by default; `"1 GB"` is smaller than that).

Each topic may be created or updated once per migration, so a migration that
failed halfway can be rerun; put all changes to a topic in one call.

Adding partitions changes which partition a key hashes to, so per-key
ordering does not hold across the change.

## Deleting

`delete_stream`, `delete_topic` and `delete_consumer_group` drop the messages
and consumer offsets in what they delete, for good, so `up` refuses to apply
them without `--allow-deletes`. See [Commands](commands.md#deletes).

## Editor support

Generic Starlark tooling doesn't know the functions above, so it flags
`create_topic` and `env` as undefined. `iggy-migrate lsp` is a language
server that does: it completes and documents the functions, and shows the
problems `up` would refuse, at the line they happen, evaluated in every
environment (`in prod: Key "prod" was not found`). Point your editor's
Starlark language server at `iggy-migrate lsp`; it takes `--environments`
like every command.

For Zed, the Starlark extension's `buck2-lsp` slot accepts a binary override.
In your project's `.zed/settings.json`, with `iggy-migrate` on your `PATH`:

```json
{
  "languages": {
    "Starlark": {
      "language_servers": ["buck2-lsp", "!starpls", "!tilt", "..."]
    }
  },
  "lsp": {
    "buck2-lsp": {
      "binary": {
        "path": "iggy-migrate",
        "arguments": ["lsp"]
      }
    }
  }
}
```
