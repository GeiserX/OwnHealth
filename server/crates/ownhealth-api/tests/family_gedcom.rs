//! v1.0 slice 24: a GEDCOM family tree proposes relatives (`POST /profiles/{p}/family/gedcom/preview`
//! and `POST /profiles/{p}/family/gedcom`). The file is read in memory and never stored; what it does
//! leave behind is checked in `gedcom_never_stored.rs`. Each test runs on its own throwaway database
//! as a browser session.

use axum::body::Body;
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::access::{scope_set, set_grant};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::family::{self, MemberIn};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

type R = anyhow::Result<()>;

const F551: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../fixtures/gedcom/rivera-551.ged"
));
const F70: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../fixtures/gedcom/rivera-70.ged"
));
const BOUNDARY: &str = "ownhealthgedcomboundary";

struct App {
    _db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    family: Family,
}

async fn app() -> anyhow::Result<App> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    Ok(App {
        _db: db,
        _dir: dir,
        rt,
        family,
    })
}

fn form(file: &[u8], me: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(me) = me {
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"self\"\r\n\r\n{me}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"arbol.ged\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn call(
    a: &App,
    who: &str,
    method: &str,
    uri: &str,
    body: Body,
    json_body: bool,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(who), None, None, "passkey").await?;
    let content_type = if json_body {
        "application/json".to_string()
    } else {
        format!("multipart/form-data; boundary={BOUNDARY}")
    };
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("{COOKIE}={secret}"))
        .header(header::CONTENT_TYPE, content_type)
        .body(body)?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 26).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

async fn upload(a: &App, who: &str, uri: &str, file: &[u8], me: Option<&str>) -> anyhow::Result<(StatusCode, Value)> {
    call(a, who, "POST", uri, Body::from(form(file, me)), false).await
}

async fn get(a: &App, who: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
    call(a, who, "GET", uri, Body::empty(), true).await
}

async fn post(a: &App, who: &str, uri: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
    call(a, who, "POST", uri, Body::from(body.to_string()), true).await
}

async fn proposals(a: &App) -> anyhow::Result<Vec<Value>> {
    let (s, v) = get(a, "alex", "/review?kind=family_proposal&profile=alex").await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

async fn members(a: &App) -> anyhow::Result<i64> {
    Ok(
        sqlx::query_scalar("select count(*) from family_member where profile_id = $1 and status = 'current'")
            .bind(a.family.profile("alex"))
            .fetch_one(&a.rt.pool)
            .await?,
    )
}

#[tokio::test]
async fn a_tree_proposes_each_relative_once_and_accept_adds_one_while_dismiss_adds_none() -> R {
    let a = app().await?;
    let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom/preview", F551, None).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["version"], "5.5.1");
    let people = v["people"].as_array().cloned().unwrap_or_default();
    assert_eq!(people.len(), 19);
    // the picker gets structure only: no name, note or place of anyone
    assert_eq!(
        people[0],
        json!({"i": 0, "sex": "male", "born_year": 1985, "deceased": false, "death_age": null,
               "parents": [1, 2], "partners": [9], "children": [10]})
    );
    for p in &people {
        let keys: Vec<&String> = p.as_object().map(|o| o.keys().collect()).unwrap_or_default();
        assert_eq!(keys.len(), 8, "{p}");
    }

    let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom", F551, Some("0")).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["proposed"].as_array().map(Vec::len), Some(14), "{v}");
    assert_eq!(
        (v["already"].as_u64(), v["left_out"].as_u64(), v["outside"].as_u64()),
        (Some(0), Some(0), Some(4))
    );
    let items = proposals(&a).await?;
    let mut rels: Vec<&str> = items
        .iter()
        .filter_map(|i| i["payload"]["relationship"].as_str())
        .collect();
    // the queue shows them closest relative first, the order the import made them in
    assert_eq!(
        rels,
        [
            "FTH", "MTH", "SPS", "DAU", "SIS", "HBRO", "PGRFTH", "PGRMTH", "MGRMTH", "MGRFTH", "PUNCLE", "NEPHEW",
            "PCOUSN", "FTHINLAW"
        ]
    );
    rels.sort_unstable();
    assert_eq!(
        rels,
        [
            "DAU", "FTH", "FTHINLAW", "HBRO", "MGRFTH", "MGRMTH", "MTH", "NEPHEW", "PCOUSN", "PGRFTH", "PGRMTH",
            "PUNCLE", "SIS", "SPS"
        ]
    );
    let father = items
        .iter()
        .find(|i| i["payload"]["relationship"] == "FTH")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        father["payload"],
        json!({"source": "gedcom", "version": "5.5.1", "relationship": "FTH",
               "label": {"es": "padre", "en": "father"}, "sex_at_birth": "male", "born_year": 1955,
               "deceased": true, "death_age": 64})
    );
    let mother = items
        .iter()
        .find(|i| i["payload"]["relationship"] == "MTH")
        .cloned()
        .unwrap_or_default();

    let before = members(&a).await?;
    let (s, v) = post(
        &a,
        "alex",
        &format!("/review/{}/accept", father["id"].as_str().unwrap_or("")),
        json!({}),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(members(&a).await?, before + 1);
    let (_, fam) = get(&a, "alex", "/profiles/alex/family").await?;
    let added = fam["items"]
        .as_array()
        .and_then(|l| l.iter().find(|m| m["relationship"] == "FTH"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (
            &added["born_year"],
            &added["deceased"],
            &added["death_age"],
            &added["display_name"]
        ),
        (&json!(1955), &json!(true), &json!(64), &Value::Null)
    );
    let label: Option<String> = sqlx::query_scalar("select label from src where id = $1")
        .bind(added["src"].as_str().unwrap_or(""))
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(label.as_deref(), Some("GEDCOM"));

    let (s, v) = post(
        &a,
        "alex",
        &format!("/review/{}/dismiss", mother["id"].as_str().unwrap_or("")),
        json!({"reason": "not in this family"}),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(members(&a).await?, before + 1);

    // the same file again: the open ones and the accepted father are already there; the dismissed
    // mother is proposed again
    let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom", F551, Some("0")).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        (v["proposed"].as_array().map(Vec::len), v["already"].as_u64()),
        (Some(1), Some(13)),
        "{v}"
    );
    // a proposal is accepted as it is
    let open = proposals(&a).await?;
    let (s, v) = post(
        &a,
        "alex",
        &format!("/review/{}/accept", open[0]["id"].as_str().unwrap_or("")),
        json!({"edits": {"born_year": 1900}}),
    )
    .await?;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    Ok(())
}

#[tokio::test]
async fn a_gedcom_7_file_in_the_maximal70_shape_parses_and_proposes() -> R {
    let a = app().await?;
    let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom/preview", F70, None).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["version"], "7.0");
    assert_eq!(v["people"].as_array().map(Vec::len), Some(6));
    // @I4@ (position 3), child of @I1@ and @I2@
    let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom", F70, Some("3")).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let items = proposals(&a).await?;
    let mut got: Vec<(String, Value, Value)> = items
        .iter()
        .map(|i| {
            let p = &i["payload"];
            (
                p["relationship"].as_str().unwrap_or("").to_string(),
                p["born_year"].clone(),
                p["death_age"].clone(),
            )
        })
        .collect();
    got.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(
        got,
        [
            ("FTH".to_string(), json!(2000), json!(22)),
            ("MTH".to_string(), Value::Null, Value::Null)
        ]
    );
    Ok(())
}

#[tokio::test]
async fn bad_files_are_refused_and_name_no_value() -> R {
    let a = app().await?;
    let uri = "/profiles/alex/family/gedcom/preview";
    for (file, code) in [
        (b"%PDF-1.7 Vexlorn".to_vec(), "not_gedcom"),
        (
            b"0 HEAD\n0 @I1@ INDI\n1 NAME Vexlorn\n1 FAMC @F1@\n1 FAMS @F1@\n0 @F1@ FAM\n".to_vec(),
            "cycle",
        ),
        (
            format!("0 HEAD\n1 NOTE {}\n", "Vexlorn".repeat(5000)).into_bytes(),
            "line_too_long",
        ),
        (b"0 HEAD\n0 @I1@ INDI\n3 NAME Vexlorn\n".to_vec(), "level_jump"),
    ] {
        let (s, v) = upload(&a, "alex", uri, &file, None).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{code}: {v}");
        assert_eq!(v["detail"]["code"], "bad_gedcom", "{v}");
        assert_eq!(v["detail"]["reason"], code, "{v}");
        assert!(!v.to_string().contains("Vexlorn"), "{v}");
    }
    // past 10 MiB
    let big = vec![b' '; 10 * 1024 * 1024 + 1];
    let (s, v) = upload(&a, "alex", uri, &big, None).await?;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["detail"]["code"], "too_large", "{v}");
    // no file, or a position that is not in the file
    let (s, v) = call(
        &a,
        "alex",
        "POST",
        uri,
        Body::from(format!("--{BOUNDARY}--\r\n")),
        false,
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // a `self` past 16 bytes is refused, even when it would read as a position in the file
    for me in ["19", "-1", "x", "", "00000000000000000"] {
        let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom", F551, Some(me)).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{me}: {v}");
    }
    assert!(proposals(&a).await?.is_empty());
    Ok(())
}

async fn grant_sam(a: &App, categories: &[&str]) -> R {
    let mut c = a.rt.pool.acquire().await?;
    set_grant(
        &mut c,
        a.family.user("sam"),
        a.family.profile("alex"),
        "caregiver",
        Some(&scope_set(categories.iter().copied())),
        None,
        None,
    )
    .await?;
    Ok(())
}

async fn shown_to_sam(a: &App) -> anyhow::Result<usize> {
    let (s, v) = get(a, "sam", "/review?kind=family_proposal&profile=alex").await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    Ok(v["items"].as_array().map_or(0, Vec::len))
}

#[tokio::test]
async fn sending_a_tree_needs_import_and_read_records_and_so_does_seeing_its_proposals() -> R {
    let a = app().await?;
    // Sam is a viewer of Alex's profile: no import
    let (s, v) = upload(&a, "sam", "/profiles/alex/family/gedcom", F551, Some("0")).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = upload(&a, "sam", "/profiles/alex/family/gedcom/preview", F551, None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, _) = upload(&a, "alex", "/profiles/alex/family/gedcom", F551, Some("0")).await?;
    assert_eq!(s, StatusCode::CREATED);
    let one = proposals(&a).await?.first().cloned().unwrap_or_default();
    let one = format!("/review/{}", one["id"].as_str().unwrap_or(""));

    // a caregiver may import and review, but without read:records sees no relative
    grant_sam(&a, &["read:summary", "read:labs"]).await?;
    let (s, v) = upload(&a, "sam", "/profiles/alex/family/gedcom", F551, Some("0")).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(shown_to_sam(&a).await?, 0);
    let (s, _) = get(&a, "sam", &one).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = post(&a, "sam", &format!("{one}/accept"), json!({})).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);

    grant_sam(&a, &["read:summary", "read:records"]).await?;
    assert_eq!(shown_to_sam(&a).await?, 14);
    let (s, v) = get(&a, "sam", &one).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    Ok(())
}

/// A relative recorded on the profile, entered by Alex.
async fn record(a: &App, relationship: &str, sex: &str, born_year: Option<i32>, sensitivity: &[&str]) -> R {
    let alex = a.family.profile("alex");
    let mut m = MemberIn::new(alex, relationship);
    m.sex_at_birth = Some(sex.into());
    m.born_year = born_year;
    if relationship == "FTH" {
        m.deceased = true;
        m.death_age = Some(64);
    }
    m.sensitivity = sensitivity.iter().map(|s| (*s).to_string()).collect();
    let src = SrcRef::New(SrcIn {
        profile_id: Some(alex),
        ..SrcIn::manual(Some(a.family.user("alex")))
    });
    let mut c = a.rt.pool.acquire().await?;
    family::add(&mut c, &m, &src, "person", None, None).await?;
    Ok(())
}

#[tokio::test]
async fn the_import_counts_no_sensitive_relative_the_caller_cannot_see() -> R {
    let a = app().await?;
    // Alex's father is recorded with a sensitivity tag; Sam has read:records but not read:sensitive
    record(&a, "FTH", "male", Some(1955), &["mental_health"]).await?;
    grant_sam(&a, &["read:summary", "read:records"]).await?;
    let (s, fam) = get(&a, "sam", "/profiles/alex/family").await?;
    assert_eq!(s, StatusCode::OK, "{fam}");
    assert!(
        !fam["items"]
            .as_array()
            .is_some_and(|l| l.iter().any(|m| m["relationship"] == "FTH")),
        "{fam}"
    );

    // Alex reads sensitive rows: the father is already there
    let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom", F551, Some("0")).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        (v["proposed"].as_array().map(Vec::len), v["already"].as_u64()),
        (Some(13), Some(1)),
        "{v}"
    );

    // Sam does not: the father is proposed, and `already` counts only the 13 open proposals Sam sees
    let (s, v) = upload(&a, "sam", "/profiles/alex/family/gedcom", F551, Some("0")).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        (v["proposed"].as_array().map(Vec::len), v["already"].as_u64()),
        (Some(1), Some(13)),
        "{v}"
    );
    Ok(())
}

#[tokio::test]
async fn the_import_reveals_no_birth_year_the_family_list_hides() -> R {
    let a = app().await?;
    // Alex records a mother born in 1955 and links her to Carmen's profile; Sam reads Alex's records
    // but not Carmen's, so the family list masks the row's birth year for Sam
    let alex = a.family.profile("alex");
    let mut m = MemberIn::new(alex, "MTH");
    m.sex_at_birth = Some("female".into());
    m.born_year = Some(1955);
    let src = SrcRef::New(SrcIn {
        profile_id: Some(alex),
        ..SrcIn::manual(Some(a.family.user("alex")))
    });
    {
        let mut c = a.rt.pool.acquire().await?;
        family::add(&mut c, &m, &src, "person", None, Some(a.family.profile("carmen"))).await?;
    }
    grant_sam(&a, &["read:summary", "read:records"]).await?;
    let mother = |fam: &Value| {
        let row = fam["items"]
            .as_array()
            .and_then(|l| l.iter().find(|m| m["relationship"] == "MTH").cloned())
            .unwrap_or_default();
        (row["masked"].as_bool(), row["born_year"].as_i64())
    };
    let (s, fam) = get(&a, "sam", "/profiles/alex/family").await?;
    assert_eq!(s, StatusCode::OK, "{fam}");
    assert_eq!(mother(&fam), (Some(true), None), "{fam}");
    let (s, fam) = get(&a, "alex", "/profiles/alex/family").await?;
    assert_eq!(s, StatusCode::OK, "{fam}");
    assert_eq!(mother(&fam), (Some(false), Some(1955)), "{fam}");

    // a tree of one mother, born in the year given
    let tree = |y: i32| {
        format!(
            "0 HEAD\n1 GEDC\n2 VERS 5.5.1\n0 @ME@ INDI\n1 SEX M\n1 FAMC @F1@\n0 @M@ INDI\n1 SEX F\n1 BIRT\n2 DATE {y}\n\
0 @F1@ FAM\n1 WIFE @M@\n1 CHIL @ME@\n0 TRLR\n"
        )
    };
    let answer = |v: &Value| (v["proposed"].as_array().map(Vec::len), v["already"].as_u64());
    // Sam gets the same answer whatever the year, so a run of imports cannot find the hidden one
    for y in [1954, 1955, 1956] {
        let (s, v) = upload(&a, "sam", "/profiles/alex/family/gedcom", tree(y).as_bytes(), Some("0")).await?;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        assert_eq!(answer(&v), (Some(1), Some(0)), "born {y}: {v}");
        sqlx::query("delete from review_item where kind = 'family_proposal'")
            .execute(&a.rt.pool)
            .await?;
    }
    // Alex sees the mother and her year: she is already there
    let (s, v) = upload(
        &a,
        "alex",
        "/profiles/alex/family/gedcom",
        tree(1955).as_bytes(),
        Some("0"),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(answer(&v), (Some(0), Some(1)), "{v}");
    Ok(())
}

#[tokio::test]
async fn relatives_who_look_alike_are_each_proposed() -> R {
    let a = app().await?;
    // a father and three brothers, none with a birth year; one brother is recorded already
    let tree = b"0 HEAD\n1 GEDC\n2 VERS 5.5.1\n\
0 @ME@ INDI\n1 SEX M\n1 FAMC @F1@\n\
0 @B1@ INDI\n1 SEX M\n1 FAMC @F1@\n\
0 @B2@ INDI\n1 SEX M\n1 FAMC @F1@\n\
0 @B3@ INDI\n1 SEX M\n1 FAMC @F1@\n\
0 @F@ INDI\n1 SEX M\n1 FAMS @F1@\n\
0 @F1@ FAM\n1 HUSB @F@\n1 CHIL @ME@\n1 CHIL @B1@\n1 CHIL @B2@\n1 CHIL @B3@\n0 TRLR\n";
    record(&a, "BRO", "male", None, &[]).await?;
    let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom", tree, Some("0")).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        (v["proposed"].as_array().map(Vec::len), v["already"].as_u64()),
        (Some(3), Some(1)),
        "{v}"
    );
    let mut rels: Vec<String> = proposals(&a)
        .await?
        .iter()
        .filter_map(|i| i["payload"]["relationship"].as_str().map(str::to_string))
        .collect();
    rels.sort_unstable();
    assert_eq!(rels, ["BRO", "BRO", "FTH"]);
    // the same file again: each open proposal and the recorded brother stand for one relative
    let (s, v) = upload(&a, "alex", "/profiles/alex/family/gedcom", tree, Some("0")).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        (v["proposed"].as_array().map(Vec::len), v["already"].as_u64()),
        (Some(0), Some(4)),
        "{v}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_imports_at_once_queue_each_relative_once() -> R {
    // without the import's lock both see no open proposal and queue every relative twice
    for _ in 0..3 {
        let a = std::sync::Arc::new(app().await?);
        let (a1, a2) = (a.clone(), a.clone());
        let t1 =
            tokio::spawn(async move { upload(&a1, "alex", "/profiles/alex/family/gedcom", F551, Some("0")).await });
        let t2 =
            tokio::spawn(async move { upload(&a2, "alex", "/profiles/alex/family/gedcom", F551, Some("0")).await });
        let (r1, r2) = (t1.await??, t2.await??);
        assert_eq!(
            (r1.0, r2.0),
            (StatusCode::CREATED, StatusCode::CREATED),
            "{} {}",
            r1.1,
            r2.1
        );
        let n: i64 =
            sqlx::query_scalar("select count(*) from review_item where kind = 'family_proposal' and profile_id = $1")
                .bind(a.family.profile("alex"))
                .fetch_one(&a.rt.pool)
                .await?;
        assert_eq!(n, 14);
    }
    Ok(())
}
