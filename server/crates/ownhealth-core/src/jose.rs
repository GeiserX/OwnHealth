//! Compact JOSE for SMART Health Links and SMART Health Cards: the one place OwnHealth reads and
//! writes a JWE and verifies a JWS (connectors.md, the SHC and SHL row; security-privacy.md 11).
//!
//! Two shapes, and nothing else:
//! - JWE with `alg: dir` and `enc: A256GCM` (RFC 7516, RFC 7518 4.5 and 5.3), the shape of every
//!   SHL file: a 32-byte key, a 96-bit IV, a 128-bit tag, the protected header as the AAD.
//! - JWS with `alg: ES256` (RFC 7515, RFC 7518 3.4), verify only: the signature of a SMART Health
//!   Card, raw `r || s`, over a key from the issuer's JWKS.
//!
//! Either may carry `zip: DEF` (raw DEFLATE, RFC 1951), which SHC always and SHL sometimes uses.
//! Every other `alg`, `enc` or `zip` is refused, `none` included, and so is any `crit` header:
//! there is no extension this code understands. The inflated (or plain) payload has a cap the
//! caller names, so a small compressed file cannot grow without bound.
//!
//! The header values a sender wrote never appear in an error: each error names only the field.
//! Built on the RustCrypto `aes-gcm` and `p256` crates and `flate2`, with no JOSE crate.

use std::io::Read as _;
use std::io::Write as _;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Verifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use serde_json::{Map, Value};

/// A decoded protected header.
pub type Header = Map<String, Value>;

/// The `dir` + `A256GCM` content key length.
pub const KEY_LEN: usize = 32;
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Why a JWE or JWS was refused. A wrong key, a changed tag, ciphertext, IV or header all give
/// [`JoseError::Decrypt`]; no detail tells them apart.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JoseError {
    #[error("not a compact JOSE object: {0}")]
    Malformed(&'static str),
    /// An `alg`, `enc` or `zip` other than the allowed one, or a `crit` header. Names the field.
    #[error("unsupported JOSE header field: {0}")]
    Unsupported(&'static str),
    #[error("the content does not decrypt with this key")]
    Decrypt,
    #[error("the signature does not verify with this key")]
    BadSignature,
    /// The payload, after inflate, is over the caller's cap (the cap in bytes).
    #[error("the payload is larger than {0} bytes")]
    TooLarge(usize),
    #[error("not a P-256 signing key: {0}")]
    Key(&'static str),
}

impl From<JoseError> for crate::error::Error {
    fn from(e: JoseError) -> Self {
        crate::error::Error::Invalid(e.to_string())
    }
}

/// A JWE that opened: its protected header (`cty` says what the plaintext is) and the plaintext.
/// Its Debug shows the header and the plaintext length only: the plaintext is health data.
pub struct Decrypted {
    pub header: Header,
    pub plaintext: Vec<u8>,
}

impl std::fmt::Debug for Decrypted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decrypted")
            .field("header", &self.header)
            .field("plaintext_len", &self.plaintext.len())
            .finish()
    }
}

/// A JWS that verified: its protected header and the payload, inflated when `zip` was `DEF`.
/// Its Debug shows the header and the payload length only: the payload is health data.
pub struct Verified {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for Verified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verified")
            .field("header", &self.header)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

/// A compact JWE (`dir`, `A256GCM`) of `plaintext`, with a fresh random IV. `cty` goes in the
/// header when given; `deflate` compresses first and sets `zip: DEF`.
pub fn encrypt(key: &[u8; KEY_LEN], plaintext: &[u8], cty: Option<&str>, deflate: bool) -> Result<String, JoseError> {
    let mut header = Header::new();
    header.insert("alg".into(), "dir".into());
    header.insert("enc".into(), "A256GCM".into());
    if let Some(cty) = cty {
        header.insert("cty".into(), cty.into());
    }
    let body = if deflate {
        header.insert("zip".into(), "DEF".into());
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(plaintext)
            .and_then(|()| enc.finish())
            .map_err(|_| JoseError::Malformed("the plaintext did not deflate"))?
    } else {
        plaintext.to_vec()
    };
    let protected = URL_SAFE_NO_PAD.encode(Value::Object(header).to_string());
    let mut iv = [0u8; IV_LEN];
    rand::fill(&mut iv[..]);
    let sealed = Aes256Gcm::new_from_slice(key)
        .map_err(|_| JoseError::Key("the content key is not 32 bytes"))?
        .encrypt(
            &Nonce::from(iv),
            Payload {
                msg: &body,
                aad: protected.as_bytes(),
            },
        )
        .map_err(|_| JoseError::Malformed("the plaintext is too long for AES-GCM"))?;
    let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_LEN);
    Ok(format!(
        "{protected}..{}.{}.{}",
        URL_SAFE_NO_PAD.encode(iv),
        URL_SAFE_NO_PAD.encode(ciphertext),
        URL_SAFE_NO_PAD.encode(tag)
    ))
}

/// Open a compact JWE with `alg: dir`, `enc: A256GCM` and optional `zip: DEF`. The plaintext, after
/// inflate, must fit in `max_len` bytes.
pub fn decrypt(key: &[u8; KEY_LEN], compact: &str, max_len: usize) -> Result<Decrypted, JoseError> {
    let [protected, encrypted_key, iv, ciphertext, tag] = split::<5>(compact, "a JWE has five parts")?;
    let header = parse_header(protected)?;
    if header.get("alg").and_then(Value::as_str) != Some("dir") {
        return Err(JoseError::Unsupported("alg"));
    }
    if header.get("enc").and_then(Value::as_str) != Some("A256GCM") {
        return Err(JoseError::Unsupported("enc"));
    }
    let deflate = zip_of(&header)?;
    if !encrypted_key.is_empty() {
        return Err(JoseError::Malformed("dir carries no encrypted key"));
    }
    let iv: [u8; IV_LEN] = b64(iv)?
        .try_into()
        .map_err(|_| JoseError::Malformed("the IV is not 96 bits"))?;
    let tag = b64(tag)?;
    if tag.len() != TAG_LEN {
        return Err(JoseError::Malformed("the tag is not 128 bits"));
    }
    let mut msg = b64(ciphertext)?;
    msg.extend_from_slice(&tag);
    let body = Aes256Gcm::new_from_slice(key)
        .map_err(|_| JoseError::Key("the content key is not 32 bytes"))?
        .decrypt(
            &Nonce::from(iv),
            Payload {
                msg: &msg,
                aad: protected.as_bytes(),
            },
        )
        .map_err(|_| JoseError::Decrypt)?;
    let plaintext = capped(body, deflate, max_len)?;
    Ok(Decrypted { header, plaintext })
}

/// The protected header of a compact JWS, NOT verified: only for picking the key by `kid` (and the
/// issuer by the payload) before [`verify_es256`].
pub fn jws_header(compact: &str) -> Result<Header, JoseError> {
    let [protected, _, _] = split::<3>(compact, "a JWS has three parts")?;
    parse_header(protected)
}

/// Verify a compact JWS with `alg: ES256` against `key` and return its payload, inflated when the
/// header says `zip: DEF`. The payload must fit in `max_len` bytes. A high-S signature verifies:
/// RFC 7518 has no low-S rule and WebCrypto signers emit one about half the time.
pub fn verify_es256(compact: &str, key: &VerifyingKey, max_len: usize) -> Result<Verified, JoseError> {
    let [protected, payload, signature] = split::<3>(compact, "a JWS has three parts")?;
    let header = parse_header(protected)?;
    if header.get("alg").and_then(Value::as_str) != Some("ES256") {
        return Err(JoseError::Unsupported("alg"));
    }
    let deflate = zip_of(&header)?;
    let signature = b64(signature).map_err(|_| JoseError::BadSignature)?;
    let signature = Signature::from_slice(&signature).map_err(|_| JoseError::BadSignature)?;
    let signing_input = &compact[..protected.len() + 1 + payload.len()];
    key.verify(signing_input.as_bytes(), &signature)
        .map_err(|_| JoseError::BadSignature)?;
    let payload = capped(b64(payload)?, deflate, max_len)?;
    Ok(Verified { header, payload })
}

/// The P-256 public key of a JWK from an issuer's JWKS: `kty: EC`, `crv: P-256`, and `alg`, when
/// present, `ES256`. The point must be on the curve.
pub fn es256_key(jwk: &Value) -> Result<VerifyingKey, JoseError> {
    let field = |name| jwk.get(name).and_then(Value::as_str);
    if !jwk.is_object() {
        return Err(JoseError::Key("a JWK is a JSON object"));
    }
    if field("kty") != Some("EC") {
        return Err(JoseError::Key("kty is not EC"));
    }
    if field("crv") != Some("P-256") {
        return Err(JoseError::Key("crv is not P-256"));
    }
    if jwk.get("alg").is_some() && field("alg") != Some("ES256") {
        return Err(JoseError::Key("alg is not ES256"));
    }
    let coordinate = |name| {
        field(name)
            .and_then(|text| URL_SAFE_NO_PAD.decode(text).ok())
            .filter(|raw| raw.len() == 32)
            .ok_or(JoseError::Key("x and y are 32-byte base64url"))
    };
    let mut sec1 = vec![0x04];
    sec1.extend(coordinate("x")?);
    sec1.extend(coordinate("y")?);
    VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| JoseError::Key("the point is not on P-256"))
}

fn split<'a, const N: usize>(compact: &'a str, what: &'static str) -> Result<[&'a str; N], JoseError> {
    let parts: Vec<&str> = compact.split('.').collect();
    parts.try_into().map_err(|_| JoseError::Malformed(what))
}

fn b64(text: &str) -> Result<Vec<u8>, JoseError> {
    URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| JoseError::Malformed("not base64url"))
}

/// A protected header: base64url of a JSON object, with no `crit`.
fn parse_header(protected: &str) -> Result<Header, JoseError> {
    let Ok(Value::Object(header)) = serde_json::from_slice(&b64(protected)?) else {
        return Err(JoseError::Malformed("the header is not a JSON object"));
    };
    if header.contains_key("crit") {
        return Err(JoseError::Unsupported("crit"));
    }
    Ok(header)
}

/// Whether the header asks for DEFLATE: `zip` absent, or exactly `DEF`.
fn zip_of(header: &Header) -> Result<bool, JoseError> {
    match header.get("zip") {
        None => Ok(false),
        Some(Value::String(zip)) if zip == "DEF" => Ok(true),
        Some(_) => Err(JoseError::Unsupported("zip")),
    }
}

/// The payload, inflated when asked, refused once it passes `max_len`. Inflate reads at most one
/// byte past the cap, so a bomb costs `max_len` of memory, not its full size.
fn capped(body: Vec<u8>, deflate: bool, max_len: usize) -> Result<Vec<u8>, JoseError> {
    let out = if deflate {
        let mut out = Vec::new();
        flate2::read::DeflateDecoder::new(body.as_slice())
            .take((max_len as u64).saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|_| JoseError::Malformed("zip DEF content is not DEFLATE"))?;
        out
    } else {
        body
    };
    if out.len() > max_len {
        return Err(JoseError::TooLarge(max_len));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::SigningKey;
    use p256::ecdsa::signature::Signer as _;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    const MIB: usize = 1024 * 1024;

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn key32(text: &str) -> [u8; KEY_LEN] {
        let raw = URL_SAFE_NO_PAD.decode(text).expect("test key");
        raw.try_into().expect("32 bytes")
    }

    fn parts(compact: &str) -> Vec<String> {
        compact.split('.').map(str::to_string).collect()
    }

    /// A compact JWE made by hand with this header, so a test can put any `alg` or `enc` in it.
    fn jwe_with_header(key: &[u8; KEY_LEN], header: Value, plaintext: &[u8]) -> String {
        let protected = b64(&serde_json::to_vec(&header).expect("header"));
        let iv = [9u8; IV_LEN];
        let cipher = Aes256Gcm::new_from_slice(key).expect("key");
        let sealed = cipher
            .encrypt(
                &Nonce::from(iv),
                Payload {
                    msg: plaintext,
                    aad: protected.as_bytes(),
                },
            )
            .expect("seal");
        let (ct, tag) = sealed.split_at(sealed.len() - TAG_LEN);
        format!("{protected}..{}.{}.{}", b64(&iv), b64(ct), b64(tag))
    }

    fn raw_deflate(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(data).expect("deflate");
        enc.finish().expect("deflate")
    }

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_slice(&[seed; 32]).expect("scalar")
    }

    fn jwk_of(key: &SigningKey) -> Value {
        let point = key.verifying_key().to_sec1_point(false);
        let bytes = point.as_bytes();
        json!({"kty": "EC", "crv": "P-256", "alg": "ES256", "x": b64(&bytes[1..33]), "y": b64(&bytes[33..65])})
    }

    /// A compact JWS over `payload` with this header, signed by `key`.
    fn jws(key: &SigningKey, header: Value, payload: &[u8]) -> String {
        let input = format!(
            "{}.{}",
            b64(&serde_json::to_vec(&header).expect("header")),
            b64(payload)
        );
        let sig: Signature = key.sign(input.as_bytes());
        format!("{input}.{}", b64(&sig.to_bytes()))
    }

    // An independent implementation: Node 26 WebCrypto AES-GCM and zlib.deflateRawSync, with this
    // generated test key (s10-tools/gen.mjs in the PR notes). Not a real key.
    const NODE_KEY: &str = "mj-YeNin93ugOih2Es5pnn1gt1LTUF8TmfeCV-9rmMo";
    const NODE_PLAINTEXT: &str = r#"{"resourceType":"Bundle","type":"collection","entry":[{"resource":{"resourceType":"Patient","name":[{"family":"Rivera","given":["Alex"]}]}}]}"#;
    const NODE_JWE_DEF: &str = "eyJhbGciOiJkaXIiLCJlbmMiOiJBMjU2R0NNIiwiY3R5IjoiYXBwbGljYXRpb24vZmhpcitqc29uIiwiemlwIjoiREVGIn0..pFsV8AY133TPkM7H.KMd1ewIhuZJoMY6EifGU4McrDXdZF7wl2qYI4juflMKAzMTtHc6PdsEjuzGHgZHe4cTE2hd8rBRu70FGLrS7k7FUBS_jjszx5IeYwQ9tVd9NpUukyJ6du_tgAB_Bno1Ft9lj9k4zH0GWNw.M0h-mPudcgPnIOcYQIZF6A";
    const NODE_JWE_RAW: &str = "eyJhbGciOiJkaXIiLCJlbmMiOiJBMjU2R0NNIiwiY3R5IjoiYXBwbGljYXRpb24vZmhpcitqc29uIn0..1zHoM4t-UxKsVvJX.mbJDpk39Qj8f_1noCWhInwU-A0ZDw98HY500Wzl6g-H_XgV0zWpmju0vpkU4eQsaccGnpSWaUVpTPgAyYbZBnl7RxYg46WqLpF4PN0eeaAixKRpP238qiKsDFH1mUAoqFEiUDuV36PiHck1G9t3XutxwGz2xpjPbIk5DFGDmCoSGORjmnbVjPL668eSz.cRBXmTWLJWZnCm9r0LrcUg";

    // The SMART Health Cards spec example card and its issuer key (published, public):
    // https://spec.smarthealth.cards/examples/example-00-d-jws.txt and
    // https://spec.smarthealth.cards/examples/issuer/.well-known/jwks.json
    const SHC_JWS: &str = "eyJ6aXAiOiJERUYiLCJhbGciOiJFUzI1NiIsImtpZCI6IjNLZmRnLVh3UC03Z1h5eXd0VWZVQUR3QnVtRE9QS01ReC1pRUxMMTFXOXMifQ.3ZJNj9MwEIb_ymq45sMJbUJzo0XiSyAQy15QD64zbYwcO7KdqmWV_87YzYoV2uUCSIjcxp55530f5xakc9BA5_3gmjx3A4rM9dz6DrnyXSa4bV2OJ94PCl1O3SNaSEDv9tAU1bN6sWR1tciqZZXAUUBzC_48IDRffmj-LPfkUqShgG0CwmKL2kuuPo27ryh8UNl30t6gddJo8rfIWFbQ2nC6HnWrMPRYdGa0Aq_jRpgvktkBCKMUqQWFBGiBPZMtUh6V-mwVNdzNN4wa7ooHhD9wL2k-xOY9XkR4LxXpwXNNPdbFHQd5RB2ivzFdqNcZbCcKuJME4AX3QatYLYuUFWnJYJqSB90Uv3bzuu9HLb_xOZjz3I8uxg1v5LGlwyMXQmrcmDYqCNNKfYjG3dl57Ocnp9fpVJ0Ze8gD2dzJNhfHEwmIOAklq2HaTgkMM4JoZ48WdfB2nyA1GSFGG69C2GvZXyTKGJiFWIRqb2xPv1DwwoU3Nki20g2KR5zrzdVL1Gi5unpl3CA9VwSKICrj34_9LowCi1_xKMHyvyRYrv40wfpRgk__IYKU-_cJlilbpWz5N_7BLUEEK1s6fPf2vDl19VCNH-niOw.OGZq1bUTz4wSCCFEBFInGsfTwaNWEpjAvmMIL4OayNev5ZzvLuy1tpLYKZ_ab4z6sQJv5gpRfz_V59eVj2BrPA";
    /// sha256 of the spec's example-00-c-jws-payload-minified.json, the inflated payload.
    const SHC_PAYLOAD_SHA256: &str = "0f4ea19f9c797d921ab8607b6f0eb1dc54cdf9293f271a86a883c1149f696b14";

    fn shc_issuer_jwk() -> Value {
        json!({"kty": "EC", "kid": "3Kfdg-XwP-7gXyywtUfUADwBumDOPKMQx-iELL11W9s", "use": "sig", "alg": "ES256",
               "crv": "P-256", "x": "11XvRWy1I2S0EyJlyf_bWfw_TQ5CJJNLw78bHXNxcgw",
               "y": "eZXwxvO1hvCY0KucrPfKo7yAyMT6Ajc3N7OkAB6VYy8"})
    }

    /// The other key in the same published JWKS.
    fn shc_other_jwk() -> Value {
        json!({"kty": "EC", "kid": "EBKOr72QQDcTBUuVzAzkfBTGew0ZA16GuWty64nS-sw", "alg": "ES256", "crv": "P-256",
               "x": "PQHApUWm94mflvswQgAnfHlETMwJFqjUVSs7WU6LQy4", "y": "7mj8IO-8V5VZjDbRVsJINC_Rq5ai5CDhFX18ceRsLWQ"})
    }

    // ---- JWE ----

    #[test]
    fn jwe_round_trips_with_and_without_deflate() {
        let key = [42u8; KEY_LEN];
        let body = br#"{"resourceType":"Bundle"}"#.repeat(50);
        for deflate in [false, true] {
            let compact = encrypt(&key, &body, Some("application/fhir+json"), deflate).expect("encrypt");
            let p = parts(&compact);
            assert_eq!(p.len(), 5);
            assert!(p[1].is_empty(), "dir has no encrypted key");
            let out = decrypt(&key, &compact, MIB).expect("decrypt");
            assert_eq!(out.plaintext, body);
            assert_eq!(out.header["alg"], "dir");
            assert_eq!(out.header["enc"], "A256GCM");
            assert_eq!(out.header["cty"], "application/fhir+json");
            assert_eq!(out.header.get("zip").is_some(), deflate);
        }
        // a fresh IV every time
        assert_ne!(
            encrypt(&key, b"x", None, false).expect("a"),
            encrypt(&key, b"x", None, false).expect("b")
        );
    }

    #[test]
    fn debug_shows_the_header_and_the_length_never_the_health_data() {
        // "Rivera" as the bytes a derived Debug of a Vec<u8> prints
        let rivera = "82, 105, 118, 101, 114, 97";
        let key = key32(NODE_KEY);
        let opened = format!("{:?}", decrypt(&key, NODE_JWE_RAW, MIB).expect("decrypt"));
        assert!(!opened.contains(rivera), "{opened}");
        assert!(!opened.contains("Rivera"), "{opened}");
        assert!(opened.contains("application/fhir+json"), "{opened}");
        assert!(opened.contains(&format!("plaintext_len: {}", NODE_PLAINTEXT.len())), "{opened}");
        let signer = signing_key(5);
        let vk = es256_key(&jwk_of(&signer)).expect("jwk");
        let compact = jws(&signer, json!({"alg": "ES256", "kid": "k1"}), NODE_PLAINTEXT.as_bytes());
        let verified = format!("{:?}", verify_es256(&compact, &vk, MIB).expect("verify"));
        assert!(!verified.contains(rivera), "{verified}");
        assert!(!verified.contains("Rivera"), "{verified}");
        assert!(verified.contains("k1"), "{verified}");
        assert!(verified.contains(&format!("payload_len: {}", NODE_PLAINTEXT.len())), "{verified}");
    }

    #[test]
    fn jwe_made_by_node_webcrypto_decrypts() {
        let key = key32(NODE_KEY);
        for compact in [NODE_JWE_DEF, NODE_JWE_RAW] {
            let out = decrypt(&key, compact, MIB).expect("decrypt");
            assert_eq!(out.plaintext, NODE_PLAINTEXT.as_bytes());
            assert_eq!(out.header["cty"], "application/fhir+json");
        }
    }

    #[test]
    fn jwe_refuses_a_wrong_key() {
        let mut key = key32(NODE_KEY);
        key[0] ^= 1;
        assert_eq!(decrypt(&key, NODE_JWE_DEF, MIB).err(), Some(JoseError::Decrypt));
    }

    #[test]
    fn jwe_refuses_a_changed_tag_ciphertext_iv_or_header() {
        let key = key32(NODE_KEY);
        let flip = |text: &str| {
            let mut raw = URL_SAFE_NO_PAD.decode(text).expect("b64");
            raw[0] ^= 0x80;
            b64(&raw)
        };
        for i in [0, 2, 3, 4] {
            let mut p = parts(NODE_JWE_RAW);
            if i == 0 {
                // same alg and enc, another cty: still a valid header, but the AAD changed
                p[0] = b64(br#"{"alg":"dir","enc":"A256GCM","cty":"text/plain"}"#);
            } else {
                p[i] = flip(&p[i]);
            }
            assert_eq!(
                decrypt(&key, &p.join("."), MIB).err(),
                Some(JoseError::Decrypt),
                "part {i}"
            );
        }
        // a truncated tag is not a tag
        let mut p = parts(NODE_JWE_RAW);
        p[4] = b64(&[0u8; 12]);
        assert!(matches!(decrypt(&key, &p.join("."), MIB), Err(JoseError::Malformed(_))));
    }

    #[test]
    fn jwe_refuses_an_iv_that_is_not_96_bits() {
        let key = key32(NODE_KEY);
        let real = URL_SAFE_NO_PAD.decode(parts(NODE_JWE_RAW)[2].as_str()).expect("b64");
        // the real IV with 4 more bytes would open if the IV were cut to 12 bytes
        let mut long = real.clone();
        long.extend_from_slice(&[0u8; 4]);
        for iv in [long, real[..8].to_vec(), vec![0u8; 16], Vec::new()] {
            let mut p = parts(NODE_JWE_RAW);
            p[2] = b64(&iv);
            assert_eq!(
                decrypt(&key, &p.join("."), MIB).err(),
                Some(JoseError::Malformed("the IV is not 96 bits")),
                "{} bytes",
                iv.len()
            );
        }
    }

    #[test]
    fn jwe_refuses_every_alg_enc_and_zip_but_dir_a256gcm_def() {
        let key = [7u8; KEY_LEN];
        let cases = [
            (json!({"alg": "none", "enc": "A256GCM"}), "alg"),
            (json!({"enc": "A256GCM"}), "alg"),
            (json!({"alg": "A256KW", "enc": "A256GCM"}), "alg"),
            (json!({"alg": "DIR", "enc": "A256GCM"}), "alg"),
            (json!({"alg": "dir", "enc": "A128GCM"}), "enc"),
            (json!({"alg": "dir", "enc": "A256CBC-HS512"}), "enc"),
            (json!({"alg": "dir"}), "enc"),
            (json!({"alg": "dir", "enc": "A256GCM", "zip": "GZIP"}), "zip"),
            (
                json!({"alg": "dir", "enc": "A256GCM", "crit": ["exp"], "exp": 1}),
                "crit",
            ),
        ];
        for (header, field) in cases {
            let compact = jwe_with_header(&key, header.clone(), b"hello");
            assert_eq!(
                decrypt(&key, &compact, MIB).err(),
                Some(JoseError::Unsupported(field)),
                "{header}"
            );
        }
        // dir carries no encrypted key
        let compact = jwe_with_header(&key, json!({"alg": "dir", "enc": "A256GCM"}), b"hello");
        let mut p = parts(&compact);
        p[1] = b64(&[1u8; 40]);
        assert!(matches!(decrypt(&key, &p.join("."), MIB), Err(JoseError::Malformed(_))));
        // not five parts, not base64url, not a JSON object
        for bad in [
            "a.b.c",
            "",
            "....",
            "e30.....",
            "!!..AAAA.AAAA.AAAA",
            "WzFd..AAAAAAAAAAAAAAAA.AA.AAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(matches!(decrypt(&key, bad, MIB), Err(JoseError::Malformed(_))), "{bad}");
        }
    }

    #[test]
    fn jwe_inflate_bomb_stops_at_the_cap() {
        let key = [3u8; KEY_LEN];
        let bomb = raw_deflate(&vec![0u8; 64 * MIB]);
        assert!(bomb.len() < 100 * 1024, "the bomb is small on the wire: {}", bomb.len());
        let compact = jwe_with_header(&key, json!({"alg": "dir", "enc": "A256GCM", "zip": "DEF"}), &bomb);
        assert_eq!(decrypt(&key, &compact, MIB).err(), Some(JoseError::TooLarge(MIB)));
        // the cap also holds without zip, and exactly at the cap is fine
        let plain = encrypt(&key, &[1u8; 100], None, false).expect("encrypt");
        assert_eq!(decrypt(&key, &plain, 99).err(), Some(JoseError::TooLarge(99)));
        assert_eq!(decrypt(&key, &plain, 100).expect("at the cap").plaintext.len(), 100);
        let def = encrypt(&key, &[1u8; 100], None, true).expect("encrypt");
        assert_eq!(decrypt(&key, &def, 99).err(), Some(JoseError::TooLarge(99)));
        assert_eq!(decrypt(&key, &def, 100).expect("at the cap").plaintext.len(), 100);
        // DEF that is not DEFLATE
        let junk = jwe_with_header(&key, json!({"alg": "dir", "enc": "A256GCM", "zip": "DEF"}), &[0xff; 32]);
        assert!(matches!(decrypt(&key, &junk, MIB), Err(JoseError::Malformed(_))));
    }

    #[test]
    fn a_cap_of_usize_max_inflates_everything_and_never_wraps_to_zero() {
        // `max_len + 1` overflowed here: a panic in debug, and in release a take(0) that returned
        // an empty plaintext as if it had opened.
        let key = [3u8; KEY_LEN];
        let body = br#"{"resourceType":"Bundle"}"#.repeat(20);
        let compact = encrypt(&key, &body, None, true).expect("encrypt");
        assert_eq!(decrypt(&key, &compact, usize::MAX).expect("decrypt").plaintext, body);
        let signer = signing_key(5);
        let vk = es256_key(&jwk_of(&signer)).expect("jwk");
        let jws = jws(&signer, json!({"alg": "ES256", "zip": "DEF"}), &raw_deflate(&body));
        assert_eq!(verify_es256(&jws, &vk, usize::MAX).expect("verify").payload, body);
    }

    // ---- JWS ----

    #[test]
    fn shc_spec_example_verifies_with_its_issuer_key() {
        let header = jws_header(SHC_JWS).expect("header");
        assert_eq!(header["kid"], "3Kfdg-XwP-7gXyywtUfUADwBumDOPKMQx-iELL11W9s");
        let key = es256_key(&shc_issuer_jwk()).expect("jwk");
        let out = verify_es256(SHC_JWS, &key, MIB).expect("verify");
        assert_eq!(hex::encode(Sha256::digest(&out.payload)), SHC_PAYLOAD_SHA256);
        let claims: Value = serde_json::from_slice(&out.payload).expect("json");
        assert_eq!(claims["iss"], "https://spec.smarthealth.cards/examples/issuer");
        assert_eq!(
            claims["vc"]["credentialSubject"]["fhirBundle"]["resourceType"],
            "Bundle"
        );
    }

    #[test]
    fn shc_spec_example_fails_with_the_other_key_or_any_changed_part() {
        let other = es256_key(&shc_other_jwk()).expect("jwk");
        assert_eq!(verify_es256(SHC_JWS, &other, MIB).err(), Some(JoseError::BadSignature));
        let key = es256_key(&shc_issuer_jwk()).expect("jwk");
        let mut p = parts(SHC_JWS);
        let mut raw = URL_SAFE_NO_PAD.decode(&p[1]).expect("b64");
        raw[10] ^= 1;
        p[1] = b64(&raw);
        assert_eq!(
            verify_es256(&p.join("."), &key, MIB).err(),
            Some(JoseError::BadSignature),
            "payload"
        );
        let mut p = parts(SHC_JWS);
        p[0] = b64(br#"{"zip":"DEF","alg":"ES256","kid":"another"}"#);
        assert_eq!(
            verify_es256(&p.join("."), &key, MIB).err(),
            Some(JoseError::BadSignature),
            "header"
        );
        let mut p = parts(SHC_JWS);
        let mut sig = URL_SAFE_NO_PAD.decode(&p[2]).expect("b64");
        sig[63] ^= 1;
        p[2] = b64(&sig);
        assert_eq!(
            verify_es256(&p.join("."), &key, MIB).err(),
            Some(JoseError::BadSignature),
            "signature"
        );
        // r = 0, s = 0, a short signature, and DER instead of raw r||s
        for bad in [vec![0u8; 64], vec![1u8; 63], vec![0x30, 0x44, 0x02, 0x20]] {
            let mut p = parts(SHC_JWS);
            p[2] = b64(&bad);
            assert_eq!(
                verify_es256(&p.join("."), &key, MIB).err(),
                Some(JoseError::BadSignature)
            );
        }
    }

    #[test]
    fn jws_signed_here_verifies_with_its_jwk_and_inflates() {
        let key = signing_key(5);
        let claims = br#"{"iss":"https://issuer.example","nbf":1700000000}"#;
        let compact = jws(
            &key,
            json!({"zip": "DEF", "alg": "ES256", "kid": "k1"}),
            &raw_deflate(claims),
        );
        let vk = es256_key(&jwk_of(&key)).expect("jwk");
        assert_eq!(verify_es256(&compact, &vk, MIB).expect("verify").payload, claims);
        let plain = jws(&key, json!({"alg": "ES256"}), claims);
        assert_eq!(verify_es256(&plain, &vk, MIB).expect("verify").payload, claims);
        assert_eq!(verify_es256(&plain, &vk, 10).err(), Some(JoseError::TooLarge(10)));
        assert_eq!(verify_es256(&compact, &vk, 10).err(), Some(JoseError::TooLarge(10)));
        let other = es256_key(&jwk_of(&signing_key(6))).expect("jwk");
        assert_eq!(verify_es256(&compact, &other, MIB).err(), Some(JoseError::BadSignature));
    }

    #[test]
    fn jws_refuses_alg_none_and_every_alg_but_es256() {
        let key = signing_key(5);
        let vk = es256_key(&jwk_of(&key)).expect("jwk");
        // alg none with an empty signature, the classic downgrade
        let input = format!("{}.{}", b64(br#"{"alg":"none"}"#), b64(b"{}"));
        assert_eq!(
            verify_es256(&format!("{input}."), &vk, MIB).err(),
            Some(JoseError::Unsupported("alg"))
        );
        for header in [
            json!({"alg": "none"}),
            json!({"alg": "HS256"}),
            json!({"alg": "ES384"}),
            json!({"alg": "es256"}),
            json!({}),
        ] {
            let compact = jws(&key, header.clone(), b"{}");
            assert_eq!(
                verify_es256(&compact, &vk, MIB).err(),
                Some(JoseError::Unsupported("alg")),
                "{header}"
            );
        }
        let compact = jws(&key, json!({"alg": "ES256", "zip": "GZIP"}), b"{}");
        assert_eq!(
            verify_es256(&compact, &vk, MIB).err(),
            Some(JoseError::Unsupported("zip"))
        );
        let compact = jws(&key, json!({"alg": "ES256", "crit": ["b64"], "b64": false}), b"{}");
        assert_eq!(
            verify_es256(&compact, &vk, MIB).err(),
            Some(JoseError::Unsupported("crit"))
        );
        // a JWE handed to the JWS verifier, and a JWS with five parts
        assert!(matches!(
            verify_es256(NODE_JWE_RAW, &vk, MIB),
            Err(JoseError::Malformed(_))
        ));
        assert!(matches!(jws_header(NODE_JWE_RAW), Err(JoseError::Malformed(_))));
    }

    #[test]
    fn jws_inflate_bomb_stops_at_the_cap() {
        let key = signing_key(8);
        let vk = es256_key(&jwk_of(&key)).expect("jwk");
        let compact = jws(
            &key,
            json!({"alg": "ES256", "zip": "DEF"}),
            &raw_deflate(&vec![b'a'; 64 * MIB]),
        );
        assert_eq!(verify_es256(&compact, &vk, MIB).err(), Some(JoseError::TooLarge(MIB)));
    }

    #[test]
    fn jws_high_s_twin_verifies_because_es256_does_not_require_low_s() {
        // RFC 7518 3.4 has no low-S rule and WebCrypto signers emit high S about half the time, so
        // refusing it would refuse real cards. The twin signs the same bytes: nothing is gained.
        const N: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xbc, 0xe6,
            0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63, 0x25, 0x51,
        ];
        let key = signing_key(5);
        let vk = es256_key(&jwk_of(&key)).expect("jwk");
        let compact = jws(&key, json!({"alg": "ES256"}), b"{}");
        let mut p = parts(&compact);
        let mut sig = URL_SAFE_NO_PAD.decode(&p[2]).expect("b64");
        // s' = n - s, big-endian
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let d = N[i] as i16 - sig[32 + i] as i16 - borrow;
            borrow = i16::from(d < 0);
            sig[32 + i] = (d + if d < 0 { 256 } else { 0 }) as u8;
        }
        p[2] = b64(&sig);
        let twin = p.join(".");
        assert_ne!(twin, compact);
        assert!(verify_es256(&twin, &vk, MIB).is_ok());
        // s = n is out of range
        sig[32..].copy_from_slice(&N);
        p[2] = b64(&sig);
        assert_eq!(
            verify_es256(&p.join("."), &vk, MIB).err(),
            Some(JoseError::BadSignature)
        );
    }

    #[test]
    fn jwk_refuses_another_curve_type_or_a_point_off_the_curve() {
        let good = shc_issuer_jwk();
        assert!(es256_key(&good).is_ok());
        let mut no_alg = good.clone();
        no_alg.as_object_mut().expect("object").shift_remove("alg");
        assert!(es256_key(&no_alg).is_ok(), "alg is optional in a JWK");
        let with = |field: &str, value: Value| {
            let mut jwk = good.clone();
            jwk[field] = value;
            es256_key(&jwk)
        };
        assert!(matches!(with("crv", json!("P-384")), Err(JoseError::Key(_))));
        assert!(matches!(with("crv", json!("secp256k1")), Err(JoseError::Key(_))));
        assert!(matches!(with("kty", json!("RSA")), Err(JoseError::Key(_))));
        assert!(matches!(with("kty", json!("OKP")), Err(JoseError::Key(_))));
        assert!(matches!(with("alg", json!("ES384")), Err(JoseError::Key(_))));
        assert!(
            matches!(with("x", json!(b64(&[1u8; 32]))), Err(JoseError::Key(_))),
            "off the curve"
        );
        assert!(
            matches!(with("y", json!(b64(&[1u8; 31]))), Err(JoseError::Key(_))),
            "short coordinate"
        );
        assert!(matches!(with("x", json!(48)), Err(JoseError::Key(_))));
        assert!(matches!(es256_key(&json!("not an object")), Err(JoseError::Key(_))));
    }
}
