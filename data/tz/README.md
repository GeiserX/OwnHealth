# Time zone link names

`backward` is the file of that name from the IANA time zone database, release 2026f
(https://data.iana.org/time-zones/releases/tzdata2026f.tar.gz, sha256
`01b4a91cec5167ed48c11e8ab78f2048cead78ce94b0cafcad0ae8c5a3044d3d`), unchanged. It is in the public
domain, as the LICENSE file of that release says.

Each `Link TARGET NAME` line maps an old or merged zone name to its current one. Browsers still report
some old names (Chromium says `Asia/Calcutta` for `Asia/Kolkata`), while the Debian Postgres image ships
only the current names. The CGM CSV importer (`ownhealth_devices::cgm_csv::known_zone`) keeps a name
Postgres knows, and reads a name Postgres lacks as its current one through this table. A name Postgres
knows stays as it is because the table also links zones merged since 1970 to the zone that kept their
rules (`Europe/Oslo` to `Europe/Berlin`), and a person in Oslo should read back `Europe/Oslo`. Two names
this table links are one zone: the same file uploaded again under the other name replaces nothing.

To update: replace `backward` with the file of a newer release, and change the release and the sha256
above.
