//! HOMA-IR of Matthews et al., Diabetologia 1985;28:412-419 (https://pubmed.ncbi.nlm.nih.gov/3899825/):
//!
//! ```text
//! HOMA-IR = fasting glucose [mmol/L] x fasting insulin [uU/mL] / 22.5
//! ```
//!
//! Both values come from one draw. A result whose glucose row is not labelled fasting (method
//! `fasting`, as "Glucosa en ayunas" or LOINC 1558-6 resolve) carries the limitation
//! `fasting_not_recorded`.

use rust_decimal::Decimal;

/// HOMA-IR, unrounded. None when an input is zero or negative.
pub fn homa_ir(glucose_mmol_l: Decimal, insulin_uu_ml: Decimal) -> Option<Decimal> {
    if glucose_mmol_l <= Decimal::ZERO || insulin_uu_ml <= Decimal::ZERO {
        return None;
    }
    glucose_mmol_l
        .checked_mul(insulin_uu_ml)?
        .checked_div(Decimal::new(225, 1))
}
