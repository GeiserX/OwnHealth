//! The language registry: the languages of the UI and the printed sheets. MCP stays es/en
//! (`api_token.lang`), whatever this list holds.
//!
//! Adding a language is one data PR: its code here and in `ui/src/lib/i18n/langs.ts` (a test keeps
//! the two lists equal), its UI file, its strings in every localized mapping under `data/` (the
//! completeness test names each one missing), and its imperative patterns in
//! `ownhealth_rules::wording`. `app_user.locale` already accepts every code in [`LOCALES`].

/// The languages a person can pick, in picker order. The first is the default for a new login.
pub const LANGS: [&str; 2] = ["es", "en"];

/// Every code `app_user.locale` accepts (migration 1280): the registry and the languages planned
/// for 1.0, so adding one needs no migration. `check_mirror.rs` keeps this equal to the CHECK.
pub const LOCALES: [&str; 7] = ["es", "en", "de", "fr", "ca", "it", "pt"];

/// The languages MCP speaks (`api_token.lang`; `check_mirror.rs` keeps this equal to its CHECK). They
/// stay es/en whatever [`LANGS`] holds.
pub const MCP_LANGS: [&str; 2] = ["es", "en"];

/// `lang` when it is in the registry, else the default (the first entry).
pub fn lang_or_default(lang: &str) -> &str {
    if LANGS.contains(&lang) { lang } else { LANGS[0] }
}

/// Any locale mapped onto an MCP language: itself when MCP speaks it, else the first entry (es).
pub fn mcp_lang(lang: &str) -> &'static str {
    MCP_LANGS.iter().copied().find(|l| *l == lang).unwrap_or(MCP_LANGS[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_language_can_be_stored() {
        for lang in LANGS {
            assert!(
                LOCALES.contains(&lang),
                "{lang} is in LANGS but app_user.locale refuses it"
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        assert!(LOCALES.iter().all(|l| seen.insert(*l)), "LOCALES repeats a code");
        assert!(
            LANGS
                .iter()
                .all(|l| l.len() == 2 && l.chars().all(|c| c.is_ascii_lowercase()))
        );
    }

    #[test]
    fn an_unregistered_language_falls_back_to_the_default() {
        for lang in LANGS {
            assert_eq!(lang_or_default(lang), lang);
        }
        for other in ["xx", "", "ES", "es-ES"] {
            assert_eq!(lang_or_default(other), "es", "{other}");
        }
    }

    #[test]
    fn every_locale_maps_onto_an_mcp_language() {
        for lang in MCP_LANGS {
            assert_eq!(mcp_lang(lang), lang);
        }
        for other in LOCALES.iter().chain(&["xx", "", "EN"]) {
            assert!(MCP_LANGS.contains(&mcp_lang(other)), "{other}");
        }
        assert_eq!(mcp_lang("de"), "es");
    }
}
