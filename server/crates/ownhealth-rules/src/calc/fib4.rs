//! FIB-4 of Sterling et al., Hepatology 2006;43:1317-1325 (https://pubmed.ncbi.nlm.nih.gov/16729309/):
//!
//! ```text
//! FIB-4 = age [years] x AST [U/L] / (platelets [10^9/L] x sqrt(ALT [U/L]))
//! ```
//!
//! The published bands: 1.3 and 2.67 (Shah 2009, https://pubmed.ncbi.nlm.nih.gov/19523535/), with 2.0
//! as the lower cutoff from age 65 (McPherson 2017, https://pubmed.ncbi.nlm.nih.gov/27725647/). The
//! index is validated from age 35.

use rust_decimal::{Decimal, MathematicalOps};

/// FIB-4, unrounded. None when an input is zero or negative.
pub fn fib4(age: u32, ast_u_l: Decimal, alt_u_l: Decimal, platelets_10_9_l: Decimal) -> Option<Decimal> {
    if [ast_u_l, alt_u_l, platelets_10_9_l].iter().any(|x| *x <= Decimal::ZERO) {
        return None;
    }
    let root = alt_u_l.sqrt()?;
    let divisor = platelets_10_9_l.checked_mul(root)?;
    Decimal::from(age).checked_mul(ast_u_l)?.checked_div(divisor)
}
