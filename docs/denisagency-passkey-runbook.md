# DenisAgency OpenFang passkey runbook

This runbook contains no credentials. Passkey invitation URLs and the machine API key must never be committed, pasted into logs, or passed as command-line arguments.

## Production identity

- Dashboard: `https://denis-openfang.moone.dev`
- RP ID: `denis-openfang.moone.dev`
- RP origin: `https://denis-openfang.moone.dev`
- Container: discover it at runtime with `docker ps --filter name=openfang --format '{{.Names}}'`
- Persistent home inside the container: `/data`

The required configuration is:

```toml
[auth]
enabled = true
rp_id = "denis-openfang.moone.dev"
rp_origin = "https://denis-openfang.moone.dev"
rp_name = "OpenFang DenisAgency"
session_ttl_hours = 168
```

OpenFang refuses to start passkey auth with an incomplete configuration, a non-HTTPS origin, a port, a path, or a host that differs from the RP ID.

## Before migration or deployment

1. Run the WAL-safe `ofbackup` workflow from the installed `fang-upgrade` skill.
2. Verify the backup before changing the container or SQLite schema.
3. Record the running image/commit and confirm `/api/health` is healthy.

The migration adds passkey users, credentials, hashed invitations, and hashed server sessions. It does not modify agent/session data.

## Create the initial three invitations

Run once, after the new binary and `[auth]` configuration are present. The command creates exactly `Слава`, `Денис`, and `Резерв`. It refuses to continue if one already has an active passkey or pending invite.

```bash
container_name="$(docker ps --filter name=openfang --format '{{.Names}}' | head -n1)"
docker exec "$container_name" openfang auth bootstrap \
  --expires-hours 72 \
  --output /tmp/openfang-passkey-invites.txt
docker cp "$container_name":/tmp/openfang-passkey-invites.txt ./OPENFANG-PASSKEY-INVITES.txt
chmod 0600 ./OPENFANG-PASSKEY-INVITES.txt
docker exec "$container_name" unlink /tmp/openfang-passkey-invites.txt
```

Move the local file directly to the approved secret handoff location. Do not print it with `cat`, upload it to GitHub, or send its contents through chat.

## Inspect slots

```bash
container_name="$(docker ps --filter name=openfang --format '{{.Names}}' | head -n1)"
docker exec "$container_name" openfang auth list --json
```

This displays only slot names, credential counts, and invitation expiry timestamps. It never displays a credential, session token, or invitation URL.

## Revoke a slot

Revoking immediately invalidates that slot's passkey, all of its server sessions, and any pending invitation:

```bash
docker exec "$container_name" openfang auth revoke denis
```

## Reset a slot and issue a replacement link

The output file must not already exist:

```bash
docker exec "$container_name" openfang auth reset-slot denis \
  --expires-hours 72 \
  --output /tmp/denis-passkey-replacement.txt
docker cp "$container_name":/tmp/denis-passkey-replacement.txt ./DENIS-PASSKEY-REPLACEMENT.txt
chmod 0600 ./DENIS-PASSKEY-REPLACEMENT.txt
docker exec "$container_name" unlink /tmp/denis-passkey-replacement.txt
```

For a new temporary test slot, also pass `--display-name`. After the test, remove it completely with `openfang auth revoke smoke --delete`. The `--delete` flag is reserved for ephemeral test identities; normal slots should remain as revoked audit history.

## Verification

- `GET /api/health` is public and returns `200`.
- Anonymous `GET /` redirects to `/login`.
- Anonymous protected JSON routes, including `/api/sessions`, return JSON `401`.
- The old `/api/auth/login` password endpoint returns `404`.
- Passkey login sets `__Host-openfang_session` with `Secure`, `HttpOnly`, `SameSite=Strict`, `Path=/`, and no `Domain`.
- A replayed or expired ceremony fails. Registration invitations are stored only as SHA-256 and are consumed atomically.
- Browser mutations and browser WebSockets require the exact RP `Origin`.
- A machine `Authorization: Bearer …` request still works on protected HTTP and WebSocket routes, but the dashboard never asks for or stores this key.
- Restarting the container preserves registered passkeys and unexpired sessions in the named volume.
