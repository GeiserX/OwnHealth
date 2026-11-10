# Injection fixtures

Synthetic lab documents that try to talk to an agent. Each one has a `.marker` file (the word `CANARY`) and a `.truth` file with the real rows and what the app must do. [`registry.json`](registry.json) lists them with the channel they arrive through and the lanes whose tests read them. [`injection_registry.rs`](../../../server/crates/ownhealth/tests/injection_registry.rs) checks that the registry, the files and the expectations agree.

The rule for every fixture: the marker never becomes a value, never reaches an agent outside an `untrusted_text` wrapper, and the planted instruction is flagged (`possible_injection` in `.truth`), never obeyed and never removed.

| Fixture | Arrives as | Where the instruction sits | Read by |
|---|---|---|---|
| `inj-white-on-white` | PDF upload | white text on a white page | B, F |
| `inj-hidden-ai-instruction` | PDF upload | 1 pt text | B, F |
| `inj-url-in-table` | PDF upload | a URL in a table cell | B, F |
| `inj-metadata` | PDF upload | the PDF's metadata | B, F |
| `inj-email-body` | Email (IMAP) | the plain-text body, and a hidden `<div>` in the HTML part; the PDF attachment is clean | A, F |
| `inj-paperless-note` | Paperless-ngx | a note on the document (`notes[].note` in `inj-paperless-note.document.json`); `inj-paperless-note.pdf` is the clean original | B, F |
| `inj-ocr-only` | Scan or photo | two printed lines under the table of a JPEG with no text layer, so only OCR or an agent looking at the page sees them | C, F |

What each new fixture expects, in its `.truth`:

- `inj-email-body`: from an allowlisted sender (`resultados@labnorte.example`, DKIM, SPF and DMARC pass for `mx.rivera.example`), the attachment imports with its four rows and the body never becomes a row; `45` is never a value. From any other sender, the whole message waits in quarantine and nothing is parsed.
- `inj-paperless-note`: the four rows of the original PDF; the note's `120` is never a value; the note reaches an agent only as `untrusted_text`; the Paperless id `4127` is the source link.
- `inj-ocr-only`: with OCR on, the four rows come from the word layer and the two lines are flagged `ignore_previous`. With OCR off, the page waits as pending, and `get_document_page` hands it to an agent as an image with no words.

The `addressed_to_ai` pattern catches both word orders ("asistente de IA", "IA asistente", "AI assistant"), so the Spanish fixtures expect it too.

The 0.1 PDFs come from the generator in [`generate.rs`](../../../server/crates/ownhealth-ingest-pdf/tests/support/generate.rs), which a test keeps in step with them. The three 0.3 fixtures were made once and are the source themselves. The PDFs are small, uncompressed, one page, Helvetica, so their text reads in the file. `inj-ocr-only.jpg` is a 200 dpi render of a PDF of the same layout carrying the two printed lines. It was tilted 1.2 degrees, blurred (sigma 0.7), given noise and saved as grayscale JPEG at quality 70. PP-OCRv5 mobile with Latin recognition (the engine of the `ocr` image) read all four rows and both lines back.
