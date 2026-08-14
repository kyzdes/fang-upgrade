# The comparison point (FANG-53)

Staging (`openfang-staging`, `127.0.0.1:4201`, volume `openfang-staging-data`)
used to run an older image than production. That lag was doing real work: it
answered "does this also happen on the previous build?". It also meant three
sprints of `tests/fang/*.sh` ran against code that was not in production.

Promoting staging to the production image fixes the second problem and
destroys the first. So the comparison point moves out of the running system
and into two artefacts that are booted **on demand** and thrown away again:

| | |
|---|---|
| image | `openfang-openfang:v0.6.9-pristine` — upstream v0.6.9, no fork patches |
| volume | `openfang-staging-premigration` — staging's `/data` before the sprint-3 schema migration (`PRAGMA user_version = 8`; staging is now 9) |

`tests/fang/refstand.sh` boots them together.

```sh
tests/fang/refstand.sh up                 # ~5 s: clone volume, start, wait for /api/health
tests/fang/refstand.sh status             # container / image / clone / url / health / schema
tests/fang/refstand.sh api GET /api/agents
tests/fang/refstand.sh down               # container and clone gone; reference untouched
```

Defaults: container `openfang-refstand`, clone volume `openfang-refstand-data`,
`127.0.0.1:4202`. Override with `REFSTAND_PORT`, `REFSTAND_BIND`,
`REFSTAND_IMAGE`, `REFSTAND_REF_VOLUME`, `REFSTAND_CONTAINER`,
`REFSTAND_VOLUME`.

## Two things the script refuses to do

1. **Mount the reference volume read-write.** A v0.6.9 daemon opening the
   premigration database migrates it, and then it is no longer premigration —
   the reference destroys itself the first time you use it. `up` copies the
   volume (source mounted `:ro`) into a scratch volume and mounts the copy;
   `down` deletes the copy. Every `up` starts from the reference again, so the
   stand has no memory between runs, on purpose.
2. **Touch production or staging.** Ports 4200 and 4201 are rejected, as are
   the production/staging container names and any scratch-volume name that
   looks like real data (`openfang_openfang-data`, `openfang-staging-data`,
   `*premigration*`, `*backup*`).

The clone also gets a freshly generated `api_key` at `up` time, because the
reference volume's `config.toml` still carries the key that leaked into the
public git history in commit `bf289dd`. `refstand.sh key` prints the ephemeral
one; `refstand.sh api` uses it.

## Keeping the reference alive

Both artefacts are *unused* as far as Docker is concerned between runs, which
is exactly what makes them prunable: `docker image prune -a` removes the
pristine image because no container holds it, and `docker volume prune`
removes `openfang-staging-premigration` because nothing is mounting it. Do
not run either sweep on this box without `--filter` — and there are archives
either way (2026-08-14, `/srv/backups/refstand/`, 0600):

```sh
docker load -i /srv/backups/refstand/openfang-v0.6.9-pristine.image.tar.gz
docker volume create openfang-staging-premigration
tar -C /var/lib/docker/volumes/openfang-staging-premigration \
    -xf /srv/backups/refstand/openfang-staging-premigration-vol.tar
```

`refstand.sh up` fails loudly ("nothing to compare against") if either one is
missing, rather than silently booting an empty stand.

## How staging was promoted (2026-08-14)

Staging was never a compose service — it is a plain `docker run` (no
`com.docker.compose.*` labels). Recreating it on the production image:

```sh
# 1. snapshots first
OPENFANG_HOME_HOST=/var/lib/docker/volumes/openfang-staging-data/_data \
OPENFANG_CONTAINER=openfang-staging ofbackup create /srv/backups
docker stop openfang-staging
tar -C /var/lib/docker/volumes/openfang-staging-data \
    -cf /srv/backups/openfang-staging-data-vol-<stamp>-pre-sprint3.tar _data

# 2. keep the old container as the rollback, recreate on the prod image
docker inspect openfang-staging > /srv/backups/openfang-staging-inspect-sprint2b-<stamp>.json
docker rename openfang-staging openfang-staging-sprint2b-old
docker run -d --name openfang-staging --restart unless-stopped \
  -e OPENFANG_LISTEN=0.0.0.0:4200 -e OPENFANG_HOME=/data \
  -p 127.0.0.1:4201:4200 -v openfang-staging-data:/data:z \
  openfang:sprint3
```

`openfang:sprint3` and `openfang-openfang:latest` (what production runs) are
the same image id — `sha256:cbbebc3f…` — so this is a recreate, not a build.
Rollback is `docker rm -f openfang-staging && docker rename
openfang-staging-sprint2b-old openfang-staging && docker start openfang-staging`;
restore the volume from the tar first if the schema migration has to be undone.

The staging `api_key` was rotated in the same window: the previous one was
published in commit `bf289dd` and is now rejected. `tests/fang/harness/lib.sh`
reads the key out of `config.toml` at call time, so nothing in the harness
needed changing.
