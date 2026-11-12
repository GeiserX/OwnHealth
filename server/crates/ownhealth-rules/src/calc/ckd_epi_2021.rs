//! CKD-EPI 2021, the race-free equations of Inker et al., N Engl J Med 2021;385:1737-1749
//! (https://pubmed.ncbi.nlm.nih.gov/34554658/). Creatinine in mg/dL, cystatin C in mg/L, age in
//! whole years; the result is in mL/min/1.73 m2.
//!
//! ```text
//! eGFRcr     = 142 x min(Scr/k, 1)^a x max(Scr/k, 1)^-1.200 x 0.9938^age [x 1.012 female]
//! eGFRcr-cys = 135 x min(Scr/k, 1)^a x max(Scr/k, 1)^-0.544 x min(Scys/0.8, 1)^-0.323
//!                  x max(Scys/0.8, 1)^-0.778 x 0.9961^age [x 0.963 female]
//! k = 0.7 female, 0.9 male; a = -0.241 / -0.302 (cr), -0.219 / -0.144 (cr-cys), female / male
//! ```

use rust_decimal::Decimal;

use super::{Sex, pow, powi};

fn kappa(sex: Sex) -> Decimal {
    match sex {
        Sex::Female => Decimal::new(7, 1),
        Sex::Male => Decimal::new(9, 1),
    }
}

/// eGFR from creatinine alone, unrounded.
pub fn cr(scr_mg_dl: Decimal, age: u32, sex: Sex) -> Option<Decimal> {
    let alpha = match sex {
        Sex::Female => Decimal::new(-241, 3),
        Sex::Male => Decimal::new(-302, 3),
    };
    let r = scr_mg_dl.checked_div(kappa(sex))?;
    let mut v = Decimal::new(142, 0)
        * pow(r.min(Decimal::ONE), alpha)?
        * pow(r.max(Decimal::ONE), Decimal::new(-1200, 3))?
        * powi(Decimal::new(9938, 4), age)?;
    if sex == Sex::Female {
        v *= Decimal::new(1012, 3);
    }
    Some(v)
}

/// eGFR from creatinine and cystatin C, unrounded.
pub fn cr_cys(scr_mg_dl: Decimal, scys_mg_l: Decimal, age: u32, sex: Sex) -> Option<Decimal> {
    let alpha = match sex {
        Sex::Female => Decimal::new(-219, 3),
        Sex::Male => Decimal::new(-144, 3),
    };
    let r = scr_mg_dl.checked_div(kappa(sex))?;
    let c = scys_mg_l.checked_div(Decimal::new(8, 1))?;
    let mut v = Decimal::new(135, 0)
        * pow(r.min(Decimal::ONE), alpha)?
        * pow(r.max(Decimal::ONE), Decimal::new(-544, 3))?
        * pow(c.min(Decimal::ONE), Decimal::new(-323, 3))?
        * pow(c.max(Decimal::ONE), Decimal::new(-778, 3))?
        * powi(Decimal::new(9961, 4), age)?;
    if sex == Sex::Female {
        v *= Decimal::new(963, 3);
    }
    Some(v)
}
