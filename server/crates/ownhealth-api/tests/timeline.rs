//! Lane I: `api/timeline.py` (tests/timeline/test_timeline.py): draws grouped, records dated,
//! lifecycle and scopes respected, audited.
//!
//! Python narrowed the scopes with a token principal through a dependency override; REST refuses
//! tokens here, so the same narrowing comes from the grant's categories (effective scopes = token
//! scopes ∩ grant categories − locks).

mod lane_i;

use http::StatusCode;
use lane_i::{App, R, app, manual_src, obs};
use ownhealth_core::observations::{CommitOptions, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef, create_src};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record, retract_record};
use serde_json::{Value, json};

async fn record(c: &mut sqlx::PgConnection, kind: &str, v: Value) -> anyhow::Result<uuid::Uuid> {
    let r = RecordIn::from_json(kind, v)?;
    Ok(
        add_record(c, &r, &SrcRef::New(manual_src()), RecordOptions::new("person"))
            .await?
            .id,
    )
}

async fn seed(a: &App) -> R {
    let alex = a.family.profile("alex");
    let mut tx = a.rt.pool.begin().await?;
    let person = || CommitOptions::new("person");
    // One draw: two values sharing a report_key, one flagged by the lab.
    for (value, group, flag) in [("97", "ldl_c", Value::Null), ("24", "vitamin_d", json!("L"))] {
        let o = obs(
            alex,
            value,
            json!({"alias_group": group, "interpretation": flag, "report_key": "LN-1"}),
        );
        commit_observation(&mut tx, &o, &SrcRef::New(manual_src()), person()).await?;
    }
    let o = obs(alex, "118", json!({"day": "2026-03-02"}));
    commit_observation(&mut tx, &o, &SrcRef::New(manual_src()), person()).await?;
    record(
        &mut tx,
        "medication",
        json!({"profile_id": alex, "name": "Atorvastatin", "strength": "20 mg", "started": "2022-06-20",
               "stopped": "2025-12-01", "reason_text": "paused"}),
    )
    .await?;
    let gone = record(
        &mut tx,
        "medication",
        json!({"profile_id": alex, "name": "Wrongpril", "started": "2024-01-01"}),
    )
    .await?;
    retract_record(&mut tx, "medication", gone, "entered in error").await?;
    record(
        &mut tx,
        "condition",
        json!({"profile_id": alex, "text": "Hypercholesterolaemia", "code_system": "icd10", "code": "E78.0",
               "onset": "2021-11-09"}),
    )
    .await?;
    record(
        &mut tx,
        "condition",
        json!({"profile_id": alex, "text": "Hidden thing", "onset": "2023-01-01", "sensitivity": ["mental_health"]}),
    )
    .await?;
    record(
        &mut tx,
        "immunization",
        json!({"profile_id": alex, "vaccine": "Td booster", "occurred": "2016-05-10"}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

fn events(v: &Value) -> Vec<Value> {
    v["events"].as_array().cloned().unwrap_or_default()
}

fn strs(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn test_timeline_groups_draws_and_dates_records() -> R {
    let a = app().await?;
    seed(&a).await?;
    let (s, out) = a.get("alex", "/profiles/alex/timeline?lang=en").await?;
    assert_eq!(s, StatusCode::OK, "{out}");
    let ev = events(&out);
    let dates: Vec<String> = ev.iter().map(|e| strs(e, "date")).collect();
    let mut sorted = dates.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(dates, sorted);
    let labs: Vec<&Value> = ev.iter().filter(|e| e["type"] == "lab").collect();
    let got: Vec<(String, String)> = labs.iter().map(|e| (strs(e, "date"), strs(e, "title"))).collect();
    assert_eq!(
        got,
        [
            ("2026-09-14".to_string(), "Lab report · 2 results".to_string()),
            ("2026-03-02".to_string(), "Lab report · 1 result".to_string())
        ]
    );
    assert_eq!(labs[0]["detail"], "1 flagged by the lab");
    assert_eq!(labs[1]["detail"], Value::Null);
    let mut meds: Vec<(String, String)> = ev
        .iter()
        .filter(|e| e["type"] == "medication")
        .map(|e| (strs(e, "date"), strs(e, "title")))
        .collect();
    meds.sort();
    assert_eq!(
        meds,
        [
            ("2022-06-20".to_string(), "Atorvastatin 20 mg started".to_string()),
            ("2025-12-01".to_string(), "Atorvastatin 20 mg stopped".to_string())
        ]
    );
    let mut dx: Vec<String> = ev
        .iter()
        .filter(|e| e["type"] == "diagnosis")
        .map(|e| strs(e, "title"))
        .collect();
    dx.sort();
    assert_eq!(dx, ["Hidden thing", "Hypercholesterolaemia"]);
    let hyper = ev
        .iter()
        .find(|e| e["title"] == "Hypercholesterolaemia")
        .cloned()
        .unwrap_or_default();
    assert_eq!(hyper["detail"], "ICD10 E78.0 · provisional");
    let vaccines: Vec<String> = ev
        .iter()
        .filter(|e| e["type"] == "vaccine")
        .map(|e| strs(e, "date"))
        .collect();
    assert_eq!(vaccines, ["2016-05-10"]);
    assert!(ev.iter().all(|e| strs(e, "src").starts_with("src_")));
    let keys: Vec<&String> = ev[0].as_object().map(|m| m.keys().collect()).unwrap_or_default();
    assert_eq!(
        keys,
        [
            "date",
            "type",
            "title",
            "detail",
            "country",
            "flags",
            "src",
            "src_label",
            "provider"
        ]
    );
    assert_eq!(out["withheld"], json!({"by_scope": []}));
    let (_, es) = a.get("alex", "/profiles/alex/timeline?lang=es&types=lab").await?;
    assert_eq!(events(&es)[0]["title"], "Analítica · 2 resultados");
    assert_eq!(events(&es)[0]["detail"], "1 marcado por el laboratorio");
    Ok(())
}

#[tokio::test]
async fn test_timeline_filters_scopes_and_access() -> R {
    let a = app().await?;
    seed(&a).await?;
    let (_, only) = a
        .get(
            "alex",
            "/profiles/alex/timeline?types=vaccine,diagnosis&from=2020-01-01&to=2022-01-01",
        )
        .await?;
    let got: Vec<(String, String)> = events(&only)
        .iter()
        .map(|e| (strs(e, "type"), strs(e, "date")))
        .collect();
    assert_eq!(got, [("diagnosis".to_string(), "2021-11-09".to_string())]);
    let (s, v) = a.get("alex", "/profiles/alex/timeline?types=bogus,lab,zzz").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        v,
        json!({"detail": {"code": "bad_input", "message": "Unknown types: ['bogus', 'zzz']"}})
    );
    let (s, v) = a.get("alex", "/profiles/alex/timeline?from=yesterday").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["detail"][0]["loc"], json!(["query", "from"]));

    // Labs only: no records, no sensitive rows, and the withheld types are named.
    let alex = a.family.profile("alex");
    let categories = |cats: &'static [&'static str]| {
        sqlx::query("update profile_grant set categories = $1 where user_id = $2 and profile_id = $3")
            .bind(cats.iter().map(|c| c.to_string()).collect::<Vec<_>>())
            .bind(a.family.user("alex"))
            .bind(alex)
            .execute(&a.rt.pool)
    };
    categories(&["read:labs"]).await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline").await?;
    assert!(events(&out).iter().all(|e| e["type"] == "lab"), "{out}");
    assert!(!events(&out).is_empty());
    assert_eq!(
        out["withheld"]["by_scope"],
        json!([
            "diagnosis",
            "document",
            "genome",
            "imaging",
            "medication",
            "procedure",
            "vaccine",
            "visit"
        ])
    );
    // Records only: the sensitive diagnosis stays out.
    categories(&["read:records"]).await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline").await?;
    let titles: Vec<String> = events(&out).iter().map(|e| strs(e, "title")).collect();
    assert!(titles.iter().any(|t| t == "Hypercholesterolaemia") && !titles.iter().any(|t| t == "Hidden thing"));

    // Sam has no grant on Carmen.
    let (s, v) = a.get("sam", "/profiles/carmen/timeline").await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let n = a
        .count(&format!(
            "select count(*) from audit_event where resource_type = 'timeline' and profile_id = '{alex}'"
        ))
        .await?;
    assert!(n >= 3, "{n}");
    let withheld: Option<Value> = sqlx::query_scalar(
        "select withheld from audit_event where resource_type = 'timeline' and withheld is not null order by id limit 1",
    )
    .fetch_one(&a.rt.pool)
    .await?;
    assert_eq!(
        withheld,
        Some(
            json!({"by_scope": ["diagnosis", "document", "genome", "imaging", "medication", "procedure", "vaccine", "visit"]})
        )
    );
    Ok(())
}

/// A `lang` the registry does not hold is refused, as on every REST route; an empty one counts as
/// absent, and an absent one reads English, this route's default since 0.4.0.
#[tokio::test]
async fn a_lang_outside_the_registry_is_refused_and_an_empty_one_reads_the_default() -> R {
    let a = app().await?;
    seed(&a).await?;
    for bad in ["fr", "en-US", "EN"] {
        let (s, out) = a.get("alex", &format!("/profiles/alex/timeline?lang={bad}")).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {out}");
        assert_eq!(
            (
                &out["detail"][0]["type"],
                &out["detail"][0]["loc"],
                &out["detail"][0]["input"]
            ),
            (&json!("literal_error"), &json!(["query", "lang"]), &json!(bad)),
            "{out}"
        );
    }
    for uri in ["/profiles/alex/timeline", "/profiles/alex/timeline?lang="] {
        let (s, out) = a.get("alex", uri).await?;
        assert_eq!(s, StatusCode::OK, "{uri}: {out}");
        let titles: Vec<String> = events(&out).iter().map(|e| strs(e, "title")).collect();
        assert!(
            titles.iter().any(|t| t == "Lab report · 2 results"),
            "{uri}: {titles:?}"
        );
    }
    Ok(())
}

/// An imaging report document of Alex's, dated `day`: its id.
async fn report_document(
    c: &mut sqlx::PgConnection,
    a: &App,
    name: &str,
    day: &str,
    sha: &str,
) -> anyhow::Result<uuid::Uuid> {
    Ok(sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type, \
         provider_name, collected_at) values ($1, $2, decode($3, 'hex'), $4, 'application/pdf', 10, 'imaging_report', \
         'LabNorte Imagen', ($5::date + time '10:00') at time zone 'UTC') returning id",
    )
    .bind(a.family.household_id)
    .bind(a.family.profile("alex"))
    .bind(sha.repeat(32))
    .bind(name)
    .bind(day)
    .fetch_one(c)
    .await?)
}

/// A study on Alex's record as the DICOM import writes it (src kind `dicom`): its id.
async fn study(
    c: &mut sqlx::PgConnection,
    a: &App,
    uid: &str,
    orthanc_id: Option<&str>,
    (modality, description, day): (Option<&str>, Option<&str>, &str),
    report: Option<uuid::Uuid>,
) -> anyhow::Result<uuid::Uuid> {
    let alex = a.family.profile("alex");
    let src = create_src(
        &mut *c,
        &SrcIn {
            profile_id: Some(alex),
            import_ref: Some(format!("imaging_import/test#{uid}")),
            label: Some("CD_Rivera.zip".into()),
            ..SrcIn::new("dicom")
        },
    )
    .await?;
    Ok(sqlx::query_scalar(
        "insert into imaging_study (profile_id, study_instance_uid, orthanc_id, modality, study_date, description, \
         report_document_id, status, src_id, fhir) values ($1, $2, $3, $4, $5::date, $6, $7, 'current', $8, $9) \
         returning id",
    )
    .bind(alex)
    .bind(uid)
    .bind(orthanc_id)
    .bind(modality)
    .bind(day)
    .bind(description)
    .bind(report)
    .bind(src)
    .bind(json!({"resourceType": "ImagingStudy", "numberOfSeries": 2, "numberOfInstances": 6}))
    .fetch_one(c)
    .await?)
}

fn of_type(v: &Value, t: &str) -> Vec<Value> {
    events(v).into_iter().filter(|e| e["type"] == t).collect()
}

/// Slice 42: a study is one timeline event. It names the study, the report the import linked to it
/// and the path that opens the viewer, and that report is no second (document) event. Imaging stays
/// under read:records, and the read is audited like any timeline read.
#[tokio::test]
async fn an_imaging_study_is_one_event_with_its_report_and_its_viewer_link() -> R {
    let a = app().await?;
    let mut c = a.conn().await?;
    let report = report_document(&mut c, &a, "Informe_RM_rodilla.pdf", "2026-09-14", "a1").await?;
    // a second report of the same day that no study links stays a document of its own
    let loose = report_document(&mut c, &a, "Informe_suelto.pdf", "2026-09-14", "a2").await?;
    let knee_uid = "1.2.826.0.1.3680043.10.42.1";
    let knee = study(
        &mut c,
        &a,
        knee_uid,
        Some("0b1c2d3e-orthanc-knee"),
        (Some("MR"), Some("RM rodilla derecha"), "2026-09-14"),
        Some(report),
    )
    .await?;
    // not in Orthanc (held, then recorded without pixels): no viewer link, and no report
    let chest = study(
        &mut c,
        &a,
        "1.2.826.0.1.3680043.10.42.2",
        None,
        (Some("CR"), Some("Rx tórax"), "2025-01-02"),
        None,
    )
    .await?;
    // a disc that named nothing: the study is named in the reader's language
    study(
        &mut c,
        &a,
        "1.2.826.0.1.3680043.10.42.3",
        Some("orthanc-3"),
        (None, None, "2024-05-05"),
        None,
    )
    .await?;
    drop(c);

    let (s, out) = a.get("alex", "/profiles/alex/timeline?lang=en").await?;
    assert_eq!(s, StatusCode::OK, "{out}");
    let imaging = of_type(&out, "imaging");
    assert_eq!(imaging.len(), 3, "{out}");
    let k = &imaging[0];
    assert_eq!(
        (&k["date"], &k["title"], &k["detail"]),
        (&json!("2026-09-14"), &json!("RM rodilla derecha"), &json!("MR"))
    );
    assert_eq!(k["study_id"], json!(knee));
    assert_eq!(k["study_instance_uid"], json!(knee_uid));
    assert_eq!(k["report_document_id"], json!(report));
    assert_eq!(
        k["viewer_open"],
        json!(format!("/api/profiles/alex/imaging/studies/{knee_uid}/open"))
    );
    assert_eq!((&k["series"], &k["instances"]), (&json!(2), &json!(6)));
    assert!(strs(k, "src").starts_with("src_"), "{k}");
    let c = &imaging[1];
    assert_eq!(c["study_id"], json!(chest));
    assert_eq!(
        (&c["report_document_id"], &c["viewer_open"]),
        (&Value::Null, &Value::Null)
    );
    assert_eq!(imaging[2]["title"], "Imaging study");
    // the linked report folds into its study; the other report stays a document event
    let docs: Vec<Value> = of_type(&out, "document")
        .iter()
        .map(|d| d["document_id"].clone())
        .collect();
    assert_eq!(docs, [json!(loose)], "{out}");

    // Spanish names a study the disc did not name
    let (_, es) = a.get("alex", "/profiles/alex/timeline?lang=es&types=imaging").await?;
    assert_eq!(of_type(&es, "imaging")[2]["title"], "Estudio de imagen");
    assert!(of_type(&es, "document").is_empty(), "{es}");
    // documents asked without imaging: the report is still a document, so it is never lost
    let (_, only_docs) = a.get("alex", "/profiles/alex/timeline?types=document").await?;
    let mut ids: Vec<String> = of_type(&only_docs, "document")
        .iter()
        .map(|d| strs(d, "document_id"))
        .collect();
    ids.sort();
    let mut want = vec![report.to_string(), loose.to_string()];
    want.sort();
    assert_eq!(ids, want);

    // a retracted report is no longer named by its study (and is no document event either)
    sqlx::query("update document set status = 'retracted' where id = $1")
        .bind(report)
        .execute(&a.rt.pool)
        .await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline?types=imaging,document").await?;
    assert_eq!(of_type(&out, "imaging")[0]["report_document_id"], Value::Null, "{out}");
    assert_eq!(of_type(&out, "document").len(), 1, "{out}");
    sqlx::query("update document set status = 'active' where id = $1")
        .bind(report)
        .execute(&a.rt.pool)
        .await?;

    // The scope stays read:records: labs only sees no study and is told imaging was withheld ...
    let alex = a.family.profile("alex");
    let categories = |cats: &'static [&'static str]| {
        sqlx::query("update profile_grant set categories = $1 where user_id = $2 and profile_id = $3")
            .bind(cats.iter().map(|c| c.to_string()).collect::<Vec<_>>())
            .bind(a.family.user("alex"))
            .bind(alex)
            .execute(&a.rt.pool)
    };
    categories(&["read:labs"]).await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline?types=imaging").await?;
    assert!(events(&out).is_empty(), "{out}");
    assert_eq!(out["withheld"]["by_scope"], json!(["imaging"]));
    // ... and read:records alone sees the study with its report
    categories(&["read:records"]).await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline?types=imaging").await?;
    assert_eq!(
        of_type(&out, "imaging")[0]["report_document_id"],
        json!(report),
        "{out}"
    );
    let audited: Vec<(Option<i32>, Option<Value>)> = sqlx::query_as(
        "select rows_out, withheld from audit_event where resource_type = 'timeline' and profile_id = $1 \
         order by id desc limit 2",
    )
    .bind(alex)
    .fetch_all(&a.rt.pool)
    .await?;
    assert_eq!(
        audited,
        [(Some(3), None), (Some(0), Some(json!({"by_scope": ["imaging"]})))]
    );
    // Emergency access reads the study and its report, but the viewer never opens for it
    // (imaging_proxy.rs answers 403 break_glass), so the event offers no viewer link.
    sqlx::query(
        "update profile_grant set break_glass = true, expires_at = now() + interval '14 days' \
         where user_id = $1 and profile_id = $2",
    )
    .bind(a.family.user("alex"))
    .bind(alex)
    .execute(&a.rt.pool)
    .await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline?types=imaging").await?;
    let k = &of_type(&out, "imaging")[0];
    assert_eq!(
        (&k["study_id"], &k["report_document_id"], &k["viewer_open"]),
        (&json!(knee), &json!(report), &Value::Null),
        "{out}"
    );
    Ok(())
}
