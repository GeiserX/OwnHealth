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
mkdir -p -m 700 keyring/ownhealth && sudo chown 10001:10001 keyring/ownhealth   # the profile keys, outside every backup (see Backups)
mkdir -m 700 secrets
openssl rand -base64 32 | tr -d '\n' > secrets/db_password
openssl rand -base64 32 | tr -d '\n' > secrets/app_db_password
openssl rand -base64 48 | tr -d '\n' > secrets/secret_key
openssl rand -base64 32 | tr -d '\n' > secrets/restic_password
openssl rand -base64 32 | tr -d '\n' > secrets/secrets_key     # seals the profiles' files and inbox secrets
chmod 644 secrets/*                           # the containers run as their own users; the folder stays 700
mkdir -m 755 secrets/imaging                  # the app mounts it; the imaging profile's password goes here

docker compose up -d
docker compose run --rm admin restic init     # once, creates the backup repository
```

Open `https://<OWNHEALTH_DOMAIN>` and create the first account with a passkey. Write down the recovery codes.

**Two database roles.** `ownhealth` owns the tables. The one-shot `admin` service connects as it, runs the migrations, then runs `db/app-role.sh`, which creates `ownhealth_app` and grants it SELECT, INSERT, UPDATE and DELETE on every table except UPDATE, DELETE and TRUNCATE on the audit log. The app connects as `ownhealth_app` and waits for `admin` to finish. It owns nothing, so it cannot disable the audit log's trigger or alter a table. The backup scripts run in `admin` too, because a restore needs the owner. Only `admin` reads `restic.env`, so the app, the process that faces the network, never holds the keys to the backups. An older install has `RESTIC_REPOSITORY` and the S3 keys in `.env`; it keeps working, and moving those lines into `restic.env` takes them away from the app.

**Store the restic password in your password manager now.** The backup is encrypted with it and it is not inside the backup. Without it, the backup cannot be read.

**Store `secrets/secrets_key` there too.** Each profile's documents and genome files are encrypted under that profile's own key, and those keys are encrypted under this one. It also encrypts the passwords and tokens OwnHealth keeps for its inboxes (an IMAP password, a Paperless or Nightscout token, PDF passwords). The backup holds all of them only in encrypted form, so without this key the backup cannot be read. OwnHealth refuses to start without it. This key is the only one you keep in the password manager. The profile keys stay in `keyring/ownhealth/keyring.json`, never in the password manager and never in a backup (see "The keyring" under Backups).

**HTTPS.** Caddy gets a certificate on its own when `OWNHEALTH_DOMAIN` is a public DNS name. For a name only your LAN knows, put `tls internal` in the site block of the Caddyfile and trust Caddy's root certificate on your devices, or use a Caddy build with your DNS provider's challenge plugin. Passkeys need HTTPS on a real name, so plain HTTP on an IP address does not work past the first look.

**Who can connect.** The Caddyfile answers 403 to every address outside the private ranges and the Tailscale range `100.64.0.0/10`. Keep it that way unless you mean to put health data on the public internet. The check works only if Caddy sees the real client address. Rootless Docker, and Docker's userland proxy for IPv6 clients when the compose network has no IPv6, hand Caddy a private gateway address instead, so every client passes. On such a host, or one with a public address, also close ports 80 and 443 (TCP and UDP) to the internet in the host firewall or the router, and check from outside that the name does not answer. The claude.ai connector is the one case that needs a public endpoint: [its recipe](../docs/recipes/claude-connector.md) opens only the four paths it uses and keeps the rest behind a login.

**Your own reverse proxy.** If one already runs on the host, drop the `caddy` service, give `app` a port such as `127.0.0.1:8080:8080`, and point your proxy at it. Set `OWNHEALTH_BASE_URL` to the HTTPS address people open.

## Sharing a summary

A person can share a scoped summary with a doctor through a SMART Health Link: a link, or its QR code, plus an optional PIN. The doctor's viewer fetches the summary from your instance, so the link routes must answer from the internet while the rest of OwnHealth stays on your network. Sharing is off until you set it up.

1. Caddy listens for the links on port 8090 inside its container. Compose publishes it on `127.0.0.1:8090` of the host, never on the network. Set `OWNHEALTH_SHARE_PORT` in `.env` if 8090 is taken on the host.
2. Point a tunnel on the same host at the share port: 8090, or the `OWNHEALTH_SHARE_PORT` you set in `.env`. Compose reads `.env`, but your shell does not, so if you changed the port, run `export OWNHEALTH_SHARE_PORT=<port>` first. With Tailscale Funnel: `tailscale funnel --bg ${OWNHEALTH_SHARE_PORT:-8090}`. With Cloudflare Tunnel: a public hostname whose service is `http://localhost:<port>`, with that same port. Never point it at the app or at port 443. The tunnel must add its own hop to `X-Forwarded-For`, so the connecting address comes after any value the client sent. A tunnel that passes the client's header through unchanged lets a recipient write any address into the access log. Tailscale Funnel and Cloudflare Tunnel are safe. An ssh `-R` forward or a raw TCP tunnel is not.
3. Set `OWNHEALTH_SHARE_URL` in `.env` to the https address the tunnel gives, such as `https://share.example.org`. The app refuses to start with an http address, except one on localhost, with a user name or password in it, and with anything after the host and port.
4. `docker compose up -d`.

What goes public: `POST /api/shl/<id>` (and its CORS preflight) and `GET /api/shl/<id>/file/<ticket>`, each rewritten to its canonical path. Every other path on the tunnel gets 404 from Caddy, including `/`, `/login`, `/api`, `/mcp` and `/dicom-web`. A link answers only while it is live and its creator may still share what it carries, and at most 60 times an hour. Locking a category, or narrowing a grant below it, revokes every link that carries it for good: lifting the lock brings none back. The access log the person reads names each recipient's address: Caddy keeps the address the tunnel forwards, and the app trusts that header only from the proxies in `OWNHEALTH_TRUSTED_PROXIES`. The homelab stack sets it to Caddy's own address on the `front` network (`OWNHEALTH_CADDY_IP`).

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

**Link a profile.** Create a dedicated user and an API key in Open Wearables. Connect the person's vendor accounts to that user. Save the developer password and API key in a private JSON file, with fields `admin_password` and `api_key`. Pass the file on standard input.

```sh
docker compose exec -T app ownhealth openwearables-connect \
  --profile alex --user "OPEN_WEARABLES_USER_UUID" \
  --admin-email "OPEN_WEARABLES_DEVELOPER_EMAIL" < ow-credentials.json
```

`--profile` takes the profile's short name, the one its inbox folders use (`alex`), or its id. On success the command prints the user, the profile and the new source. If a step fails, the error names it: the user check with the API key, or the developer sign-in.

Setup turns off providers outside OwnHealth's allowlist. OwnHealth pulls each allowed vendor on its own, one day at a time. The first pull asks for 30 days. Later pulls re-read the seven days before that vendor's last stored day and keep only rows they have not seen. If one vendor's connection ends in Open Wearables, the other vendors keep syncing. The Connections screen names that vendor; reconnect it in Open Wearables. If you delete the profile, OwnHealth removes its linked Open Wearables user on the next scheduler cycle. It retries if the sidecar is down.

**Vincular un perfil.** Crea un usuario dedicado y una clave de API en Open Wearables. Conecta las cuentas de la persona a ese usuario. Guarda la contraseña de administración y la clave de API en un archivo JSON privado, en los campos `admin_password` y `api_key`. Pasa el archivo por la entrada estándar con el comando anterior.

`--profile` acepta el nombre corto del perfil, el que usan sus carpetas de entrada (`alex`), o su id. Al terminar, el comando muestra el usuario, el perfil y la fuente nueva. Si un paso falla, el error lo nombra: la comprobación del usuario con la clave de API o el inicio de sesión de desarrollador.

La configuración desactiva los proveedores que no admite OwnHealth. OwnHealth sincroniza cada proveedor permitido por separado, día a día. La primera sincronización solicita 30 días. Las siguientes vuelven a leer los siete días anteriores al último día guardado de ese proveedor y solo guardan las filas nuevas. Si la conexión de un proveedor termina en Open Wearables, los demás se siguen sincronizando. La pantalla Conexiones nombra ese proveedor; vuelve a conectarlo en Open Wearables. Si borras el perfil, OwnHealth elimina su usuario de Open Wearables en el siguiente ciclo. Si el servicio no responde, vuelve a intentarlo.

The homelab stack trusts forwarded addresses from Caddy's IP on the `front` network. If that network overlaps with another network, set `OWNHEALTH_CADDY_SUBNET` and `OWNHEALTH_CADDY_IP` to an available subnet and an address inside it.

Si la red `front` coincide con otra red, configura `OWNHEALTH_CADDY_SUBNET` con una subred libre y `OWNHEALTH_CADDY_IP` con una dirección de esa subred. La aplicación solo confía en las direcciones que reenvía Caddy desde esa IP.

**Change its database password.** Write the new `OPENWEARABLES_DB_PASSWORD` in `openwearables.env` and run `docker compose up -d`. It sets the new password on the role and recreates the sidecar with it.

**Turn it off.** Take `wearables` out of `COMPOSE_PROFILES`, then `docker compose stop ow-app ow-worker ow-beat ow-redis`. An `up -d` without the profile leaves the running sidecar alone. Its database stays in the `ownhealth-pg` volume, so turning the profile on again keeps the vendor connections.

**Time zone of CGM exports.** A LibreView or Dexcom Clarity CSV export prints local times without an offset. Set `OWNHEALTH_TIMEZONE` in `.env` to the household's IANA zone, such as `OWNHEALTH_TIMEZONE=Europe/Madrid`, and every export is read in it. Unset, each export is read in the zone of the browser it was uploaded from, and the upload result names that zone. An old zone name that some browsers still send, such as `Asia/Calcutta`, is read as its current name, `Asia/Kolkata`, when the server does not know the old name. A zone the server knows keeps its name, such as `Europe/Oslo`. A zone the server does not know never stops an upload. An unknown `OWNHEALTH_TIMEZONE` is left out, so the browser's zone reads the export. Only when neither zone is known is an export read as UTC on its first import, and the upload result says so. The upload result, with its zone and this notice, shows only to members who may read wearables (`read:wearables`) on that profile. Each export keeps the zone it was read in, so changing or unsetting `OWNHEALTH_TIMEZONE` changes no export already stored, and the same file uploaded again with no known zone keeps its zone. Importing it again, for example after another export is retracted or after it moves to another profile, uses that zone. Uploading the same file in another zone replaces its readings, so they are never stored twice. Two names of one zone, such as `Asia/Calcutta` and `Asia/Kolkata`, are the same zone, and nothing is replaced.

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

- `db.dump`: `pg_dump -Fc` of the database, without the profile keys (`--exclude-table-data=profile_key`). Each held mail's key is still in it, encrypted under `secrets/secrets_key`: a backup taken while a mail was held still opens that mail, until retention removes the snapshot.
- `blobs/`: the original documents and genome files, hard-linked from the data volume, so the staging copy costs no space. A profile's files stay encrypted under its key, in `blobs/enc/`.
- `manifest.json`: app version, migrations, row count per table, the list of blob hashes, the hash of each encrypted file's bytes, the fingerprint of the secrets key (never the key), the profiles whose key the backup needs (`keys.keyring`) and the identifiers of those keys (`keys.key_ids`), the profiles of the dump whose key a delete shredded (`keys.closed`), the `state_hash` of every profile, and the head of the audit log (`audit_chain`: the number and hash of its newest entry, and how far the hash chain holds).

The dump, the row counts, the state hashes and the audit head come from one database snapshot. A backup taken while someone imports a lab report is still consistent. Files that the snapshot names stay on disk until the backup has copied them, also when someone releases a held mail, moves a document or deletes a profile while the backup runs. The removal of those files waits until the copy is done.

**The keyring.** Each profile's key lives in the database for the app, and in `keyring/ownhealth/keyring.json` for the backups. The keys in that file are encrypted under `secrets/secrets_key`, so the file alone opens nothing. The dump leaves the keys out, and every backup rewrites `keyring.json` after the dump and before it copies the files. The folder is a bind mount of its own (`./keyring/ownhealth` for the default Compose project, `OWNHEALTH_KEYRING_DIR=/keyring` in the `admin` and `app` containers), outside the data volume, so it is never in the staging folder or in restic. A backup refuses a keyring folder inside the blob folder or the backup folder. Without `OWNHEALTH_KEYRING_DIR` (an older compose file), the keyring sits in `/data/keyring` on the data volume, and each backup says so in `backup.log`: take the new compose files. Each install needs a keyring folder of its own: two installs never share one. Compose gives each project its own folder (`keyring/<project>`). When the manifest includes keys, restore and the restore test read the current keyring and check each key against the archive's identifier. A tombstone closes the key that was deleted. When a backup that holds no key for a profile (a keyless backup, or one taken before the profile's first encrypted file) brings it back and the tombstone names its deleted key, the profile can create a new key. If the keyring never held the deleted key, the tombstone has no key ID and the profile cannot store new sealed files. A profile the backup holds no key for (a backup taken before its first encrypted file, or before 1.0) gets the keyring's key, at restore or with its next file, so that file never replaces the key.

Keyless backups from v0.4.0 and earlier can still be restored. Untagged builds between the introduction of profile keys and the keyring wrote keyed dumps with a different format. If you have one of those backups, restore it with the build that wrote it. Then take a new backup with this version.

When you delete a profile, OwnHealth removes its key from the database and rewrites `keyring.json` at once, before it deletes the files. If that write fails, the delete still completes, the delete screen says that the key is still in `keyring.json`, and OwnHealth retries the rewrite. Once the rewrite succeeds, no backup opens that profile's files again, as long as no older copy of `keyring.json` exists. So:

- Never back up or version the `keyring/` folder. Tell host-level backup tools (Time Machine, Duplicacy, a NAS snapshot, a volume backup) to skip it.
- Keep exactly one extra copy off the box, never versioned, and overwrite it after every backup and every profile delete. Without it, files encrypted with missing profile keys stay unreadable (see Restore). Do not put the file in your password manager: only `secrets/secrets_key` goes there.
- After a profile delete, destroy every older copy of `keyring.json` you know of. The delete screen says so too.

A backup still holds the deleted profile's rows (its records, without the files) until retention removes the snapshot: 7 daily, 5 weekly and 12 monthly snapshots by default, so about a year at most. One more exception has the same limit: a held mail, or a document that had no profile yet when the backup ran, has its own key in the dump. If it went to the deleted profile later, that backup still opens it until retention removes the snapshot.

**The audit anchor.** Each backup also writes the audit head to `audit-head.json`, outside the database: in `OWNHEALTH_ANCHOR_DIR`, or `/data/anchor` on the data volume by default. The file is replaced atomically and only its owner can read it. The backup refuses a folder or file that is a symlink, or that another user or group may write. The next backup first checks that this entry of the live log still has the same hash. Someone with the database owner's rights can edit an entry and hash the whole log again, so that the log checks out on its own; the anchor catches that. When the check fails, the backup stops before it writes anything and exits 1 with the entry it expected, and the audit page on MCP & access shows the mismatch. Find out what changed first. If the database was restored from an older backup by hand (`ownhealth restore` resets the anchor itself, see Restore), that explains it. Then move `audit-head.json` aside and keep it as evidence: the next backup anchors the log as it is now. The first backup finds no anchor, checks nothing, and writes one. Two backups never move the anchor at once, and a backup only moves it forward. A log the page shows as broken, or that cannot be checked at all, does not stop the backups: the manifest records it, and the anchor stays where it was. A broken log is never anchored again, so the anchor stays at the last head that held. It moves on only once the whole log holds again, for example after the changed entry is put back as it was. Moving `audit-head.json` aside does not change that: the next backup then acts as a first backup, and a first backup anchors only a log that holds.

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
2. `ownhealth verify-restore` runs one check at a time per live database. It drops any database left at the reserved name `<live_db>_restore_check`, then creates it again and loads the dump.
3. For a backup with a keyring, it loads the profile keys from `keyring.json`. It compares the row count of every table with the manifest, hashes every blob again, rebuilds the state of every profile and compares each `state_hash` with the manifest. An encrypted file the manifest does not list counts as corrupt. It opens every profile's key with the configured secrets key (`keys` in the report). A profile deleted since the backup is listed under `keys.shredded`: its files stay closed, and that is not a failure. A key the keyring lacks without listing the profile as deleted is listed under `keys.missing`, and fails the check. It checks the restored audit log too: it must end at the head the manifest recorded, and hold as far as it held then.
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
  "blobs": {"checked": 27, "missing": [], "corrupt": []},
  "keys": {"checked": 4, "failed": [], "shredded": [], "missing": [], "ok": true}
}
```

`tables` lists only the tables whose counts differ. Pass a snapshot id to test an older one: `./deploy/restic/restore-test.sh 4be1a2c3`. It only reads the live database and blobs.

## Restore

Use this to restore onto a new machine, or after losing the volumes.

An older compose file keeps the keyring in `/data/keyring` on the data volume. Before you remove that volume, move `keyring.json` into the keyring folder of the current compose files (`keyring/<project>`).

```sh
cd /opt/ownhealth                 # same compose.yaml, .env and secrets/ as before; keyring/ from the box or its one off-box copy
docker compose down               # if anything still runs
docker volume rm ownhealth_ownhealth-data ownhealth_ownhealth-pg   # only when you mean to replace them
docker compose up -d db
./deploy/restic/restore.sh        # or: ./deploy/restic/restore.sh <snapshot id>
docker compose up -d
```

`restore.sh` runs in a one-off admin container before the app starts. Start a new restore with an empty target database and an empty blob store. If the backup names a secrets key fingerprint, use the secrets key that matches it. The restore reports the expected fingerprint before loading data if the key is missing or differs.

If the manifest includes `keys.keyring`, put the newest `keyring.json` in the Compose project's keyring folder when the backup needs profile keys. Use the copy on the box or its one copy off the box. The restore stops before loading data if a required key is missing and the keyring has no tombstone for it. Keyless backups from v0.4.0 and earlier restore without `keyring.json`.

When no newer copy exists (the disk died, or the copy is older than the backup), run it again with `OWNHEALTH_ALLOW_MISSING_KEYS=1 ./deploy/restic/restore.sh` (`ownhealth restore --allow-missing-keys`): the restore loads every record and copies the archived files. Files encrypted with unavailable profile keys stay unreadable, and the report lists those profiles under `keys.missing`. When the keyring holds another key for a profile than the one the backup needs, the profile keeps the keyring's key, because newer backups need it. The restore records files known to be sealed under unavailable profile keys in `.restore-losses.json` in the restored blob store. A new backup lists the accepted omissions in its manifest. Restoring that backup applies its omissions to the target blob store, even if a later backup recorded a repair. The restore test reports every other missing file, including missing plain files that belong to a deleted profile. Older manifests without this information can still be restored with matching key IDs. Later backups report any missing file that the restore could not identify as an accepted loss. When the restore says that `keyring.json` holds a damaged key for one profile and you have no undamaged copy, remove that profile's entry from `profiles` in the file and run the restore with `OWNHEALTH_ALLOW_MISSING_KEYS=1`: that profile's files stay unreadable, and everything else loads. Pending items can still open with the keys in the dump. A profile deleted since the backup comes back with its records, but its encrypted files stay closed (`keys.shredded` in the report). If the keyring holds a newer usable key for the profile, the profile keeps it and can accept new files. Otherwise, the restored profile with a shredded key cannot accept new files. It stays closed through any number of later backups and restores: each backup lists it under `keys.closed`, and a restore of that backup gives it the shredded key again. Delete it again in Settings to remove those records. After loading the dump and the blobs it rebuilds every profile's state and compares the `state_hash` with the manifest; the same hash means the restored data is the data you backed up. It exits 1 if anything differs. When everything matches and the backup's audit log held, it sets the audit anchor to the head of that backup (`audit_anchor` in its report), so the audit page and the next backup compare against the log as restored. It checks the anchor folder as a backup does: when the folder or file is a symlink, or another user or group may write it, the restore still succeeds but sets no anchor, and `audit_anchor_error` in its report says why.

A restore that records missing files saves a checkpoint with the profile keys, right after it loads the database. If it fails after that point, while recording the missing files or placing blobs, resolve the reported error and keep the app stopped. Rerun the same restore on the same target with the same snapshot, keyring contents, and restore options. The retry records the losses and places the files without loading the database again. A rerun with another snapshot, keyring or options stops before it loads anything. It stops with "holds no key for ... profile(s) of this backup" when the keyring lacks a key and `OWNHEALTH_ALLOW_MISSING_KEYS=1` is not set. Otherwise it finds no matching checkpoint and stops with "the target database is not empty". Run it again with the same snapshot, keyring and options.

Every other stopped restore has no checkpoint. When `pg_restore` itself failed, it loaded nothing (it runs as one transaction) and placed no files, so the target is still empty: resolve the error and run the same restore again. Any other stopped restore starts over: a restore with no missing files (a keyless backup too), or one that stopped between loading the database and saving the checkpoint. A rerun on the same target stops with "the target database is not empty" and loads nothing. Only then, empty the target and run the same restore again; the snapshot is not changed. With the Compose install:

```sh
docker compose down               # if anything still runs
docker volume rm ownhealth_ownhealth-data ownhealth_ownhealth-pg   # the stopped restore's database and blob store
docker compose up -d db
./deploy/restic/restore.sh <snapshot id>   # the first run's snapshot id
docker compose up -d
```

Repeat the first run's options too. If the first run set `OWNHEALTH_ALLOW_MISSING_KEYS=1`, use this line for the restore instead:

```sh
OWNHEALTH_ALLOW_MISSING_KEYS=1 ./deploy/restic/restore.sh <snapshot id>
```

With an older compose file, move `keyring.json` out of `/data/keyring` before you remove the data volume (see the start of this section).

Elsewhere, drop and recreate the database, and empty the blob folder (`OWNHEALTH_BLOB_DIR`, or `blobs` in the data folder).

Some untagged builds wrote keyed backups before the keyring or listed profile UUIDs without key IDs. Restore these backups with the build that wrote them, then take a new backup with this version.

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

**The keyring folder (from 1.0).** Both Compose services mount `./keyring/<project>`. For the default project, create `keyring/ownhealth` before you start Compose, as shown above. If you use `-p` or `COMPOSE_PROJECT_NAME`, use that project name in place of `ownhealth`. If Docker created the folder, give UID and GID 10001 ownership and set mode 0700.

If your old Compose file mounted `./keyring` directly, stop the old project before switching Compose files. Move its current `keyring/keyring.json` into `keyring/<project>/keyring.json` before starting the updated project. Move the file to the project that owns it. Take a backup right after the upgrade: it writes `keyring.json`. Backups taken before the upgrade still hold the profile keys in their dump, until retention removes them. Tell your host backup tools to skip `keyring/`.

**The secrets key (from 1.0).** OwnHealth now encrypts each profile's documents and genome files, so every command except `migrate` and `healthcheck` refuses to start without `OWNHEALTH_SECRETS_KEY` and prints one line that says how to make one. Keep the key you have: it already encrypts your inbox secrets, and a new one would lock them.

- Homelab: the new `compose.homelab.yaml` gives `secrets/secrets_key` to `admin` and `ocr` as well as the app (backups and restores open the encrypted files). The file exists since 0.3.0, so take the new compose file and nothing else changes.
- Quickstart (`compose.yaml`): the key now comes from the file `secrets/secrets_key`, never from `.env`. These steps keep a key file you already have, move the value from `.env` when there is no file, and make a new key only when neither has one. Then delete the `OWNHEALTH_SECRETS_KEY=` line from `.env`:

```sh
mkdir -p -m 700 secrets
if test -s secrets/secrets_key; then
  echo "secrets/secrets_key is there: keeping it"
elif grep -q '^OWNHEALTH_SECRETS_KEY=.' .env; then
  sed -n 's/^OWNHEALTH_SECRETS_KEY=//p' .env | tr -d '\n' > secrets/secrets_key
else
  openssl rand -base64 32 | tr -d '\n' > secrets/secrets_key   # neither had a key
fi
chmod 644 secrets/secrets_key
```

If OwnHealth then stops with "does not open ... of the ... profile keys", the key is not the one the install was made with. Nothing was changed: put the original key back in `secrets/secrets_key` (from the old `.env`, or from the copy you keep with the backups) and start again. A new key never opens files already stored.

Files stored before the upgrade stay where they are and open as before; files stored after it are encrypted. Take a backup after the upgrade and keep the key next to the restic password. Then encrypt the older files and connection tokens too, with a job that runs only when you start it:

```sh
docker compose run --rm -T admin ownhealth seal-legacy --dry-run   # what it would do; changes nothing
docker compose run --rm -T admin ownhealth seal-legacy
```

The job stores each older file under the key of every profile or pending item that reads it. It keeps the unencrypted copy if an ownerless reader still needs it or no record names it. The job also encrypts each older connection refresh token under its profile's key. It can run while OwnHealth runs, and a backup that runs at the same time keeps every file it needs. If you stop the job, run it again to resume.

If legacy passwords or tokens exist, `OWNHEALTH_SECRETS_KEY` must open at least one before the job writes anything. The command prints a JSON report and exits 1 if it could not encrypt a file or token. The report counts copies that landed before a later copy failed. The `errors` list identifies failed files by their SHA-256 and lists document IDs when available. If a file fails its SHA-256 check, restore it from a backup or retract its document, then run the job again.

Inbox records from before 1.0 did not say which profile they were for. The job gives each one the profile of its document, of the documents its released mail made, or of its inbox when the inbox is a profile's own, and then encrypts its file. Some files stay unencrypted, and the report counts them under `files.kept` and lists them under `files.kept_files`:

- `ownerless`: reference data, or a file of a household inbox from before 1.0 that no document or release ties to a profile (a mail with only a body, a scan that made no document).
- `unnamed`: a file no record names any more. Nothing reads it.

A run after a complete one reports the same kept files again. Backups taken before the job still hold the unencrypted files until retention removes them.

**To 0.4.0.** After the backup, set `admin` and `app` to `drumsergio/ownhealth:0.4.0` and `ocr` to `drumsergio/ownhealth:0.4.0-ocr`; nothing else in the compose files changes. Rule pack 0.2.0 moves every profile's `state_hash` once, so restore a backup made before the upgrade with 0.3.0, as above. For the same reason, `restore-test.sh` on a snapshot made before the upgrade reports every profile's `state_hash` as different; run `./deploy/restic/backup.sh` once after the upgrade, so the restore test checks a 0.4.0 snapshot. A household with a genome starts downloading NCBI's weekly ClinVar file (about 200 MB, no personal data in the request); turn it off in Settings, or set a local file path on a server without internet.

**To 0.3.0.** Take the new `compose.homelab.yaml` (it adds the `ocr` worker, the inbox folder and the `secrets_key` secret), then before `docker compose up -d`:

```sh
test -s secrets/secrets_key || openssl rand -base64 32 | tr -d '\n' > secrets/secrets_key   # keep a key you have
chmod 644 secrets/secrets_key
mkdir -p inbox && sudo chown 10001:10001 inbox
```

Compose refuses to start without the secret file. Keep a copy of it with the restic password.

Scans a 0.2.0 install kept as pending documents are read by OCR once you turn the `ocr` profile on: the worker queues them when it starts.
