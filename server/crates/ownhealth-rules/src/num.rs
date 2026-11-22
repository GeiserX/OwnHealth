//! Python's number behaviour where the state depends on it: `format(Decimal, "f")`, `quantize` with
//! ROUND_HALF_EVEN, `Decimal(str(x))`, `sum()` of floats (Neumaier since Python 3.12) and float4
//! columns read the way psycopg reads them (the shortest printed form, parsed as a double).

use std::str::FromStr;

use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;

/// `format(d, "f")`: no exponent, the precision the value carries.
pub fn num(d: &Decimal) -> String {
    d.to_string()
}

/// How one language writes a number: its decimal mark, the mark between thousands of a measured
/// value with the integer length it starts at (None: never), and the same for a whole count (the
/// dashboard's 30-day sums).
#[derive(Debug, Clone, Copy)]
pub struct Format {
    decimal: char,
    group: Option<(char, usize)>,
    count_group: (char, usize),
}

/// The number format of every registry language (`ownhealth_core::langs::LANGS`), read with
/// `langs::pick`; `tests/langs.rs` fails on a registry language without its row. Spanish follows the
/// UI's `fmtNum` (es-ES): a decimal comma and a '.' between thousands from five integer digits on.
pub const FORMATS: &[(&str, Format)] = &[
    (
        "es",
        Format {
            decimal: ',',
            group: Some(('.', 5)),
            count_group: ('.', 5),
        },
    ),
    (
        "en",
        Format {
            decimal: '.',
            group: None,
            count_group: (' ', 4),
        },
    ),
];

/// The registry languages [`FORMATS`] has no row for.
pub fn missing_langs(langs: &[&str]) -> Vec<String> {
    ownhealth_core::langs::missing(&ownhealth_core::langs::codes(FORMATS), langs)
        .into_iter()
        .map(|l| format!("number format has no `{l}` row"))
        .collect()
}

fn grouped(int: &str, mark: char, from: usize) -> String {
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if int.len() >= from && i > 0 && (int.len() - i).is_multiple_of(3) {
            out.push(mark);
        }
        out.push(c);
    }
    out
}

/// A whole count (`12345`) as a person reading `lang` writes it.
pub fn count(n: i64, lang: &str) -> String {
    let (mark, from) = ownhealth_core::langs::pick(FORMATS, lang).count_group;
    let sign = if n < 0 { "-" } else { "" };
    format!("{sign}{}", grouped(&n.unsigned_abs().to_string(), mark, from))
}

/// A plain printed number (`-1234.5`) as a person reading `lang` writes it, by its [`FORMATS`] row;
/// any other text comes back as it is.
pub fn localized(s: &str, lang: &str) -> String {
    let f = ownhealth_core::langs::pick(FORMATS, lang);
    let (sign, body) = match s.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", s),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let digits = |x: &str| x.chars().all(|c| c.is_ascii_digit());
    if int.is_empty() || !digits(int) || !digits(frac) || (body.contains('.') && frac.is_empty()) {
        return s.to_string();
    }
    let mut out = String::from(sign);
    match f.group {
        Some((mark, from)) => out.push_str(&grouped(int, mark, from)),
        None => out.push_str(int),
    }
    if !frac.is_empty() {
        out.push(f.decimal);
        out.push_str(frac);
    }
    out
}

/// `d.quantize(Decimal(1).scaleb(-places), rounding=ROUND_HALF_EVEN)`: exactly `places` decimals.
pub fn quantize(d: Decimal, places: u32) -> Decimal {
    let mut r = d.round_dp_with_strategy(places, RoundingStrategy::MidpointNearestEven);
    r.rescale(places);
    r
}

/// `Decimal(s)` for a printed number (plain or with an exponent).
pub fn parse(s: &str) -> Option<Decimal> {
    let s = s.trim();
    Decimal::from_str(s).ok().or_else(|| Decimal::from_scientific(s).ok())
}

/// `dec(value)` of `state/pack.py`: `Decimal(str(value))`, None for None.
pub fn dec(v: Option<&Value>) -> Option<Decimal> {
    match v? {
        Value::Null => None,
        Value::String(s) => parse(s),
        Value::Number(n) => parse(&n.to_string()),
        Value::Bool(b) => Some(Decimal::from(i64::from(*b))),
        _ => None,
    }
}

/// `Decimal(str(f))`: the shortest repr of the float.
pub fn from_f64(f: f64) -> Option<Decimal> {
    if !f.is_finite() {
        return None;
    }
    parse(&format!("{f}"))
}

/// A float4 column as psycopg returns it: the text Postgres prints, parsed as a double.
pub fn f32_as_py(v: f32) -> f64 {
    format!("{v}").parse().unwrap_or(f64::from(v))
}

/// `sum(values)` over floats, as CPython 3.12+ adds them (Neumaier's compensated sum).
pub fn py_sum(values: &[f64]) -> f64 {
    let mut total = 0.0f64;
    let mut c = 0.0f64;
    for &x in values {
        let t = total + x;
        if total.abs() >= x.abs() {
            c += (total - t) + x;
        } else {
            c += (x - t) + total;
        }
        total = t;
    }
    if c != 0.0 && c.is_finite() { total + c } else { total }
}

/// `float(Decimal)`.
pub fn to_f64(d: &Decimal) -> f64 {
    ownhealth_core::pyjson::decimal_to_f64(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spanish_numbers_take_a_decimal_comma() {
        assert_eq!(localized("2.4", "es"), "2,4");
        assert_eq!(localized("0.98", "es"), "0,98");
        assert_eq!(localized("9240", "es"), "9240");
        assert_eq!(localized("12345.5", "es"), "12.345,5");
        assert_eq!(localized("-1.1", "es"), "-1,1");
        assert_eq!(localized("2.4", "en"), "2.4");
        assert_eq!(localized("< 75", "es"), "< 75");
        assert_eq!(localized("abc", "es"), "abc");
        // a whole count, as the dashboard's 30-day sums print it
        assert_eq!(count(1234, "es"), "1234");
        assert_eq!(count(12345, "es"), "12.345");
        assert_eq!(count(1234, "en"), "1 234");
        assert_eq!(count(-1_234_567, "en"), "-1 234 567");
        assert_eq!(count(999, "en"), "999");
    }

    #[test]
    fn quantize_keeps_the_scale_and_rounds_half_even() {
        assert_eq!(num(&quantize(Decimal::from(118), 1)), "118.0");
        assert_eq!(num(&quantize(parse("0.125").unwrap_or_default(), 2)), "0.12");
        assert_eq!(num(&quantize(parse("0.135").unwrap_or_default(), 2)), "0.14");
        assert_eq!(num(&parse("0.90").unwrap_or_default()), "0.90");
    }

    #[test]
    fn neumaier_matches_python() {
        // Python 3.13: sum([0.1] * 10) == 1.0; the naive left fold gives 0.9999999999999999.
        assert_eq!(py_sum(&[0.1; 10]), 1.0);
        assert_eq!(py_sum(&[1e100, 1.0, -1e100]), 1.0);
    }

    #[test]
    fn float4_reads_as_its_printed_form() {
        assert_eq!(f32_as_py(54.3f32), 54.3f64);
        assert_eq!(from_f64(54.3).map(|d| num(&d)), Some("54.3".into()));
    }
}
