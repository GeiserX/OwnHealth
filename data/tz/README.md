# Time zone link names

`backward` is the file of that name from the IANA time zone database, release 2026f
(https://data.iana.org/time-zones/releases/tzdata2026f.tar.gz, sha256
`01b4a91cec5167ed48c11e8ab78f2048cead78ce94b0cafcad0ae8c5a3044d3d`), unchanged. It is in the public
domain, as the LICENSE file of that release says.

Each `Link TARGET NAME` line maps an old or merged zone name to its current one. Browsers still report
some old names (Chromium says `Asia/Calcutta` for `Asia/Kolkata`), while the Debian Postgres image ships
only the current names. The CGM CSV importer (`ownhealth_devices::cgm_csv::known_zone`) reads an old
name as its current one through this table, also on a Postgres that knows both names, so one zone always
has one name.

To update: replace `backward` with the file of a newer release, and change the release and the sha256
above.
