# Deploy, back up, restore

This folder holds the homelab setup and the backup recipe. For a first look on one machine, use the plain [compose.yaml](../compose.yaml) and the [quick start](../docs/quickstart.md) instead.

| File | What it is |
|---|---|
| [compose.homelab.yaml](compose.homelab.yaml) | The app, Postgres 17 and Caddy with HTTPS. Secrets are files, the app container is read-only |
| [db/app-role.sh](db/app-role.sh) | Gives the app its own database role, which owns no table and cannot rewrite the audit log |
| [Caddyfile](Caddyfile) | HTTPS for `OWNHEALTH_DOMAIN`, open to the LAN and a Tailscale tailnet only |
| [env.homelab.example](env.homelab.example) | The `.env` for the homelab compose |
| [restic/backup.sh](restic/backup.sh) | Nightly backup into a restic repository, with retention |
| [restic/restore-test.sh](restic/restore-test.sh) | Restores a snapshot into a scratch database and checks it |
| [restic/restore.sh](restic/restore.sh) | Restores a snapshot into a fresh stack |

## Homelab setup

You need Docker with Compose v2 and a name for OwnHealth that your devices resolve, such as `health.example.lan`.

```sh
mkdir -p /opt/ownhealth && cd /opt/ownhealth
git clone --depth 1 https://github.com/GeiserX/OwnHealth.git src
cp src/deploy/compose.homelab.yaml compose.yaml
cp src/deploy/Caddyfile Caddyfile
cp src/deploy/env.homelab.example .env        # set OWNHEALTH_DOMAIN and RESTIC_REPOSITORY
mkdir -p deploy && cp -r src/deploy/restic src/deploy/db deploy/

mkdir -m 700 secrets
openssl rand -base64 32 | tr -d '\n' > secrets/db_password
openssl rand -base64 32 | tr -d '\n' > secrets/app_db_password
openssl rand -base64 48 | tr -d '\n' > secrets/secret_key
openssl rand -base64 32 | tr -d '\n' > secrets/restic_password
chmod 644 secrets/*                           # the containers run as their own users; the folder stays 700

docker compose up -d
docker compose run --rm admin restic init     # once, creates the backup repository
```

Open `https://<OWNHEALTH_DOMAIN>` and create the first account with a passkey. Write down the recovery codes.

**Two database roles.** `ownhealth` owns the tables. The one-shot `admin` service connects as it, runs the migrations, then runs `db/app-role.sh`, which creates `ownhealth_app` and grants it SELECT, INSERT, UPDATE and DELETE on every table except UPDATE, DELETE and TRUNCATE on the audit log. The app connects as `ownhealth_app` and waits for `admin` to finish. It owns nothing, so it cannot disable the audit log's trigger or alter a table. The backup scripts run in `admin` too, because a restore needs the owner.

**Store the restic password in your password manager now.** The backup is encrypted with it and it is not inside the backup. Without it, the backup cannot be read.

**HTTPS.** Caddy gets a certificate on its own when `OWNHEALTH_DOMAIN` is a public DNS name. For a name only your LAN knows, put `tls internal` in the site block of the Caddyfile and trust Caddy's root certificate on your devices, or use a Caddy build with your DNS provider's challenge plugin. Passkeys need HTTPS on a real name, so plain HTTP on an IP address does not work past the first look.

**Who can connect.** The Caddyfile answers 403 to every address outside the private ranges and the Tailscale range `100.64.0.0/10`. Keep it that way unless you mean to put health data on the public internet.

**Your own reverse proxy.** If one already runs on the host, drop the `caddy` service, give `app` a port such as `127.0.0.1:8080:8080`, and point your proxy at it. Set `OWNHEALTH_BASE_URL` to the HTTPS address people open.

## Backups

`ownhealth backup` writes three parts into one directory.

- `db.dump`: `pg_dump -Fc` of the database.
- `blobs/`: the original documents and genome files, hard-linked from the data volume, so the staging copy costs no space.
- `manifest.json`: app version, migrations, row count per table, the list of blob hashes and the `state_hash` of every profile.

The dump, the row counts and the state hashes come from one database snapshot. A backup taken while someone imports a lab report is still consistent.

`restic/backup.sh` writes that directory, takes one restic snapshot of it, deletes the staging copy and applies retention (7 daily, 5 weekly, 12 monthly; set `OWNHEALTH_RESTIC_KEEP` to change it). restic encrypts and deduplicates, so a nightly run only uploads what changed.

```sh
cd /opt/ownhealth
./deploy/restic/backup.sh
```

Run it from cron or a systemd timer:

```cron
15 3 * * *  cd /opt/ownhealth && ./deploy/restic/backup.sh >> backup.log 2>&1
45 4 * * 0  cd /opt/ownhealth && ./deploy/restic/restore-test.sh >> backup.log 2>&1
```

## Restore test

A backup that was never restored counts as missing. `restic/restore-test.sh` proves the latest snapshot works:

1. restic restores the snapshot into `/data/backups/restore`.
2. `ownhealth verify-restore` loads the dump into a scratch database `<db>_restore_check`.
3. It compares the row count of every table with the manifest, hashes every blob again, rebuilds the state of every profile and compares each `state_hash` with the manifest.
4. It drops the scratch database and the restored files.

It prints a JSON report and exits 1 on any difference.

```json
{
  "ok": true,
  "as_of": "2026-11-07",
  "tables": {},
  "profiles": {
    "2f42707f-20c9-406c-b04a-d17c398b68a6": {"expected": "sha256:5c61…", "actual": "sha256:5c61…", "ok": true}
  },
  "blobs": {"checked": 27, "missing": [], "corrupt": []}
}
```

`tables` lists only the tables whose counts differ. Pass a snapshot id to test an older one: `./deploy/restic/restore-test.sh 4be1a2c3`. It only reads the live database and blobs.

## Restore

Use this to restore onto a new machine, or after losing the volumes.

```sh
cd /opt/ownhealth                 # same compose.yaml, .env and secrets/ as before
docker compose down               # if anything still runs
docker volume rm ownhealth_ownhealth-data ownhealth_ownhealth-pg   # only when you mean to replace them
docker compose up -d db
./deploy/restic/restore.sh        # or: ./deploy/restic/restore.sh <snapshot id>
docker compose up -d
```

`restore.sh` runs in a one-off admin container before the app starts. It refuses a database that already has tables, so it cannot overwrite a running instance. After loading the dump and the blobs it rebuilds every profile's state and compares the `state_hash` with the manifest; the same hash means the restored data is the data you backed up. It exits 1 if anything differs.

Restore with the OwnHealth version that made the backup (`app_version` in the manifest), then upgrade. A newer rule pack can change a `state_hash` on purpose, and the check would report it as a difference.

## Upgrades

Change the image tag in `compose.yaml` to the new version, then:

```sh
./deploy/restic/backup.sh         # a fresh backup before every upgrade
docker compose pull && docker compose up -d
```

Migrations are forward-only and run at start, in the `admin` service, before the app comes up. To go back to an older version, restore the backup you took before the upgrade.
