# Deploy, back up, restore

This folder holds the homelab setup and the backup recipe. For a first look on one machine, use the plain [compose.yaml](../compose.yaml) and the [quick start](../docs/quickstart.md) instead.

| File | What it is |
|---|---|
| [compose.homelab.yaml](compose.homelab.yaml) | The app, Postgres 17 and Caddy with HTTPS, and the `ocr` worker and the Open Wearables sidecar when those profiles are on. Secrets are files, the containers are read-only |
| [db/app-role.sh](db/app-role.sh) | Gives the app its own database role, which owns no table and cannot rewrite the audit log |
| [db/ow-role.sql](db/ow-role.sql) | Gives the Open Wearables sidecar (`wearables` profile) its own database, and a role that cannot open the health database |
| [Caddyfile](Caddyfile) | HTTPS for `OWNHEALTH_DOMAIN`, open to the LAN and a Tailscale tailnet only, and the share listener that answers only the SMART Health Link routes |
| [env.homelab.example](env.homelab.example) | The `.env` for the homelab compose |
| [restic.env.example](restic.env.example) | The `restic.env`: the backup repository and its keys, read by the `admin` service only |
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
cp src/deploy/env.homelab.example .env        # set OWNHEALTH_DOMAIN
cp src/deploy/restic.env.example restic.env   # set RESTIC_REPOSITORY, and the S3 keys if it needs them
mkdir -p deploy && cp -r src/deploy/restic src/deploy/db deploy/

mkdir -p inbox && sudo chown 10001:10001 inbox   # the watched folder (docs/recipes/scanner-to-folder.md)
mkdir -m 700 secrets
openssl rand -base64 32 | tr -d '\n' > secrets/db_password
openssl rand -base64 32 | tr -d '\n' > secrets/app_db_password
openssl rand -base64 48 | tr -d '\n' > secrets/secret_key
openssl rand -base64 32 | tr -d '\n' > secrets/restic_password
openssl rand -base64 32 | tr -d '\n' > secrets/secrets_key     # seals inbox passwords and tokens
chmod 644 secrets/*                           # the containers run as their own users; the folder stays 700
mkdir -m 755 secrets/imaging                  # the app mounts it; the imaging profile's password goes here

docker compose up -d
docker compose run --rm admin restic init     # once, creates the backup repository
```

Open `https://<OWNHEALTH_DOMAIN>` and create the first account with a passkey. Write down the recovery codes.

**Two database roles.** `ownhealth` owns the tables. The one-shot `admin` service connects as it, runs the migrations, then runs `db/app-role.sh`, which creates `ownhealth_app` and grants it SELECT, INSERT, UPDATE and DELETE on every table except UPDATE, DELETE and TRUNCATE on the audit log. The app connects as `ownhealth_app` and waits for `admin` to finish. It owns nothing, so it cannot disable the audit log's trigger or alter a table. The backup scripts run in `admin` too, because a restore needs the owner. Only `admin` reads `restic.env`, so the app, the process that faces the network, never holds the keys to the backups. An older install has `RESTIC_REPOSITORY` and the S3 keys in `.env`; it keeps working, and moving those lines into `restic.env` takes them away from the app.

**Store the restic password in your password manager now.** The backup is encrypted with it and it is not inside the backup. Without it, the backup cannot be read.

**Store `secrets/secrets_key` there too.** It encrypts the passwords and tokens OwnHealth keeps for its inboxes (an IMAP password, a Paperless or Nightscout token, PDF passwords). The backup holds them only in encrypted form. Restored with another key, those inboxes stop until you enter their secrets again.

**HTTPS.** Caddy gets a certificate on its own when `OWNHEALTH_DOMAIN` is a public DNS name. For a name only your LAN knows, put `tls internal` in the site block of the Caddyfile and trust Caddy's root certificate on your devices, or use a Caddy build with your DNS provider's challenge plugin. Passkeys need HTTPS on a real name, so plain HTTP on an IP address does not work past the first look.

**Who can connect.** The Caddyfile answers 403 to every address outside the private ranges and the Tailscale range `100.64.0.0/10`. Keep it that way unless you mean to put health data on the public internet. The check works only if Caddy sees the real client address. Rootless Docker, and Docker's userland proxy for IPv6 clients when the compose network has no IPv6, hand Caddy a private gateway address instead, so every client passes. On such a host, or one with a public address, also close ports 80 and 443 (TCP and UDP) to the internet in the host firewall or the router, and check from outside that the name does not answer. The claude.ai connector is the one case that needs a public endpoint: [its recipe](../docs/recipes/claude-connector.md) opens only the four paths it uses and keeps the rest behind a login.

**Your own reverse proxy.** If one already runs on the host, drop the `caddy` service, give `app` a port such as `127.0.0.1:8080:8080`, and point your proxy at it. Set `OWNHEALTH_BASE_URL` to the HTTPS address people open.

## Sharing a summary

A person can share a scoped summary with a doctor through a SMART Health Link: a link, or its QR code, plus an optional PIN. The doctor's viewer fetches the summary from your instance, so the link routes must answer from the internet while the rest of OwnHealth stays on your network. Sharing is off until you set it up.

1. Caddy listens for the links on port 8090 inside its container. Compose publishes it on `127.0.0.1:8090` of the host, never on the network. Set `OWNHEALTH_SHARE_PORT` in `.env` if 8090 is taken on the host.
2. Point a tunnel on the same host at the share port: 8090, or the `OWNHEALTH_SHARE_PORT` you set in `.env`. Compose reads `.env`, but your shell does not, so if you changed the port, run `export OWNHEALTH_SHARE_PORT=<port>` first. With Tailscale Funnel: `tailscale funnel --bg ${OWNHEALTH_SHARE_PORT:-8090}`. With Cloudflare Tunnel: a public hostname whose service is `http://localhost:<port>`, with that same port. Never point it at the app or at port 443. The tunnel must add its own hop to `X-Forwarded-For`, so the connecting address comes after any value the client sent. A tunnel that passes the client's header through unchanged lets a recipient write any address into the access log. Tailscale Funnel and Cloudflare Tunnel are safe. An ssh `-R` forward or a raw TCP tunnel is not.
3. Set `OWNHEALTH_SHARE_URL` in `.env` to the https address the tunnel gives, such as `https://share.example.org`. The app refuses to start with an http address, except one on localhost, with a user name or password in it, and with anything after the host and port.
4. `docker compose up -d`.

What goes public: `POST /api/shl/<id>` (and its CORS preflight) and `GET /api/shl/<id>/file/<ticket>`, each rewritten to its canonical path. Every other path on the tunnel gets 404 from Caddy, including `/`, `/login`, `/api`, `/mcp` and `/dicom-web`. A link answers only while it is live and its creator may still share what it carries, and at most 60 times an hour. Locking a category, or narrowing a grant below it, revokes every link that carries it for good: lifting the lock brings none back. The access log the person reads names each recipient's address: Caddy keeps the address the tunnel forwards, and the app trusts that header only from the proxies in `OWNHEALTH_TRUSTED_PROXIES` (the compose network).

Check it from outside: `curl -i https://share.example.org/login` answers 404, and a link the app makes opens in a SMART Health Links viewer. To turn sharing off again, remove `OWNHEALTH_SHARE_URL` and run `docker compose up -d`: every link and every file address already handed out then answers 404.

## Inboxes and OCR

Since 0.3.0, documents and device data can arrive without an upload: a mailbox, a watched folder, Paperless-ngx, an iOS Shortcut, Health Auto Export, Nightscout. You add each one in the app, under **Inbox**. The setup for each is in [docs/recipes](../docs/recipes/README.md).

**The watched folder** is `./inbox` next to `compose.yaml`, mounted as `/inbox`. A subfolder per profile slug (`inbox/alex/`) sends files to that person. The app runs as user 10001 and moves imported files to `inbox/.done/`, so the folder must be writable by that user.

**OCR for scans and photos** is the `ocr` profile. Add two lines to `.env`:

```sh
COMPOSE_PROFILES=ocr   # starts the `ocr` worker, from drumsergio/ownhealth:<version>-ocr
OWNHEALTH_OCR=1        # the app sends pages without a text layer to it
```

then `docker compose up -d`. The worker is the same program from the image variant that carries the OCR engine (PaddleOCR PP-OCRv5 on ONNX Runtime) and its models, about 75 MB more than the default image (the OCR engine and 13 MB of models). It serves no HTTP and only runs jobs on the `ocr` queue, on the CPU. Turn both lines on together. With only `OWNHEALTH_OCR=1`, scans queue for a worker that never starts. Without the profile, scans wait as pending documents for your own agent ([agent-assisted extraction](../docs/agent-extraction.md)) or for you.

Scans that arrived while OCR was off are not lost: each time the `ocr` worker starts with `OWNHEALTH_OCR=1`, it queues every scan and photo still waiting as a pending document for pages without text, and takes it off the pending list. Pages OCR then cannot read go back on it.

## Wearables

The `wearables` profile runs the Open Wearables sidecar (OW 0.11.0, pinned by digest), which holds the Oura, Whoop, Polar and Withings logins. It publishes no port and sends no usage data. Its own database lives on the same Postgres, and its role cannot open the health database.

**Turn it on.** On an install made before 1.0, get the new files first: `git -C src pull && cp -r src/deploy/db deploy/`, then copy the new `src/deploy/compose.homelab.yaml` over `compose.yaml` and carry over any edits of your own. Then put OW's three secrets in `openwearables.env`, next to `.env`. Never put them in `.env`: every OwnHealth container reads that file.

```sh
for k in DB_PASSWORD SECRET_KEY ADMIN_PASSWORD; do echo "OPENWEARABLES_$k=$(openssl rand -hex 32)"; done > openwearables.env
chmod 600 openwearables.env   # compose reads it, not the containers
```

Then add `wearables` to the `COMPOSE_PROFILES` line in `.env`: `COMPOSE_PROFILES=ocr,wearables` with OCR on, otherwise `COMPOSE_PROFILES=wearables`. A second `COMPOSE_PROFILES` line replaces the first, so OCR would stop. Start it with `docker compose up -d`.

The one-shot `ow-db-init` creates the `openwearables` database and role, then OW migrates its tables. It starts only once the app is healthy. So if `openwearables.env` is missing, or a line in it is empty, only the sidecar stops: OwnHealth keeps running, and `docker compose logs ow-db-init ow-app` names the missing variable. Fix the file and run `docker compose up -d` again.

**Change its database password.** Write the new `OPENWEARABLES_DB_PASSWORD` in `openwearables.env` and run `docker compose up -d`. It sets the new password on the role and recreates the sidecar with it.

**Turn it off.** Take `wearables` out of `COMPOSE_PROFILES`, then `docker compose stop ow-app ow-worker ow-beat ow-redis`. An `up -d` without the profile leaves the running sidecar alone. Its database stays in the `ownhealth-pg` volume, so turning the profile on again keeps the vendor connections.

**Restore.** `ownhealth backup` leaves OW's database out, because it is a cache of vendor data. After a restore onto new volumes, `docker compose up -d` with the profile on creates an empty one, and every vendor has to be connected again: the logins were only in that database.

## Monitoring the inboxes

`GET /api/inboxes/health` lists every inbox you can see with its last run, its last successful sync, its last error, the failed runs in a row, and `stale`: switched on, polled, and no success for three poll intervals. `last_run_status` is `ok`, `partial` (some items were skipped or failed) or `error`, with `last_run_problems` and the first problem as `last_run_problem` (a code and a short message, never the content of a mail or document). A run in which every item failed counts as an error: the failed-runs count goes up and the last successful sync stays where it was. For a person, each row says `can_manage`: the Connections screen (v1.0) lists and reconnects only those rows. Only those rows also carry `created_at`, `poll_seconds`, `reconsent_needed_at` (the vendor refused the token, so someone must sign in again), `grant_expires_at`, `access_expires_at`, the connection's `server`, and `open_gaps`, the open sync-gap alerts filed on the source's profile. A viewer and a source token get none of them. Add `?format=prometheus` for the Prometheus text format. Labels carry the source id and kind only, never names or error text:

```text
ownhealth_source_enabled{source="…",kind="email"} 1
ownhealth_source_stale{source="…",kind="email"} 0
ownhealth_source_consecutive_failures{source="…",kind="email"} 0
ownhealth_source_last_success_timestamp_seconds{source="…",kind="email"} 1762700112.5
ownhealth_source_last_run_timestamp_seconds{source="…",kind="email"} 1762700112.5
ownhealth_source_last_run_ok{source="…",kind="email"} 1
```

`ownhealth_source_last_run_ok` is 1 when the newest finished run brought everything in (or no run has finished yet) and 0 when it was partial or failed.

A monitor has no browser session, so it reads one inbox at a time with that inbox's own push token (`oh_ing_...`, created in **Inbox**): `GET /api/inboxes/<id>/health?format=prometheus` with `Authorization: Bearer oh_ing_...`. The token sees its own inbox and nothing else. For a push inbox (Paperless, Shortcut, Health Auto Export) it is the token the sender already uses. For a polled one (mailbox, folder, Nightscout), create a token just for the monitor with **Monitor token** on the inbox's card. The card then shows the health address to scrape.

A Prometheus job per inbox:

```yaml
scrape_configs:
  - job_name: ownhealth-mailbox
    scheme: https
    metrics_path: /api/inboxes/<source id>/health
    params: { format: [prometheus] }
    authorization: { credentials_file: /etc/prometheus/ownhealth-mailbox.token }
    static_configs: [{ targets: ["health.example.lan"] }]
```

Alert rules that cover both kinds of inbox:

```yaml
groups:
  - name: ownhealth-inboxes
    rules:
      - alert: OwnHealthInboxStale        # a polled inbox stopped syncing
        expr: ownhealth_source_stale == 1
        for: 30m
      - alert: OwnHealthPushSilent        # nothing pushed for 6 hours
        expr: ownhealth_source_enabled == 1 and time() - ownhealth_source_last_success_timestamp_seconds{kind=~"health_auto_export|paperless|shortcut"} > 6 * 3600
      - alert: OwnHealthInboxPartial      # runs finish but skip items
        expr: ownhealth_source_last_run_ok == 0
        for: 3m                           # three runs of a mailbox polled every minute
```

A push inbox has no schedule, so OwnHealth never marks it stale; the second rule alerts on silence instead. Pick the hours to match how often the sender pushes.

## Backups

`ownhealth backup` writes three parts into one directory.

- `db.dump`: `pg_dump -Fc` of the database.
- `blobs/`: the original documents and genome files, hard-linked from the data volume, so the staging copy costs no space.
- `manifest.json`: app version, migrations, row count per table, the list of blob hashes, the `state_hash` of every profile, and the head of the audit log (`audit_chain`: the number and hash of its newest entry, and how far the hash chain holds).

The dump, the row counts, the state hashes and the audit head come from one database snapshot. A backup taken while someone imports a lab report is still consistent.

**The audit anchor.** Each backup also writes the audit head to `audit-head.json`, outside the database: in `OWNHEALTH_ANCHOR_DIR`, or `/data/anchor` on the data volume by default. The file is replaced atomically and only its owner can read it. The next backup first checks that this entry of the live log still has the same hash. Someone with the database owner's rights can edit an entry and hash the whole log again, so that the log checks out on its own; the anchor catches that. When the check fails, the backup stops before it writes anything and exits 1 with the entry it expected, and the audit page on MCP & access shows the mismatch. Find out what changed first. If you restored an older backup onto the same data volume, that explains it. Then move `audit-head.json` aside and keep it as evidence: the next backup anchors the log as it is now. The first backup finds no anchor, checks nothing, and writes one. A log the page already shows as broken does not stop the backups; the manifest records the break.

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
3. It compares the row count of every table with the manifest, hashes every blob again, rebuilds the state of every profile and compares each `state_hash` with the manifest. It checks the restored audit log too: it must end at the head the manifest recorded, and hold as far as it held then.
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

Take a fresh backup first, while `compose.yaml` still names the old version: `backup.sh` runs in the `admin` image, and a newer image would write the backup with its own rules before the migrations ran.

```sh
./deploy/restic/backup.sh         # a fresh backup before every upgrade
```

Then change the image tag of the `admin` and `app` services in `compose.yaml` to the new version, and the `ocr` service's tag to `<version>-ocr`, and start it:

```sh
docker compose pull && docker compose up -d
```

Migrations are forward-only and run at start, in the `admin` service, before the app comes up. To go back to an older version, restore the backup you took before the upgrade.

**The imaging folder (from 1.0).** The new `compose.homelab.yaml` mounts `secrets/imaging` into the app, with the `imaging` profile on or off. Create the folder before the first `docker compose up -d` with that file: `mkdir -m 755 secrets/imaging`. If Docker started first, it made the folder as root: `sudo chown "$(id -u):$(id -g)" secrets/imaging && chmod 755 secrets/imaging`. To turn imaging on, follow the comment above the `orthanc` service in `compose.homelab.yaml`: it writes `secrets/imaging/orthanc_password` with mode 644.

**To 0.4.0.** After the backup, set `admin` and `app` to `drumsergio/ownhealth:0.4.0` and `ocr` to `drumsergio/ownhealth:0.4.0-ocr`; nothing else in the compose files changes. Rule pack 0.2.0 moves every profile's `state_hash` once, so restore a backup made before the upgrade with 0.3.0, as above. For the same reason, `restore-test.sh` on a snapshot made before the upgrade reports every profile's `state_hash` as different; run `./deploy/restic/backup.sh` once after the upgrade, so the restore test checks a 0.4.0 snapshot. A household with a genome starts downloading NCBI's weekly ClinVar file (about 200 MB, no personal data in the request); turn it off in Settings, or set a local file path on a server without internet.

**To 0.3.0.** Take the new `compose.homelab.yaml` (it adds the `ocr` worker, the inbox folder and the `secrets_key` secret), then before `docker compose up -d`:

```sh
openssl rand -base64 32 | tr -d '\n' > secrets/secrets_key && chmod 644 secrets/secrets_key
mkdir -p inbox && sudo chown 10001:10001 inbox
```

Compose refuses to start without the secret file. Keep a copy of it with the restic password.

Scans a 0.2.0 install kept as pending documents are read by OCR once you turn the `ocr` profile on: the worker queues them when it starts.
