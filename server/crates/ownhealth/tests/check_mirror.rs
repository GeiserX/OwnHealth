//! Every CHECK that lists allowed values has one Rust constant, and the two say the same thing
//! (CONTRACTS.md section 7). A later migration that re-creates one of these CHECKs with only its own
//! lane's values, or a constant that gains a value no migration allows, fails here.

use std::collections::BTreeSet;

use ownhealth_auth::challenges;
use ownhealth_core::testing::TestDb;
use ownhealth_core::{documents, langs, provenance, review, secrets, sources};
use ownhealth_rules::summary;

/// (table, constraint, the Rust constant that writers check against).
fn mirrors() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    vec![
        ("source", "source_kind_check", sources::SOURCE_KINDS.to_vec()),
        ("secret", "secret_purpose_check", secrets::PURPOSES.to_vec()),
        (
            "auth_challenge",
            "auth_challenge_purpose_check",
            challenges::PURPOSES.to_vec(),
        ),
        (
            "document_source",
            "document_source_channel_check",
            documents::CHANNELS.to_vec(),
        ),
        ("src", "src_kind_check", provenance::SRC_KINDS.to_vec()),
        ("review_item", "review_item_kind_check", review::REVIEW_KINDS.to_vec()),
        ("app_user", "app_user_locale_check", langs::LOCALES.to_vec()),
        ("api_token", "api_token_lang_check", langs::MCP_LANGS.to_vec()),
        ("shl", "shl_lang_check", summary::LANGS.to_vec()),
    ]
}

/// The quoted values of `CHECK ((kind = ANY (ARRAY['a'::text, 'b'::text])))`.
fn quoted(def: &str) -> BTreeSet<String> {
    def.split('\'').skip(1).step_by(2).map(str::to_string).collect()
}

#[tokio::test]
async fn every_value_check_matches_its_rust_constant() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let mut drift = Vec::new();
    for (table, name, constant) in mirrors() {
        let def: Option<String> = sqlx::query_scalar(
            "select pg_get_constraintdef(oid) from pg_constraint where conrelid = $1::regclass and conname = $2",
        )
        .bind(table)
        .bind(name)
        .fetch_optional(&db.pool)
        .await?;
        let Some(def) = def else {
            drift.push(format!("{table}: no constraint {name}"));
            continue;
        };
        let rust: BTreeSet<String> = constant.iter().map(|s| s.to_string()).collect();
        assert_eq!(rust.len(), constant.len(), "{name}: the constant repeats a value");
        let sql = quoted(&def);
        assert!(!sql.is_empty(), "{name}: no values read from {def}");
        let only_sql: Vec<&String> = sql.difference(&rust).collect();
        let only_rust: Vec<&String> = rust.difference(&sql).collect();
        if !only_sql.is_empty() || !only_rust.is_empty() {
            drift.push(format!("{name}: only in SQL {only_sql:?}, only in Rust {only_rust:?}"));
        }
    }
    assert!(drift.is_empty(), "CHECK and constant differ:\n{}", drift.join("\n"));
    Ok(())
}

#[test]
fn the_reader_takes_every_quoted_value() {
    let def = "CHECK ((kind = ANY (ARRAY['email'::text, 'health_auto_export'::text])))";
    let want: BTreeSet<String> = ["email", "health_auto_export"].iter().map(|s| s.to_string()).collect();
    assert_eq!(quoted(def), want);
}

/// The values migration 1200 adds for the v1.0 connectors. The mirror alone would stay green if one of
/// them left both the CHECK and its constant; this list keeps each one.
#[test]
fn every_v1_value_is_in_its_constant() {
    let added: [(&str, &[&str], Vec<&str>); 6] = [
        (
            "source.kind",
            &["smart", "bluebutton", "brands", "openwearables", "dexcom"],
            sources::SOURCE_KINDS.to_vec(),
        ),
        (
            "secret.purpose",
            &[
                "connector_refresh_token",
                "connector_client_secret",
                "openwearables_api_key",
            ],
            secrets::PURPOSES.to_vec(),
        ),
        ("auth_challenge.purpose", &["connector"], challenges::PURPOSES.to_vec()),
        (
            "document_source.channel",
            &[
                "smart",
                "bluebutton",
                "shl",
                "shc",
                "ccda",
                "openwearables",
                "dexcom",
                "imaging",
            ],
            documents::CHANNELS.to_vec(),
        ),
        ("src.kind", &["dicom"], provenance::SRC_KINDS.to_vec()),
        (
            "review_item.kind",
            &["source_gap", "fhir_unmapped_summary", "family_proposal"],
            review::REVIEW_KINDS.to_vec(),
        ),
    ];
    let missing: Vec<String> = added
        .iter()
        .flat_map(|(list, values, constant)| {
            values
                .iter()
                .filter(|v| !constant.contains(v))
                .map(move |v| format!("{list}: {v}"))
        })
        .collect();
    assert!(missing.is_empty(), "v1.0 values missing: {missing:?}");
}
