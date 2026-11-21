# SMART Health Card photo fixtures (slice 22)

Synthetic only. The cards are for the made-up Rivera family, signed with throwaway test keys for an
issuer under `.example`. No real person, card or key is in them.

| File | What it is |
|---|---|
| `vaccine-photo.jpg` | A printed vaccination card photographed on a desk: rotated 7°, a slight keystone, uneven light, blur, sensor noise, JPEG. 1 QR code. |
| `vaccine-photo.jws` | The card that photo holds. |
| `three-code-sheet.jpg` | The same kind of photo of one card split into 3 QR codes (the deprecated chunked form), side by side. |
| `three-code-sheet.jws` | The card those 3 codes hold together. |

How they were made: the QR codes were drawn with the `qrcode` npm package from the `.jws` text. A
Python script with Pillow pasted them on a paper card, rotated it onto a wood-grain desk, applied the
keystone, a light gradient, a Gaussian blur and random pixel noise, and saved a JPEG at quality 78.
Each photo was then saved again in grayscale at quality 55 to keep it small.

The receiver tests in `server/crates/ownhealth-ingest-structured/tests/shc.rs` read both photos.
