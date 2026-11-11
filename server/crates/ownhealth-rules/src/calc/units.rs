//! The unit conversions of `data/calculators/units.yaml`: a printed lab value is divided by the
//! row's number before a formula reads it. A unit with no row is refused, never guessed.

use rust_decimal::Decimal;
use serde::Deserialize;

/// One row of units.yaml.
#[derive(Debug, Clone, Deserialize)]
pub struct UnitRow {
    pub id: String,
    pub group: String,
    pub unit: String,
    pub to: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub divide: Decimal,
    pub cite: String,
    /// A limitation code every result that used this row carries.
    #[serde(default)]
    pub limitation: Option<String>,
}

/// The row that reads `group` printed in `unit`.
pub fn find<'a>(rows: &'a [UnitRow], group: &str, unit: &str) -> Option<&'a UnitRow> {
    rows.iter().find(|r| r.group == group && r.unit == unit)
}

/// The value in the formula's unit.
pub fn convert(row: &UnitRow, value: Decimal) -> Option<Decimal> {
    value.checked_div(row.divide)
}
