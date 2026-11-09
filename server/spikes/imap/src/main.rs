//! cargo run -- [imap host:port] [smtp host:port]   (GreenMail: 127.0.0.1:3143 and 127.0.0.1:3025)
//!
//! 1. sends one synthetic LabNorte mail with a PDF attachment over SMTP;
//! 2. logs in over IMAP, selects INBOX, UID FETCHes the whole message, parses it;
//! 3. creates the Done folder, sets \Seen, UID MOVEs the message there, and checks both folders.
use std::time::Instant;

use anyhow::{Context, Result, bail};
use futures::TryStreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const PDF: &[u8] = b"%PDF-1.4\n% synthetic LabNorte slip for Alex Rivera\n%%EOF\n";

async fn smtp_send(addr: &str, msg: &str) -> Result<()> {
    let stream = TcpStream::connect(addr).await?;
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut line = String::new();
    async fn expect(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>, line: &mut String, code: &str) -> Result<()> {
        loop {
            line.clear();
            r.read_line(line).await?;
            if !line.starts_with(code) {
                bail!("smtp: want {code}, got {line:?}");
            }
            if line.as_bytes().get(3) == Some(&b' ') {
                return Ok(());
            }
        }
    }
    expect(&mut r, &mut line, "220").await?;
    for (cmd, code) in [
        ("EHLO spike.local\r\n", "250"),
        ("MAIL FROM:<resultados@labnorte.example>\r\n", "250"),
        ("RCPT TO:<inbox@ownhealth.test>\r\n", "250"),
        ("DATA\r\n", "354"),
    ] {
        w.write_all(cmd.as_bytes()).await?;
        expect(&mut r, &mut line, code).await?;
    }
    w.write_all(msg.as_bytes()).await?;
    w.write_all(b"\r\n.\r\n").await?;
    expect(&mut r, &mut line, "250").await?;
    w.write_all(b"QUIT\r\n").await?;
    Ok(())
}

fn message() -> String {
    use std::fmt::Write;
    let b64 = {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in PDF.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    };
    let mut m = String::new();
    let _ = write!(
        m,
        "Authentication-Results: mx.ownhealth.test; dkim=pass header.d=labnorte.example; spf=pass smtp.mailfrom=labnorte.example\r\n\
Message-ID: <spike-0001@labnorte.example>\r\n\
From: LabNorte Resultados <resultados@labnorte.example>\r\n\
To: inbox@ownhealth.test\r\n\
Subject: =?UTF-8?Q?Resultados_anal=C3=ADticos_-_Alex_Rivera?=\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n\
--b1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n\
Adjuntamos su informe. La contraseña es su DNI.\r\n\
--b1\r\nContent-Type: application/pdf; name=\"informe.pdf\"\r\nContent-Disposition: attachment; filename=\"informe.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\n{b64}\r\n--b1--\r\n"
    );
    m
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let imap_addr = args.first().map_or("127.0.0.1:3143", |s| s.as_str());
    let smtp_addr = args.get(1).map_or("127.0.0.1:3025", |s| s.as_str());
    smtp_send(smtp_addr, &message()).await.context("smtp")?;

    let t = Instant::now();
    let tcp = TcpStream::connect(imap_addr).await?;
    let client = async_imap::Client::new(tcp);
    let mut s = client
        .login("inbox", "inbox-pass-x1")
        .await
        .map_err(|(e, _)| anyhow::anyhow!("login: {e}"))?;
    let caps = s.capabilities().await?;
    let has_move = caps.has_str("MOVE");
    let has_idle = caps.has_str("IDLE");
    let mbox = s.select("INBOX").await?;
    println!("INBOX exists={} uidvalidity={:?} MOVE={has_move} IDLE={has_idle}", mbox.exists, mbox.uid_validity);

    let uids: Vec<u32> = s.uid_search("UNSEEN").await?.into_iter().collect();
    println!("unseen uids {uids:?}");
    let uid = *uids.first().context("no mail")?;
    let fetched: Vec<_> = s.uid_fetch(uid.to_string(), "(UID RFC822.SIZE BODY.PEEK[])").await?.try_collect().await?;
    let raw = fetched.first().and_then(|f| f.body()).context("no body")?.to_vec();
    let size = fetched.first().and_then(|f| f.size);
    let parsed = mail_parser::MessageParser::default().parse(&raw).context("parse")?;
    let from = parsed.from().and_then(|a| a.first()).and_then(|a| a.address()).unwrap_or_default();
    let auth: Vec<String> = parsed.headers().all("Authentication-Results").map(|h| String::from_utf8_lossy(h.raw_value()).trim().to_string())
        .collect();
    println!("size={size:?} from={from} subject={:?} message_id={:?}", parsed.subject(), parsed.message_id());
    println!("authentication-results={auth:?}");
    println!("body={:?}", parsed.body_text(0));
    for a in parsed.attachments() {
        println!(
            "attachment name={:?} type={:?} bytes={} same={}",
            a.attachment_name(),
            a.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or(""))),
            a.decoded().len(),
            a.decoded().as_ref() == PDF
        );
    }

    // still unseen: BODY.PEEK does not set \Seen
    let still: Vec<u32> = s.uid_search("UNSEEN").await?.into_iter().collect();
    println!("after PEEK unseen={still:?}");

    let done = "OwnHealth/Done";
    if let Err(e) = s.create(done).await {
        println!("create {done}: {e} (exists already?)");
    }
    let _: Vec<_> = s.uid_store(uid.to_string(), "+FLAGS (\\Seen)").await?.try_collect().await?;
    if has_move {
        s.uid_mv(uid.to_string(), done).await?;
    } else {
        s.uid_copy(uid.to_string(), done).await?;
        let _: Vec<_> = s.uid_store(uid.to_string(), "+FLAGS (\\Deleted)").await?.try_collect().await?;
        let _: Vec<_> = s.expunge().await?.try_collect().await?;
    }
    let inbox = s.select("INBOX").await?;
    let moved = s.select(done).await?;
    let seen: Vec<u32> = s.uid_search("SEEN").await?.into_iter().collect();
    println!("after move INBOX exists={} Done exists={} Done seen uids={seen:?}", inbox.exists, moved.exists);
    s.logout().await?;
    println!("round trip {:.3}s", t.elapsed().as_secs_f64());
    Ok(())
}
