//! Outbound HTTP for the few places OwnHealth fetches anything: a client's CIMD document (lane E), a
//! Paperless original (lane B), a Nightscout site (lane D), and the POSTs of a connector (a token or
//! refresh request, an SHL manifest). This is the address half of connectors.md 6.3 rule 7 and
//! security-privacy.md 16; the declared-host check sits in the connector client (slice 13). Every
//! name is resolved first and every address must be public, unless the person ticked "on my
//! network" for that host (`allow_private`); the connection then goes to exactly the addresses we
//! checked (no second lookup, so no DNS rebinding, and no proxy from the environment), redirects
//! are not followed, and the body is capped in bytes and time.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// How one fetch may go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Egress {
    /// Private, loopback and link-local addresses allowed (a Paperless or Nightscout at home).
    pub allow_private: bool,
    /// Only `https://` (CIMD documents, and a home service on a public address). Home services may
    /// be plain http on the LAN.
    pub https_only: bool,
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl Egress {
    /// A CIMD document: https only, public addresses only, 5 s, 64 KiB.
    pub fn cimd() -> Egress {
        Egress {
            allow_private: false,
            https_only: true,
            timeout: Duration::from_secs(5),
            max_bytes: 64 * 1024,
        }
    }

    /// A service the person runs (Paperless, Nightscout): 30 s, 50 MiB. Plain http only when it is
    /// on the person's network (`allow_private`, and every address private): its API token goes in
    /// every request, and on a public address it would cross the internet in clear.
    pub fn home_service(allow_private: bool) -> Egress {
        Egress {
            allow_private,
            https_only: !allow_private,
            timeout: Duration::from_secs(30),
            max_bytes: 50 * 1024 * 1024,
        }
    }

    /// A connector vendor's small answers (a token, refresh or revoke request, an SHL manifest):
    /// https only, public addresses only, 15 s, 1 MiB. A data pull sets its own cap by struct update
    /// (`Egress { max_bytes: .., ..Egress::connector() }`): 30 days of Dexcom readings is about
    /// 2.6 MB. Like every profile it never follows a redirect, so a token request is never sent on
    /// to another host.
    pub fn connector() -> Egress {
        Egress {
            allow_private: false,
            https_only: true,
            timeout: Duration::from_secs(15),
            max_bytes: 1024 * 1024,
        }
    }

    /// A public reference file streamed to disk (the ClinVar weekly VCF, about 200 MB): 1 hour, 1 GiB.
    /// `allow_private` for a mirror on the person's network; plain http only there.
    pub fn reference_file(allow_private: bool) -> Egress {
        Egress {
            allow_private,
            https_only: !allow_private,
            timeout: Duration::from_secs(3600),
            max_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// Why a fetch did not happen or did not finish. Messages carry the host, never a body or a header.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetError {
    #[error("not a valid http(s) URL")]
    BadUrl,
    #[error("only https URLs are allowed here (plain http only for a service on a private address you allowed)")]
    NotHttps,
    #[error("{0} resolves to a private, loopback or link-local address")]
    PrivateAddress(String),
    #[error("{0} does not resolve")]
    Resolve(String),
    #[error("the response is larger than {0} bytes")]
    TooLarge(usize),
    #[error("{0} did not answer in time")]
    Timeout(String),
    #[error("request to {host} failed: {message}")]
    Http { host: String, message: String },
    #[error("could not write the download: {0}")]
    Write(String),
}

impl From<NetError> for crate::Error {
    fn from(e: NetError) -> Self {
        crate::Error::Other(anyhow::anyhow!(e))
    }
}

/// A finished response: status, the headers a caller needs, the capped body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub status: u16,
    pub content_type: Option<String>,
    pub cache_control: Option<String>,
    pub body: Vec<u8>,
}

fn v4_public(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1]))   // 100.64.0.0/10 shared address space (CGNAT, Tailscale)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)       // 192.0.0.0/24 protocol assignments
        || (o[0] == 198 && (18..20).contains(&o[1]))     // 198.18.0.0/15 benchmarking
        || o[0] >= 240)
}

fn v6_public(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_public(v4);
    }
    let s = ip.segments();
    // 64:ff9b::/96 NAT64 carries an IPv4 address in its last 32 bits
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return v4_public(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || (s[0] == 0x2001 && s[1] == 0x0db8)           // documentation
        || (s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0)) // ::/96 (IPv4-compatible)
}

/// True when the address is on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_public(v4),
        IpAddr::V6(v6) => v6_public(v6),
    }
}

/// Resolve `host:port` and check every address. One private address refuses the whole name.
pub async fn resolve_checked(host: &str, port: u16, allow_private: bool) -> Result<Vec<SocketAddr>, NetError> {
    let addrs: Vec<SocketAddr> = match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| NetError::Resolve(host.to_string()))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(NetError::Resolve(host.to_string()));
    }
    if !allow_private && addrs.iter().any(|a| !is_public(a.ip())) {
        return Err(NetError::PrivateAddress(host.to_string()));
    }
    Ok(addrs)
}

/// Send one request under `egress`: the checks every fetch shares. `body` is the content type and the
/// bytes of a POST. Returns the response and the host, for error messages.
async fn send(
    method: reqwest::Method,
    url: &str,
    egress: &Egress,
    headers: &[(&str, &str)],
    body: Option<(&str, Vec<u8>)>,
) -> Result<(reqwest::Response, String), NetError> {
    let parsed = url::Url::parse(url).map_err(|_| NetError::BadUrl)?;
    match parsed.scheme() {
        "https" => {}
        "http" if !egress.https_only => {}
        "http" => return Err(NetError::NotHttps),
        _ => return Err(NetError::BadUrl),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(NetError::BadUrl);
    }
    let host = parsed.host_str().ok_or(NetError::BadUrl)?.to_string();
    // reqwest appends a header, never replaces it: a caller's copy of these would go out twice or
    // disagree with the body
    for (k, _) in headers {
        let k = k.to_ascii_lowercase();
        if ["content-type", "content-length", "host", "transfer-encoding"].contains(&k.as_str()) {
            return Err(NetError::Http {
                host,
                message: format!("the {k} header is set here, not by the caller"),
            });
        }
    }
    let port = parsed.port_or_known_default().ok_or(NetError::BadUrl)?;
    let addrs = resolve_checked(&host, port, egress.allow_private).await?;
    // "on my network" allows plain http to the person's network only: a token sent to a public
    // address would cross the internet in clear
    if parsed.scheme() == "http" && addrs.iter().any(|a| is_public(a.ip())) {
        return Err(NetError::NotHttps);
    }
    let http = |e: reqwest::Error| body_error(&host, e);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // a proxy would look the name up again and connect past the addresses we checked
        .no_proxy()
        .timeout(egress.timeout)
        .resolve_to_addrs(&host, &addrs)
        .build()
        .map_err(http)?;
    let mut req = client.request(method, parsed.clone());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Some((content_type, bytes)) = body {
        req = req.header(reqwest::header::CONTENT_TYPE, content_type).body(bytes);
    }
    let resp = req.send().await.map_err(http)?;
    if resp.content_length().is_some_and(|n| n > egress.max_bytes as u64) {
        return Err(NetError::TooLarge(egress.max_bytes));
    }
    Ok((resp, host))
}

fn body_error(host: &str, e: reqwest::Error) -> NetError {
    if e.is_timeout() {
        NetError::Timeout(host.to_string())
    } else {
        NetError::Http {
            host: host.to_string(),
            message: e.without_url().to_string(),
        }
    }
}

/// GET `url` under `egress`, with extra request headers (a token for the person's own service).
pub async fn get(url: &str, egress: &Egress, headers: &[(&str, &str)]) -> Result<Fetched, NetError> {
    let (resp, host) = send(reqwest::Method::GET, url, egress, headers, None).await?;
    read(resp, &host, egress).await
}

/// POST `fields` as `application/x-www-form-urlencoded` under `egress` (an OAuth token or refresh
/// request). The content type is set here; `headers` adds the rest (`accept`, a basic auth).
pub async fn post_form(
    url: &str,
    egress: &Egress,
    headers: &[(&str, &str)],
    fields: &[(&str, &str)],
) -> Result<Fetched, NetError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    let body = Some(("application/x-www-form-urlencoded", body.into_bytes()));
    let (resp, host) = send(reqwest::Method::POST, url, egress, headers, body).await?;
    read(resp, &host, egress).await
}

/// POST `body` as `application/json` under `egress` (an SHL manifest request).
pub async fn post_json(
    url: &str,
    egress: &Egress,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
) -> Result<Fetched, NetError> {
    let body = Some(("application/json", body.to_string().into_bytes()));
    let (resp, host) = send(reqwest::Method::POST, url, egress, headers, body).await?;
    read(resp, &host, egress).await
}

/// Read a response into a [`Fetched`], cutting the body at `egress.max_bytes`.
async fn read(mut resp: reqwest::Response, host: &str, egress: &Egress) -> Result<Fetched, NetError> {
    let header =
        |name: reqwest::header::HeaderName| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(String::from);
    let content_type = header(reqwest::header::CONTENT_TYPE);
    let cache_control = header(reqwest::header::CACHE_CONTROL);
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| body_error(host, e))? {
        if body.len() + chunk.len() > egress.max_bytes {
            return Err(NetError::TooLarge(egress.max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Fetched {
        status,
        content_type,
        cache_control,
        body,
    })
}

/// GET `url` under `egress` into a new file at `dest`, chunk by chunk, so memory stays flat whatever
/// the size (a ClinVar release is about 200 MB). Only a 200 answer is written; any other status is
/// returned with nothing written. Returns the status and the bytes written. On an error the partial
/// file is left for the caller to delete.
pub async fn download(url: &str, egress: &Egress, dest: &std::path::Path) -> Result<(u16, u64), NetError> {
    use tokio::io::AsyncWriteExt;
    let (mut resp, host) = send(reqwest::Method::GET, url, egress, &[], None).await?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Ok((status, 0));
    }
    let write = |e: std::io::Error| NetError::Write(e.kind().to_string());
    let mut file = tokio::fs::File::create(dest).await.map_err(write)?;
    let mut written: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| body_error(&host, e))? {
        written += chunk.len() as u64;
        if written > egress.max_bytes as u64 {
            return Err(NetError::TooLarge(egress.max_bytes));
        }
        file.write_all(&chunk).await.map_err(write)?;
    }
    file.flush().await.map_err(write)?;
    Ok((status, written))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    #[test]
    fn private_loopback_link_local_and_mapped_addresses_are_not_public() {
        for s in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.10.100",
            "127.0.0.1",
            "169.254.169.254",
            "100.100.1.1",
            "0.0.0.0",
            "198.18.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip(s)), "{s} counted as public");
        }
        for s in [
            "93.184.215.14",
            "2606:4700::1111",
            "::ffff:93.184.215.14",
            "64:ff9b::5db8:d70e",
        ] {
            assert!(is_public(ip(s)), "{s} counted as private");
        }
    }

    #[tokio::test]
    async fn a_private_literal_is_refused_unless_allowed_and_no_request_is_sent() {
        let e = get("https://127.0.0.1:9/client.json", &Egress::cimd(), &[])
            .await
            .expect_err("refused");
        assert_eq!(e, NetError::PrivateAddress("127.0.0.1".into()));
        assert_eq!(
            get("http://example.com/x", &Egress::cimd(), &[]).await,
            Err(NetError::NotHttps)
        );
        assert_eq!(
            get("https://user:pw@example.com/x", &Egress::cimd(), &[]).await,
            Err(NetError::BadUrl)
        );
        // "on my network" allows plain http to a private address, never to a public one
        assert_eq!(
            get("http://93.184.215.14/api/", &Egress::home_service(true), &[]).await,
            Err(NetError::NotHttps)
        );
        assert!(resolve_checked("127.0.0.1", 80, true).await.is_ok());
        assert!(resolve_checked("localhost", 80, false).await.is_err());
    }

    /// A token for a Paperless or Nightscout on a public address never goes out over plain http.
    #[tokio::test]
    async fn a_home_service_on_a_public_address_needs_https() {
        let public = Egress::home_service(false);
        assert_eq!(
            get(
                "http://paperless.example.com/api/",
                &public,
                &[("authorization", "Token t")]
            )
            .await,
            Err(NetError::NotHttps)
        );
        assert!(Egress::home_service(true).https_only.eq(&false));
    }

    #[tokio::test]
    async fn the_body_is_capped_and_redirects_are_not_followed() -> anyhow::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            for answer in [
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}".to_string(),
                format!("HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n{}", "x".repeat(5000)),
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n".to_string(),
            ] {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(answer.as_bytes()).await;
            }
        });
        let home = Egress {
            max_bytes: 1000,
            ..Egress::home_service(true)
        };
        let url = format!("http://127.0.0.1:{port}/");
        let ok = get(&url, &home, &[("Authorization", "Token t")]).await?;
        assert_eq!((ok.status, ok.body.as_slice()), (200, &b"{}"[..]));
        assert_eq!(ok.content_type.as_deref(), Some("application/json"));
        assert_eq!(get(&url, &home, &[]).await, Err(NetError::TooLarge(1000)));
        assert_eq!(get(&url, &home, &[]).await?.status, 302);
        Ok(())
    }

    #[tokio::test]
    async fn a_download_streams_to_a_file_and_writes_nothing_but_a_200() -> anyhow::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let big = "v".repeat(300_000);
        let answers = [
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{big}", big.len()),
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found".to_string(),
            "HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n".to_string() + &"x".repeat(5000),
        ];
        tokio::spawn(async move {
            for answer in answers {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(answer.as_bytes()).await;
            }
        });
        let dir = tempfile::tempdir()?;
        let url = format!("http://127.0.0.1:{port}/clinvar.vcf.gz");
        let dest = dir.path().join("release");
        let egress = Egress::reference_file(true);
        assert_eq!(download(&url, &egress, &dest).await?, (200, 300_000));
        assert_eq!(std::fs::read(&dest)?.len(), 300_000);
        let missing = dir.path().join("missing");
        assert_eq!(download(&url, &egress, &missing).await?, (404, 0));
        assert!(!missing.exists(), "a 404 body is never written");
        let small = Egress {
            max_bytes: 1000,
            ..egress
        };
        assert_eq!(
            download(&url, &small, &dir.path().join("big")).await,
            Err(NetError::TooLarge(1000))
        );
        // Public addresses only by default, and https only there.
        assert_eq!(
            download(&url, &Egress::reference_file(false), &dest).await,
            Err(NetError::NotHttps)
        );
        Ok(())
    }

    /// A fake vendor: reads each request in full (head and body), hands it to the test, then answers.
    async fn fake(answers: Vec<String>) -> anyhow::Result<(u16, tokio::sync::mpsc::UnboundedReceiver<String>)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for answer in answers {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let Ok(n) = s.read(&mut buf).await else { return };
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().to_string())
                            })
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&req).to_string());
                let _ = s.write_all(answer.as_bytes()).await;
            }
        });
        Ok((port, rx))
    }

    /// Token, refresh and SHL manifest requests: https only, public addresses only, 15 s, 1 MiB.
    #[tokio::test]
    async fn a_connector_post_goes_only_to_a_public_https_address() {
        let c = Egress::connector();
        assert_eq!(
            (c.allow_private, c.https_only, c.timeout, c.max_bytes),
            (false, true, Duration::from_secs(15), 1024 * 1024)
        );
        let form = [("grant_type", "refresh_token"), ("refresh_token", "fake-refresh")];
        assert_eq!(
            post_form("https://127.0.0.1:9/token", &c, &[], &form).await,
            Err(NetError::PrivateAddress("127.0.0.1".into()))
        );
        assert_eq!(
            post_form("https://localhost:9/token", &c, &[], &form).await,
            Err(NetError::PrivateAddress("localhost".into()))
        );
        assert_eq!(
            post_json("https://[fd00::1]:9/manifest", &c, &[], &serde_json::json!({})).await,
            Err(NetError::PrivateAddress("[fd00::1]".into()))
        );
        assert_eq!(
            post_json("http://example.com/manifest", &c, &[], &serde_json::json!({})).await,
            Err(NetError::NotHttps)
        );
    }

    /// The form and JSON bodies arrive encoded, a redirect is not followed (the secret in the body is
    /// never sent on to another host), and an oversize answer is cut.
    #[tokio::test]
    async fn a_post_sends_its_body_once_and_the_answer_is_capped() -> anyhow::Result<()> {
        let (port, mut seen) = fake(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}".to_string(),
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_string(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n{}", "x".repeat(5000)),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1388\r\n".to_string()
                + &"y".repeat(5000)
                + "\r\n0\r\n\r\n",
        ])
        .await?;
        let other = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let other_port = other.local_addr()?.port();
        let (redirect_port, _redirect_seen) = fake(vec![format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{other_port}/token\r\nContent-Length: 0\r\n\r\n"
        )])
        .await?;
        // the fake is on loopback over http, so the test widens only those two fields
        let lan = Egress {
            allow_private: true,
            https_only: false,
            max_bytes: 1000,
            ..Egress::connector()
        };
        let url = format!("http://127.0.0.1:{port}/token");
        let ok = post_form(
            &url,
            &lan,
            &[("accept", "application/json")],
            &[
                ("grant_type", "authorization_code"),
                ("code", "a b&c"),
                ("redirect_uri", "https://app.test/cb"),
            ],
        )
        .await?;
        assert_eq!((ok.status, ok.body.as_slice()), (200, &b"{}"[..]));
        let req = seen.recv().await.expect("form request");
        assert!(req.starts_with("POST /token "), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded\r\n"),
            "{req}"
        );
        assert!(
            req.ends_with(
                "\r\n\r\ngrant_type=authorization_code&code=a+b%26c&redirect_uri=https%3A%2F%2Fapp.test%2Fcb"
            ),
            "{req}"
        );

        let body = serde_json::json!({"recipient": "Alex Rivera", "passcode": "fake-1234"});
        assert_eq!(post_json(&url, &lan, &[], &body).await?.body, b"ok");
        let req = seen.recv().await.expect("json request");
        assert!(req.starts_with("POST /token "), "{req}");
        assert!(
            req.to_ascii_lowercase().contains("content-type: application/json\r\n"),
            "{req}"
        );
        assert!(
            req.ends_with(r#"{"recipient":"Alex Rivera","passcode":"fake-1234"}"#),
            "{req}"
        );

        // cut on the declared length and on the streamed length
        assert_eq!(post_form(&url, &lan, &[], &[]).await, Err(NetError::TooLarge(1000)));
        assert_eq!(post_form(&url, &lan, &[], &[]).await, Err(NetError::TooLarge(1000)));

        let moved = post_form(
            &format!("http://127.0.0.1:{redirect_port}/token"),
            &lan,
            &[],
            &[("client_secret", "fake-secret")],
        )
        .await?;
        assert_eq!(moved.status, 307);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), other.accept())
                .await
                .is_err(),
            "the redirect target was contacted"
        );
        Ok(())
    }

    /// A vendor that never answers costs the timeout, not a stuck poller.
    #[tokio::test]
    async fn a_post_to_a_silent_vendor_times_out() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let hold = tokio::spawn(async move {
            let kept = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(kept);
        });
        let quick = Egress {
            allow_private: true,
            https_only: false,
            timeout: Duration::from_millis(300),
            ..Egress::connector()
        };
        let started = std::time::Instant::now();
        assert_eq!(
            post_form(&format!("http://127.0.0.1:{port}/token"), &quick, &[], &[]).await,
            Err(NetError::Timeout("127.0.0.1".into()))
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        hold.abort();
        Ok(())
    }

    /// A proxy from the environment would resolve the name again, past our checks (DNS rebinding,
    /// private addresses). The proxy variables are process-wide, so the check runs in a child copy of
    /// this test binary started with every proxy pointing at a closed port.
    #[tokio::test]
    async fn the_proxy_environment_is_ignored() -> anyhow::Result<()> {
        const CHILD: &str = "OWNHEALTH_NET_PROXY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let port = listener.local_addr()?.port();
            tokio::spawn(async move {
                if let Ok((mut s, _)) = listener.accept().await {
                    let mut buf = [0u8; 2048];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\ndirect").await;
                }
            });
            let ok = get(&format!("http://127.0.0.1:{port}/"), &Egress::home_service(true), &[]).await?;
            assert_eq!(ok.body, b"direct");
            return Ok(());
        }
        let name = "net::tests::the_proxy_environment_is_ignored";
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.args(["--exact", name, "--test-threads=1"]).env(CHILD, "1");
        for k in ["NO_PROXY", "no_proxy"] {
            cmd.env_remove(k);
        }
        for k in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            cmd.env(k, "http://127.0.0.1:1");
        }
        let out = cmd.output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }

    /// The helper sets the content type and the framing headers; a caller's copy would go out as a
    /// second header, or disagree with the body. It is refused before any lookup or connection.
    #[tokio::test]
    async fn a_caller_header_that_clashes_with_the_body_is_refused_and_nothing_is_sent() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let lan = Egress {
            allow_private: true,
            https_only: false,
            ..Egress::connector()
        };
        let url = format!("http://127.0.0.1:{port}/token");
        for name in ["Content-Type", "content-length", "HOST", "Transfer-Encoding"] {
            assert_eq!(
                post_form(&url, &lan, &[(name, "x")], &[("grant_type", "refresh_token")]).await,
                Err(NetError::Http {
                    host: "127.0.0.1".into(),
                    message: format!(
                        "the {} header is set here, not by the caller",
                        name.to_ascii_lowercase()
                    ),
                })
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err(),
            "a request went out"
        );
        Ok(())
    }

    /// The timeout covers the whole exchange, body included: a vendor that sends its headers and
    /// then one byte every 100 ms is cut at the timeout, not kept alive byte by byte.
    #[tokio::test]
    async fn a_slow_drip_body_is_cut_at_the_total_timeout() -> anyhow::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let drip = tokio::spawn(async move {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            if s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 30\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if s.write_all(b"x").await.is_err() {
                    return;
                }
            }
        });
        let quick = Egress {
            allow_private: true,
            https_only: false,
            timeout: Duration::from_millis(300),
            ..Egress::connector()
        };
        let started = std::time::Instant::now();
        assert_eq!(
            post_form(&format!("http://127.0.0.1:{port}/token"), &quick, &[], &[]).await,
            Err(NetError::Timeout("127.0.0.1".into()))
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drip.abort();
        Ok(())
    }
}
