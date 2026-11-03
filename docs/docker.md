# Docker

The image `ghcr.io/cstruct/iggy-migrate` runs the static musl binary on
Alpine, for amd64 and arm64. It is tagged with each version (`0.2.0`), and
releases also move `<major>.<minor>` (`0.2`) and `latest`; prereleases don't.

Its working directory is `/work`, so mount the directory with your
`iggy-migrate.styx` and migrations there:

```bash
docker run --rm -v "$PWD:/work" ghcr.io/cstruct/iggy-migrate status
```

Settings work as anywhere else ([Configuration](configuration.md)), so the
secrets file is read from the mounted directory, or pass environment
variables with `-e`:

```bash
docker run --rm -v "$PWD:/work" -e IGGY_MIGRATE__ENV=prod \
  ghcr.io/cstruct/iggy-migrate plan
```

Some things differ in a container:

- **Reaching the server.** `localhost` is the container itself. Use the
  server's name on your network, `host.docker.internal` for a server on your
  machine with Docker Desktop, or `--network host` on Linux.
- **Who applied it.** `$USER` isn't set in the image, so the ledger records
  `unknown` as the runner. Set `runner` in the config, or pass
  `-e IGGY_MIGRATE__RUNNER=ci`.
- **File ownership.** `new` and `squash` write migration files as root.
  Pass `--user "$(id -u):$(id -g)"` to write them as yourself.

## Rehearsals

`up` rehearses on a throwaway Iggy server that it starts with the docker CLI,
which the image includes, and then connects to on `127.0.0.1` of the Docker
host. So the container needs the Docker socket and the host's network:

```bash
docker run --rm -v "$PWD:/work" \
  -v /var/run/docker.sock:/var/run/docker.sock --network host \
  ghcr.io/cstruct/iggy-migrate up
```

The same goes for `verify` and `squash`, and it works on Linux, such as on CI
runners. Docker Desktop (macOS and Windows) forwards published ports through
its own proxy, so the container can't reach the rehearsal server even with
host networking; there, run the binary instead. Running as another user with
`--user` also needs access to the socket, e.g. with
`--group-add "$(stat -c %g /var/run/docker.sock)"`.

Without the socket, `up --skip-rehearsal` applies without rehearsing, and
`status`, `plan`, `drift` and the `ledger` commands don't need Docker at all.

## Building it

`docker build .` builds the image from source, in a musl Rust image. Releases
build its `release` target instead, which copies in the binaries from the
release archives.
