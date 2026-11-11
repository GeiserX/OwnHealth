//! HOMA-IR of Matthews et al., Diabetologia 1985;28:412-419 (https://pubmed.ncbi.nlm.nih.gov/3899825/):
//!
//! ```text
//! HOMA-IR = fasting glucose [mmol/L] x fasting insulin [uU/mL] / 22.5
//! ```
//!
//! Both values come from one draw. The record does not say whether the draw was fasting, so every
//! result carries the limitation `fasting_not_recorded`.

use rust_decimal::Decimal;

/// HOMA-IR, unrounded.
pub fn homa_ir(glucose_mmol_l: Decimal, insulin_uu_ml: Decimal) -> Option<Decimal> {
    glucose_mmol_l
        .checked_mul(insulin_uu_ml)?
        .checked_div(Decimal::new(225, 1))
}
