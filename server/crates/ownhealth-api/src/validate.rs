//! FastAPI's request validation, for the routes of lane I: query, path and JSON body values that
//! fail answer 422 `{"detail": [{type, loc, msg, input, ctx?}, ...]}` with pydantic's wording, every
//! failing parameter in one answer.

use axum::body::Bytes;
use axum::http::{HeaderMap, header};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::routes::auth::{Fail, R, error_entry};

/// A query string as pairs; for a scalar parameter the last value wins, as Starlette reads it.
pub type Params = Vec<(String, String)>;

pub fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// pydantic prints a float bound without a trailing `.0` (`le=4.0` -> "4").
fn bound(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Collects every failing parameter, then answers 422 once.
#[derive(Debug, Default)]
pub struct Errors(Vec<Value>);

impl Errors {
    pub fn new() -> Errors {
        Errors::default()
    }

    pub fn push(&mut self, kind: &str, loc: Vec<Value>, msg: &str, input: Value, ctx: Option<Value>) {
        self.0.push(error_entry(kind, loc, msg, input, ctx));
    }

    pub fn missing(&mut self, loc: Vec<Value>, input: Value) {
        self.push("missing", loc, "Field required", input, None);
    }

    pub fn done(self) -> R<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Fail::Invalid(self.0))
        }
    }

    /// `int = Query(default, ge=.., le=..)`.
    pub fn int(
        &mut self,
        place: &str,
        q: &[(String, String)],
        key: &str,
        default: i64,
        ge: Option<i64>,
        le: Option<i64>,
    ) -> i64 {
        let Some(raw) = param(q, key) else { return default };
        let loc = vec![json!(place), json!(key)];
        let Some(v) = parse_int(raw) else {
            self.push(
                "int_parsing",
                loc,
                "Input should be a valid integer, unable to parse string as an integer",
                json!(raw),
                None,
            );
            return default;
        };
        if let Some(ge) = ge
            && v < ge
        {
            self.push(
                "greater_than_equal",
                loc,
                &format!("Input should be greater than or equal to {ge}"),
                json!(raw),
                Some(json!({"ge": ge})),
            );
            return default;
        }
        if let Some(le) = le
            && v > le
        {
            self.push(
                "less_than_equal",
                loc,
                &format!("Input should be less than or equal to {le}"),
                json!(raw),
                Some(json!({"le": le})),
            );
            return default;
        }
        v
    }

    /// `float = Query(default, ge=.., le=..)`.
    pub fn float(&mut self, q: &[(String, String)], key: &str, default: f64, ge: f64, le: f64) -> f64 {
        let Some(raw) = param(q, key) else { return default };
        let loc = vec![json!("query"), json!(key)];
        let Some(v) = raw.trim().parse::<f64>().ok().filter(|v| !v.is_nan()) else {
            self.push(
                "float_parsing",
                loc,
                "Input should be a valid number, unable to parse string as a number",
                json!(raw),
                None,
            );
            return default;
        };
        if v < ge {
            self.push(
                "greater_than_equal",
                loc,
                &format!("Input should be greater than or equal to {}", bound(ge)),
                json!(raw),
                Some(json!({"ge": ge})),
            );
            return default;
        }
        if v > le {
            self.push(
                "less_than_equal",
                loc,
                &format!("Input should be less than or equal to {}", bound(le)),
                json!(raw),
                Some(json!({"le": le})),
            );
            return default;
        }
        v
    }

    /// `x: UUID` (path) or `x: UUID | None` (query).
    pub fn uuid(&mut self, place: &str, key: &str, raw: &str) -> Option<Uuid> {
        match parse_uuid(raw) {
            Ok(u) => Some(u),
            Err(e) => {
                self.push(
                    "uuid_parsing",
                    vec![json!(place), json!(key)],
                    &format!("Input should be a valid UUID, {e}"),
                    json!(raw),
                    Some(json!({"error": e})),
                );
                None
            }
        }
    }

    /// `x: date | None = Query(None)`.
    pub fn date(&mut self, q: &[(String, String)], key: &str) -> Option<chrono::NaiveDate> {
        let raw = param(q, key)?;
        match chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
            Ok(d) if raw.len() == 10 => Some(d),
            _ => {
                self.push(
                    "date_from_datetime_parsing",
                    vec![json!("query"), json!(key)],
                    "Input should be a valid date or datetime, input is too short",
                    json!(raw),
                    Some(json!({"error": "input is too short"})),
                );
                None
            }
        }
    }

    /// `x: str | None = Query(None, max_length=..)`.
    pub fn str_max<'a>(&mut self, q: &'a [(String, String)], key: &str, max: usize) -> Option<&'a str> {
        let raw = param(q, key)?;
        if raw.chars().count() > max {
            self.push(
                "string_too_long",
                vec![json!("query"), json!(key)],
                &format!("String should have at most {max} characters"),
                json!(raw),
                Some(json!({"max_length": max})),
            );
            return None;
        }
        Some(raw)
    }
}

/// pydantic's lax int from a string: optional sign, digits, surrounding whitespace.
fn parse_int(raw: &str) -> Option<i64> {
    let t = raw.trim();
    let t = t.strip_prefix('+').unwrap_or(t);
    let digits = t.strip_prefix('-').unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        return None;
    }
    t.replace('_', "").parse().ok()
}

/// A UUID as pydantic reads it: hyphenated, simple, braced or `urn:uuid:`.
pub fn parse_uuid(raw: &str) -> Result<Uuid, String> {
    Uuid::parse_str(raw).map_err(|e| e.to_string())
}

/// `x: bool = Query(default)` with pydantic's accepted spellings.
pub fn bool_param(q: &[(String, String)], key: &str, default: bool) -> R<bool> {
    let Some(raw) = param(q, key) else { return Ok(default) };
    match raw.to_ascii_lowercase().as_str() {
        "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
        "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
        _ => Err(Fail::Invalid(vec![error_entry(
            "bool_parsing",
            vec![json!("query"), json!(key)],
            "Input should be a valid boolean, unable to interpret input",
            json!(raw),
            None,
        )])),
    }
}

/// A path parameter declared `UUID`.
pub fn uuid_path(key: &str, raw: &str) -> R<Uuid> {
    let mut e = Errors::new();
    let id = e.uuid("path", key, raw);
    e.done()?;
    id.ok_or_else(|| Fail::Invalid(Vec::new()))
}

fn is_json(headers: &HeaderMap) -> bool {
    match headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(ct) => {
            let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
        }
    }
}

/// A JSON object body as FastAPI reads a pydantic model parameter. `None` when the body is empty
/// and `required` is false (`body: Model | None = None`).
pub fn json_object(headers: &HeaderMap, bytes: &Bytes, required: bool) -> R<Option<serde_json::Map<String, Value>>> {
    if bytes.is_empty() {
        if required {
            let mut e = Errors::new();
            e.missing(vec![json!("body")], Value::Null);
            e.done()?;
        }
        return Ok(None);
    }
    let value: Value = if is_json(headers) {
        serde_json::from_slice(bytes).map_err(|err| {
            Fail::Invalid(vec![error_entry(
                "json_invalid",
                vec![json!("body"), json!(err.column().saturating_sub(1))],
                "JSON decode error",
                json!({}),
                Some(json!({"error": err.to_string()})),
            )])
        })?
    } else {
        Value::String(String::from_utf8_lossy(bytes).into_owned())
    };
    match value {
        Value::Object(m) => Ok(Some(m)),
        Value::Null if !required => Ok(None),
        other => Err(Fail::Invalid(vec![error_entry(
            "model_attributes_type",
            vec![json!("body")],
            "Input should be a valid dictionary or object to extract fields from",
            other,
            None,
        )])),
    }
}

/// A required `str` field of a body model.
pub fn body_str(e: &mut Errors, body: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    let input = Value::Object(body.clone());
    match body.get(key) {
        None => {
            e.missing(vec![json!("body"), json!(key)], input);
            None
        }
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            e.push(
                "string_type",
                vec![json!("body"), json!(key)],
                "Input should be a valid string",
                other.clone(),
                None,
            );
            None
        }
    }
}

/// A route declared `-> dict` answered through pydantic's JSON mode, which writes a UTC datetime
/// with `Z` (`2026-09-14T08:00:00Z`); an unannotated route went through `jsonable_encoder`, which
/// writes `+00:00`. This rewrites the named datetime fields of `v` (row values already printed as
/// `isoformat`) into the pydantic form.
pub fn pydantic_datetimes(v: &mut Value, keys: &[&str]) {
    let Some(m) = v.as_object_mut() else { return };
    for k in keys {
        if let Some(Value::String(s)) = m.get_mut(*k)
            && let Ok(dt) = ownhealth_core::pyjson::parse_datetime(s)
        {
            *s = ownhealth_core::pyjson::pydantic_datetime(&dt);
        }
    }
}
