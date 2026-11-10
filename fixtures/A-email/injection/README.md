# Email injection fixtures (lane A, v0.2)

Synthetic messages for the email-in tests (`server/crates/ownhealth-inbox/tests/email.rs` and
`server/crates/ownhealth-api/tests/inbox_email.rs`). Each carries a `CANARY-EMAIL-*` string that must
never reach a document, a review item or a log, and must reach the API only inside an
`untrusted_text` envelope. The trusted receiving server in the tests is `mx.correo-salud.example`.

| File | What it tries | Expected |
|---|---|---|
| `inj-email-body-unknown-sender.eml` | An unknown sender's body claims it is allowlisted, names a profile and pastes an `Authentication-Results` line | Quarantined as `sender_not_allowed`; nothing parsed |
| `inj-email-forged-auth.eml` | LabNorte's address with an `Authentication-Results` header from another authserv-id, and a look-alike `X-` header | Quarantined as `auth_failed` |
| `inj-email-forged-auth-below.eml` | The receiving server's own header says DKIM and SPF failed; a copy with the same authserv-id below it says `dkim=pass` | Quarantined as `auth_failed`: only the topmost trusted header counts |
| `inj-email-subject.eml` | A subject with instructions and CR/LF to break out of a list | Quarantined; the subject is stored without control characters and served as `untrusted_text` |
| `inj-email-body-allowed.eml` | An allowlisted sender's hidden HTML body talks to an agent; the file name carries a canary | The PDF arrives; the body is not imported; the file name is only ever served as untrusted text |
