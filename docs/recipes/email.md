# Email

Labs and clinics that mail results can send them straight to OwnHealth. Give OwnHealth its own mailbox, allow the senders you trust, and their attachments arrive in the review queue. Mail from anyone else waits in quarantine, unread, until you release it.

## How it works

- Every minute OwnHealth reads one folder of the mailbox over IMAP (`INBOX` by default).
- A message from a sender on the mailbox's allowlist goes to that sender's profile. Its PDFs and health exports take the same path as an upload, and the values still go through review.
- Every other message is held in quarantine. OwnHealth stores it as received and does not read it until a person releases it under **Inbox**.
- An allowed sender must also be proven: your mail server must say that DKIM, SPF or DMARC passed for the sender's domain (next section).
- Handled mail is marked read and moved to the done folder (`OwnHealth/Done` by default). The same message seen again is not imported twice.

## You need

- A mailbox used only for health mail, with IMAP access. Do not point OwnHealth at your personal mailbox: every message from a sender you have not allowed would land in quarantine.
- OwnHealth 0.3.0 or newer, with `OWNHEALTH_SECRETS_KEY` set. The IMAP password and the PDF passwords are stored sealed with it.
- Your mail provider's IMAP host, port and TLS mode, and an app password if the provider asks for one.

## 1. Find your mail server's authserv-id

`From` is trivial to forge. OwnHealth trusts a sender only when your own mail server checked the message and wrote the result in an `Authentication-Results` header. That header begins with a name, the authserv-id, that says which server wrote it. A sender can write an `Authentication-Results` header too, so OwnHealth reads only the one that carries your server's authserv-id.

To find it:

1. Send a mail to the new mailbox from another account you own.
2. Open it in webmail and show the original message or the raw headers ("Show original", "View source", "Message source").
3. Find the topmost `Authentication-Results:` line. The first word after the colon, up to the `;`, is the authserv-id.

```text
Authentication-Results: mx.google.com;
       dkim=pass header.i=@labnorte.example ...
```

Here the authserv-id is `mx.google.com`, which Gmail writes. A self-hosted server writes its own host name or a name set in its configuration (for example in Rspamd, OpenDMARC or Stalwart).

Check two or three messages from different senders. OwnHealth matches one name exactly. If messages show different names (some providers put the name of whichever receiving host took the mail), only mail with the name you set can pass, and the rest is held as `auth_failed`.

Microsoft 365 and Outlook.com write an `Authentication-Results` header that starts with `spf=` and has no authserv-id, so OwnHealth cannot read their result. For such a mailbox, use another provider for the health mailbox, or read [What trust unauthenticated gives up](#what-trust-unauthenticated-gives-up) before you turn it on.

## 2. Check that your server strips forged headers

A sender can put a fake `Authentication-Results: <your authserv-id>; dkim=pass ...` into a message. OwnHealth reads only the topmost header with your authserv-id, which is the one your server added on arrival, so a fake lower copy is ignored. The fake copy counts only when your server adds no header of its own to that message. Most servers remove such headers on arrival (RFC 8601 section 5). Check yours once:

1. From another account, send the mailbox a message with an extra header `Authentication-Results: <your authserv-id>; dkim=pass header.d=labnorte.example`. With `swaks` this is `--add-header`. Many mail clients cannot add headers.
2. Look at the raw message in the mailbox. Either the fake header is gone, or your server's own `Authentication-Results` sits above it. Both are fine.
3. If the fake header is the topmost one with your authserv-id, your server does not protect you. Do not rely on the allowlist with this server, and ask your provider or fix the server's configuration.

## 3. The mailbox in OwnHealth

In OwnHealth, open **Inbox**, add a source of kind **Email in** and fill in:

| Field | Example | Notes |
|---|---|---|
| Profile | Alex | The default profile; each allowed sender names its own |
| Host, port, TLS | `imap.correo.example`, `993`, implicit | `starttls` uses port 143. `none` is refused unless the host is on your LAN or the same machine |
| Username and password | `salud.rivera@correo.example` | The password is stored encrypted and never shown again |
| Folder to read | `INBOX` | |
| Move processed mail to | `OwnHealth/Done` | See the next section if your server uses an `INBOX.` prefix |
| Trusted authserv-id | `mx.google.com` | The name from step 1 |

### The done folder on servers with an `INBOX.` prefix

Some servers (Courier, and Dovecot set up with a namespace prefix) keep every folder under `INBOX` and separate levels with a dot: `INBOX.Sent`, `INBOX.Archive`. On such a server `OwnHealth/Done` cannot be created. Each run then fails with an error that names the done folder, and handled mail stays in the folder it was read from. OwnHealth still imports each message once, and reads new (unread) mail before the handled mail left behind, but fix the folder name so the source turns green again.

Look at the folder names your mail client shows. If they start with `INBOX.`, set **Move processed mail to** to `INBOX.OwnHealth` (or another name under `INBOX.`). OwnHealth creates the folder if it is missing.

## 4. Allow your senders

Under the mailbox, add each sender you trust with the profile their mail belongs to:

- An address (`resultados@labnorte.example`) allows that address only. A domain (`labnorte.example`) allows every address at it. An address rule wins over a domain rule.
- DKIM, SPF or DMARC proves the domain, not the person. An address rule assumes the sender's mail service lets each account send only as its own address. The large providers enforce this. If the sender's domain lets one account send as another address, the address rule in effect trusts the whole domain.

The other way to allow a sender is from quarantine: open the held message and pick **Release and allow sender**. This needs the trusted authserv-id to be set. A message held as `auth_failed` can be released once, but the screen does not offer to allow its sender, because its `From` address proves nothing.

### What trust unauthenticated gives up

A sender rule with **Unauthenticated OK** is accepted when your server wrote no result for it, or when the mailbox has no trusted authserv-id at all. Then the only check is the `From` address, and anyone can forge that. A stranger who knows which lab you use can send you a PDF "from" that lab, and it goes straight to the profile instead of quarantine. The values still wait in review, but nothing tells you the mail was not from the lab.

Turn it on only when your mail server adds no `Authentication-Results` and you cannot change that, and only for the senders you need. A message your server marked as failed is held as `auth_failed` even then.

## 5. PDF passwords

Many Spanish portals send PDFs locked with a password, often derived from the patient's ID document number. Add the password to the sender's rule (**PDF password**). OwnHealth tries only that sender's passwords, in the order you added them, and never a list shared by all senders. The locked original is kept, and the unlocked copy is imported with a link to it. Passwords are stored encrypted and never appear in logs or errors.

A locked PDF that none of the passwords opened is kept and shown in the review queue as "Locked PDF: no stored password opened it". When you then add the password to that sender's rule, OwnHealth opens the sender's locked PDFs still waiting in the review queue at once and imports their unlocked copies. A locked PDF you released from **Quarantine** has no sender rule, so it is not reopened: open it with the password yourself and upload it under **Import & review**.

## 6. Check it

Send a mail with a lab PDF from an allowed sender. Within two minutes it is in the review queue, and the mailbox in **Inbox** shows a successful run. Send one from an address that is not allowed: it shows under **Quarantine** with the reason `sender_not_allowed`.

If allowed mail is held as `auth_failed`:

- The trusted authserv-id does not match what your server writes (step 1), or the field is empty.
- The sender's domain failed DKIM and SPF at your server. Ask the sender, or release the message once.

To alert on a stalled mailbox, see [Monitoring the inboxes](../../deploy/README.md#monitoring-the-inboxes).

## Notes

- Limits: 25 MB per attachment and 50 attachments per message. A message over 64 MB is not downloaded. Its header is held as `too_large`, and the message is in the done folder for you to save by hand.
- The text of a mail is kept inside the stored message and is not imported.
- A message with more than one `From` address is never routed to a profile.
