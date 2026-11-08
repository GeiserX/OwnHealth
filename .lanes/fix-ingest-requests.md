# Requests from the fix-ingest lane

Status: all done in the 0.2.0 gate (feat/rust-backend).

## To the owner of ownhealth-api: cap the `profile` field and the part counts in POST /api/imports

Finding (medium): `server/crates/ownhealth-api/src/routes/imports.rs` `upload()` reads the `profile`
field with `field.text().await` under `DefaultBodyLimit::disable()`, so a multi-GB `profile` field is
buffered whole, before any permission check, and the 404 echoes it back. Measured: a 200 MB field took
RSS from 805 MB to 982 MB and returned a 200,000,124-byte body. It also accepts any number of parts.
0.1.1 (Starlette 1.10 MultiPartParser, formparsers.py:79-113) had max_part_size=1 MiB,
max_fields=1000, max_files=1000 and answered 400 at 1 MB.

Fix asked for (this lane does not own the file, so nothing was changed there):
- Read `profile` with `field.chunk()` into a `Vec<u8>`; once the total passes `1024 * 1024` bytes,
  return `bad_body()` (400 `bad_request` "There was an error parsing the body"). Decode as UTF-8 at
  the end (`String::from_utf8`, error -> `bad_body()`).
- Count parts: more than 1000 file parts (a part with a filename) or more than 1000 non-file parts ->
  `bad_body()`. Keep file parts uncapped in size and keep skipping unknown fields without buffering.
- API test: post a 2 MiB `profile` field plus one small `files` part, expect 400 with that message;
  watch it fail (404 echoing the value) on the current code first.

Sketch:
```rust
const MAX_PART: usize = 1024 * 1024;
const MAX_PARTS: usize = 1000;
let (mut file_parts, mut field_parts) = (0usize, 0usize);
// in the loop, before the match:
if field.file_name().is_some() { file_parts += 1 } else { field_parts += 1 }
if file_parts > MAX_PARTS || field_parts > MAX_PARTS { return Err(bad_body()); }
// "profile":
let mut buf = Vec::new();
while let Some(c) = field.chunk().await.map_err(|_| bad_body())? {
    if buf.len() + c.len() > MAX_PART { return Err(bad_body()); }
    buf.extend_from_slice(&c);
}
profile = Some(String::from_utf8(buf).map_err(|_| bad_body())?);
```
