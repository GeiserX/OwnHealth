//! The language registry: the languages of the UI and the printed sheets. MCP stays es/en
//! (`api_token.lang`, [`MCP_LANGS`]), whatever this list holds.
//!
//! Adding a language is one data PR: its code here and in `ui/src/lib/i18n/langs.ts` (a test keeps
//! the two lists equal), its UI file, its strings in every localized mapping under `data/` (the
//! completeness test names each one missing), and its imperative patterns in
//! `ownhealth_rules::wording`. `app_user.locale` and `shl.lang` already accept every code in
//! [`LOCALES`], so it needs no migration. `api_token.lang` stays es/en by design: a new language
//! does not change it, and a person in that language gets MCP in es ([`mcp_lang`]).
//!
//! Every renderer picks its text for a language one way: [`lookup`] (or [`pick`] over a table of
//! `(code, text)` rows), never an `if lang == "es"` branch. The texts a renderer keeps in code or in
//! a catalog with fixed es/en fields name the languages they lack with [`missing`], and
//! `server/crates/ownhealth/tests/langs.rs` runs every one of them with the registry and with a
//! dummy language, so a registry language without its text fails there instead of reading another
//! language at run time. The same test refuses an `== "es"` pick in the server's sources.

/// The languages a person can pick, in picker order. The first is the default for a new login.
pub const LANGS: [&str; 2] = ["es", "en"];

/// Every code `app_user.locale` and `shl.lang` accept (migration 1280): the registry and the
/// languages planned for 1.0, so adding one needs no migration. `check_mirror.rs` keeps this equal to
/// both CHECKs.
pub const LOCALES: [&str; 7] = ["es", "en", "de", "fr", "ca", "it", "pt"];

/// The languages MCP speaks (`api_token.lang`; `check_mirror.rs` keeps this equal to its CHECK). They
/// stay es/en whatever [`LANGS`] holds.
pub const MCP_LANGS: [&str; 2] = ["es", "en"];

/// The language of a browser whose languages the registry lacks, as the UI's `pickLang` picks it.
pub const BROWSER_DEFAULT: &str = "en";

/// `lang` when it is in the registry, else the default (the first entry). This is the one rule for
/// a code the registry does not know, in every renderer.
pub fn lang_or_default(lang: &str) -> &'static str {
    LANGS.iter().copied().find(|l| *l == lang).unwrap_or(LANGS[0])
}

/// The registry language of a BCP 47 tag (`es-ES`, `en`), by its primary subtag; None when the
/// registry has no such language.
pub fn of_tag(tag: &str) -> Option<&'static str> {
    let base = tag.trim().split(['-', '_']).next()?.to_ascii_lowercase();
    LANGS.iter().copied().find(|l| *l == base)
}

/// The text of `lang` through `get`, which reads one language's text from wherever it lives (a
/// localized mapping, a struct field, a table row). The order, the same in every renderer:
/// [`lang_or_default`] of `lang`, then the default language. The tests keep every source complete
/// for every registry language, so the second step never runs for one.
pub fn lookup<T>(lang: &str, mut get: impl FnMut(&'static str) -> Option<T>) -> Option<T> {
    let lang = lang_or_default(lang);
    get(lang).or_else(|| if lang == LANGS[0] { None } else { get(LANGS[0]) })
}

/// [`lookup`] over a table of `(code, text)` rows, the way a renderer keeps texts in code; the first
/// row when neither language has one. `texts` is never empty.
pub fn pick<T: Copy>(texts: &[(&str, T)], lang: &str) -> T {
    lookup(lang, |l| texts.iter().find(|(c, _)| *c == l).map(|(_, t)| *t)).unwrap_or(texts[0].1)
}

/// `{<lang>: get(lang)}` for every registry language: a text the screen switches between without a
/// new request (the UI's `Localized`).
pub fn per_lang(mut get: impl FnMut(&'static str) -> serde_json::Value) -> serde_json::Value {
    LANGS
        .iter()
        .map(|l| (l.to_string(), get(l)))
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

/// The languages of `langs` that `have` (the codes a text source carries) lacks, in `langs` order.
/// Each source runs this with the registry, and its control with a dummy language.
pub fn missing<'l>(have: &[&str], langs: &[&'l str]) -> Vec<&'l str> {
    langs.iter().copied().filter(|l| !have.contains(l)).collect()
}

/// The codes of a `(code, text)` table, for [`missing`].
pub fn codes<T>(texts: &[(&'static str, T)]) -> Vec<&'static str> {
    texts.iter().map(|(c, _)| *c).collect()
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
    fn a_text_is_looked_up_in_the_language_asked_then_in_the_default() {
        let table = [("en", "Hello"), ("es", "Hola"), ("xx", "Xx")];
        assert_eq!(pick(&table, "en"), "Hello");
        assert_eq!(pick(&table, "es"), "Hola");
        // a code the registry does not know reads the default, even when the table has it
        assert_eq!(pick(&table, "xx"), "Hola");
        assert_eq!(pick(&table, "EN"), "Hola");
        // a registry language a table lacks reads the default; the first row only when that is gone too
        assert_eq!(pick(&[("xx", "Xx"), ("es", "Hola")], "en"), "Hola");
        assert_eq!(pick(&[("en", "Hello")], "es"), "Hello");
        let mut asked = Vec::new();
        assert_eq!(
            lookup("en", |l| {
                asked.push(l);
                None::<&str>
            }),
            None
        );
        assert_eq!(asked, ["en", "es"]);
        asked.clear();
        let _ = lookup("es", |l| {
            asked.push(l);
            None::<&str>
        });
        assert_eq!(asked, ["es"], "the default is asked once");
    }

    #[test]
    fn a_tag_maps_onto_a_registry_language_by_its_first_subtag() {
        assert_eq!(of_tag("es-ES"), Some("es"));
        assert_eq!(of_tag(" EN_gb"), Some("en"));
        assert_eq!(of_tag("en"), Some("en"));
        for other in ["de", "est", "", "e"] {
            assert_eq!(of_tag(other), None, "{other}");
        }
    }

    #[test]
    fn per_lang_has_every_registry_language() {
        let v = per_lang(|l| serde_json::json!(format!("text {l}")));
        let keys: Vec<&str> = v
            .as_object()
            .into_iter()
            .flat_map(|m| m.keys())
            .map(String::as_str)
            .collect();
        assert_eq!(keys, LANGS);
        assert_eq!(v["es"], "text es");
    }

    #[test]
    fn missing_names_each_language_a_source_lacks() {
        assert_eq!(missing(&["es", "en"], &LANGS), Vec::<&str>::new());
        assert_eq!(missing(&["es", "en"], &["es", "en", "xx"]), ["xx"]);
        assert_eq!(missing(&["en"], &LANGS), ["es"]);
        assert_eq!(codes(&[("es", 1), ("en", 2)]), ["es", "en"]);
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
        assert!(LANGS.contains(&BROWSER_DEFAULT));
    }
}
