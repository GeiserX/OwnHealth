//! The hospital CDs the imaging tests read, written by an independent DICOM implementation
//! (dicom-object) at run time, and a fake Orthanc that files what it gets as Orthanc does: a study per
//! PatientID and StudyInstanceUID. Synthetic people only (the Rivera family, Carmen Ortega).
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::tags;
use dicom_object::meta::FileMetaTableBuilder;
use dicom_object::{InMemDicomObject, OpenFileOptions};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

pub const EXPLICIT_LE: &str = "1.2.840.10008.1.2.1";
pub const IMPLICIT_LE: &str = "1.2.840.10008.1.2";
const CT_IMAGE: &str = "1.2.840.10008.5.1.4.1.1.2";
const ENCAPSULATED_PDF: &str = "1.2.840.10008.5.1.4.1.1.104.1";
const DIRECTORY: &str = "1.2.840.10008.1.3.10";

/// A patient as a CD writes one.
#[derive(Debug, Clone)]
pub struct Person {
    pub name: &'static str,
    pub id: &'static str,
    pub birth: &'static str,
}

pub const ALEX: Person = Person {
    name: "RIVERA^ALEX",
    id: "LN-ALEX-01",
    birth: "19790514",
};
pub const SAM: Person = Person {
    name: "RIVERA^SAM",
    id: "LN-SAM-02",
    birth: "19810202",
};
pub const CARMEN: Person = Person {
    name: "ORTEGA^CARMEN",
    id: "LN-CARMEN-03",
    birth: "19550130",
};

/// One study on a disc: `series` series of `per_series` images each.
#[derive(Debug, Clone)]
pub struct Study {
    pub uid: String,
    pub person: Person,
    pub modality: &'static str,
    pub date: &'static str,
    pub description: &'static str,
    pub series: usize,
    pub per_series: usize,
    /// Each image's side in pixels.
    pub size: u16,
    pub report: bool,
    pub issuer: &'static str,
    pub charset: &'static str,
}

impl Study {
    pub fn new(uid: &str, person: Person) -> Study {
        Study {
            uid: uid.into(),
            person,
            modality: "CT",
            date: "20260914",
            description: "TC TORAX",
            series: 2,
            per_series: 3,
            size: 16,
            report: false,
            issuer: "",
            charset: "",
        }
    }

    pub fn series_uid(&self, s: usize) -> String {
        format!("{}.{}", self.uid, s + 1)
    }

    pub fn images(&self) -> usize {
        self.series * self.per_series
    }
}

fn put(obj: &mut InMemDicomObject, tag: dicom_core::Tag, vr: VR, value: &str) {
    obj.put(DataElement::new(tag, vr, PrimitiveValue::from(value)));
}

fn patient_and_study(obj: &mut InMemDicomObject, st: &Study) {
    put(obj, tags::PATIENT_NAME, VR::PN, st.person.name);
    put(obj, tags::PATIENT_ID, VR::LO, st.person.id);
    if !st.issuer.is_empty() {
        put(obj, tags::ISSUER_OF_PATIENT_ID, VR::LO, st.issuer);
    }
    if !st.charset.is_empty() {
        put(obj, tags::SPECIFIC_CHARACTER_SET, VR::CS, st.charset);
    }
    put(obj, tags::PATIENT_BIRTH_DATE, VR::DA, st.person.birth);
    put(obj, tags::STUDY_INSTANCE_UID, VR::UI, &st.uid);
    put(obj, tags::STUDY_DATE, VR::DA, st.date);
    put(obj, tags::STUDY_DESCRIPTION, VR::LO, st.description);
    put(obj, tags::ACCESSION_NUMBER, VR::SH, "LN2026");
    put(obj, tags::INSTITUTION_NAME, VR::LO, "LabNorte");
}

fn write(obj: InMemDicomObject, ts: &str) -> Vec<u8> {
    let file = obj
        .with_meta(FileMetaTableBuilder::new().transfer_syntax(ts))
        .unwrap_or_else(|e| panic!("meta: {e}"));
    let mut out = Vec::new();
    file.write_all(&mut out).unwrap_or_else(|e| panic!("write: {e}"));
    out
}

/// One image of the study: a gradient that differs per image, so a viewer shows something.
pub fn image(st: &Study, series: usize, n: usize, ts: &str) -> Vec<u8> {
    let mut obj = InMemDicomObject::new_empty();
    put(&mut obj, tags::SOP_CLASS_UID, VR::UI, CT_IMAGE);
    put(
        &mut obj,
        tags::SOP_INSTANCE_UID,
        VR::UI,
        &format!("{}.{}", st.series_uid(series), n + 1),
    );
    put(&mut obj, tags::MODALITY, VR::CS, st.modality);
    patient_and_study(&mut obj, st);
    put(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, &st.series_uid(series));
    put(&mut obj, tags::SERIES_NUMBER, VR::IS, &(series + 1).to_string());
    put(&mut obj, tags::INSTANCE_NUMBER, VR::IS, &(n + 1).to_string());
    obj.put(DataElement::new(
        tags::SAMPLES_PER_PIXEL,
        VR::US,
        PrimitiveValue::from(1u16),
    ));
    put(&mut obj, tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
    obj.put(DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(st.size)));
    obj.put(DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::from(st.size)));
    obj.put(DataElement::new(
        tags::BITS_ALLOCATED,
        VR::US,
        PrimitiveValue::from(8u16),
    ));
    obj.put(DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::from(8u16)));
    obj.put(DataElement::new(tags::HIGH_BIT, VR::US, PrimitiveValue::from(7u16)));
    obj.put(DataElement::new(
        tags::PIXEL_REPRESENTATION,
        VR::US,
        PrimitiveValue::from(0u16),
    ));
    let side = usize::from(st.size);
    let pixels: Vec<u8> = (0..side * side)
        .map(|i| {
            let (x, y) = (i % side, i / side);
            let r2 = (x as i64 - side as i64 / 2).pow(2) + (y as i64 - side as i64 / 2).pow(2);
            let ring = ((r2 as f64).sqrt() as usize + n * 3 + series * 7) % 32 < 16;
            if ring { 200 } else { (x * 255 / side) as u8 / 3 }
        })
        .collect();
    obj.put(DataElement::new(tags::PIXEL_DATA, VR::OB, PrimitiveValue::from(pixels)));
    write(obj, ts)
}

/// One more image of the study, its own SOP instance, with a private element of `n` zero bytes in
/// group 0009: the patient and study tags (groups 0010 and 0020) sit behind it.
pub fn image_with_private_bytes(st: &Study, n: usize) -> Vec<u8> {
    let mut obj = InMemDicomObject::new_empty();
    put(&mut obj, tags::SOP_CLASS_UID, VR::UI, CT_IMAGE);
    put(
        &mut obj,
        tags::SOP_INSTANCE_UID,
        VR::UI,
        &format!("{}.999", st.series_uid(0)),
    );
    put(&mut obj, tags::MODALITY, VR::CS, st.modality);
    patient_and_study(&mut obj, st);
    put(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, &st.series_uid(0));
    put(&mut obj, dicom_core::Tag(0x0009, 0x0010), VR::LO, "SYNTHETIC");
    obj.put(DataElement::new(
        dicom_core::Tag(0x0009, 0x1010),
        VR::OB,
        PrimitiveValue::from(vec![0u8; n]),
    ));
    write(obj, EXPLICIT_LE)
}

/// A small PDF report.
pub fn report_pdf(st: &Study) -> Vec<u8> {
    use ownhealth_core::pdfwrite::{Font, Writer};
    let mut w = Writer::new();
    w.paragraph("LabNorte - Servicio de Radiologia", Font::Bold, 14.0, 20.0);
    w.paragraph(
        &format!("Paciente: {}  Fecha: {}", st.person.name.replace('^', ", "), st.date),
        Font::Regular,
        11.0,
        16.0,
    );
    w.paragraph(&format!("Estudio: {}", st.description), Font::Regular, 11.0, 16.0);
    w.paragraph("Informe sintetico de prueba. Sin hallazgos.", Font::Regular, 11.0, 16.0);
    w.finish().pdf
}

/// The study's report as an Encapsulated PDF instance, in its own series.
pub fn pdf_instance(st: &Study, pdf: &[u8]) -> Vec<u8> {
    let mut obj = InMemDicomObject::new_empty();
    put(&mut obj, tags::SOP_CLASS_UID, VR::UI, ENCAPSULATED_PDF);
    put(&mut obj, tags::SOP_INSTANCE_UID, VR::UI, &format!("{}.99.1", st.uid));
    put(&mut obj, tags::MODALITY, VR::CS, "DOC");
    patient_and_study(&mut obj, st);
    put(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, &format!("{}.99", st.uid));
    put(
        &mut obj,
        tags::MIME_TYPE_OF_ENCAPSULATED_DOCUMENT,
        VR::LO,
        "application/pdf",
    );
    put(&mut obj, tags::DOCUMENT_TITLE, VR::ST, "Informe");
    let mut doc = pdf.to_vec();
    if doc.len() % 2 == 1 {
        doc.push(0);
    }
    obj.put(DataElement::new(
        tags::ENCAPSULATED_DOCUMENT,
        VR::OB,
        PrimitiveValue::from(doc),
    ));
    write(obj, EXPLICIT_LE)
}

/// A DICOMDIR: the disc's index, a DICOM file of its own class.
pub fn dicomdir() -> Vec<u8> {
    let mut obj = InMemDicomObject::new_empty();
    put(&mut obj, tags::FILE_SET_ID, VR::CS, "LABNORTE");
    let file = obj
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(EXPLICIT_LE)
                .media_storage_sop_class_uid(DIRECTORY)
                .media_storage_sop_instance_uid("1.2.826.0.1.3680043.10.999.900"),
        )
        .unwrap_or_else(|e| panic!("meta: {e}"));
    let mut out = Vec::new();
    file.write_all(&mut out).unwrap_or_else(|e| panic!("write: {e}"));
    out
}

/// Every file of the studies, in the folder layout CDs use (`DICOM/S<n>/I<n>`). The first image of
/// each study is written implicit VR, the rest explicit, as discs mix them.
pub fn study_files(st: &Study, dir: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for s in 0..st.series {
        for n in 0..st.per_series {
            let ts = if s == 0 && n == 0 { IMPLICIT_LE } else { EXPLICIT_LE };
            out.push((format!("{dir}/S{s:02}/I{n:04}"), image(st, s, n, ts)));
        }
    }
    if st.report {
        out.push((format!("{dir}/REPORT/I0001"), pdf_instance(st, &report_pdf(st))));
    }
    out
}

/// A hospital CD: the studies, DICOMDIR, the viewer and its autorun, and one damaged image.
pub fn hospital_cd(studies: &[Study]) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![
        ("DICOMDIR".to_string(), dicomdir()),
        (
            "viewer.exe".to_string(),
            b"MZ\x90\x00\x03 synthetic viewer, not a program".to_vec(),
        ),
        ("autorun.inf".to_string(), b"[autorun]\r\nopen=viewer.exe\r\n".to_vec()),
        ("README.TXT".to_string(), b"LabNorte. Synthetic test disc.".to_vec()),
    ];
    for (k, st) in studies.iter().enumerate() {
        files.extend(study_files(st, &format!("DICOM/P{k:02}")));
    }
    if let Some(st) = studies.first() {
        // the preamble, the magic and half a file meta group: DICOM that does not read
        let mut damaged = image(st, 0, 0, EXPLICIT_LE);
        damaged.truncate(150);
        files.push(("DICOM/DAMAGED/I0001".into(), damaged));
    }
    files
}

pub fn zip_files(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.add_directory("DICOM/", opts).unwrap_or_else(|e| panic!("zip: {e}"));
        for (name, bytes) in files {
            zip.start_file(name.as_str(), opts)
                .unwrap_or_else(|e| panic!("zip: {e}"));
            zip.write_all(bytes).unwrap_or_else(|e| panic!("zip: {e}"));
        }
        zip.finish().unwrap_or_else(|e| panic!("zip: {e}"));
    }
    out.into_inner()
}

// ------------------------------------------------------------------------------------- Orthanc ----

pub const USER: &str = "ownhealth";

/// An Orthanc id from the values Orthanc hashes: five groups of eight hex digits.
fn orthanc_id(parts: &[&str]) -> String {
    let h = hex::encode(Sha1::digest(parts.join("|").as_bytes()));
    [&h[0..8], &h[8..16], &h[16..24], &h[24..32], &h[32..40]].join("-")
}

#[derive(Debug, Clone, Default)]
pub struct Held {
    /// Orthanc study id -> (StudyInstanceUID, PatientID).
    pub studies: BTreeMap<String, (String, String)>,
    /// Orthanc instance id -> (its study id, SeriesInstanceUID).
    pub instances: BTreeMap<String, (String, String)>,
    /// Orthanc instance ID -> (SOPInstanceUID, original file bytes).
    instance_files: BTreeMap<String, (String, axum::body::Bytes)>,
    /// SeriesInstanceUID -> Modality.
    pub modalities: BTreeMap<String, String>,
    /// Every request but `/tools/lookup` and `GET /studies/{id}`: (method, path).
    pub log: Vec<(String, String)>,
}

type AfterStore = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct FakeOrthanc {
    pub url: String,
    pub password: String,
    pub held: Arc<Mutex<Held>>,
    /// How long each `POST /instances` takes.
    pub delay: Arc<Mutex<Duration>>,
    /// While set, the next `/tools/lookup` waits (up to a second) for a second one to arrive, so two
    /// writers that do not take the one UID lock would both read Orthanc before either sends.
    pub pair_lookups: Arc<AtomicBool>,
    waiting: Arc<(AtomicUsize, tokio::sync::Notify)>,
    /// Series metadata reads fail while this is set.
    pub metadata_fails: Arc<Mutex<bool>>,
    /// Files every instance of this StudyInstanceUID under another one, as an Orthanc that read the
    /// files otherwise than the importer would.
    pub misread: Arc<Mutex<Option<(String, String)>>>,
    /// The same for a PatientID: files every instance of this patient under another one.
    pub misread_patient: Arc<Mutex<Option<(String, String)>>>,
    pub misread_except_series: Arc<Mutex<Option<String>>>,
    pub fail_delete_at: Arc<AtomicUsize>,
    pub deletes: Arc<AtomicUsize>,
    pub lose_store_reply_at: Arc<AtomicUsize>,
    pub store_status: Arc<Mutex<Option<Value>>>,
    /// Answers Success, not AlreadyStored, for an instance it holds already (OverwriteInstances).
    pub overwrite: Arc<AtomicBool>,
    pub refused_sop: Arc<Mutex<Option<String>>>,
    pub lookup_response: Arc<Mutex<Option<Value>>>,
    pub after_store: Arc<Mutex<Option<AfterStore>>>,
    /// While set, every request answers 503, as a stopped Orthanc behind its proxy does.
    pub down: Arc<AtomicBool>,
}

fn json_answer(status: StatusCode, v: Value) -> Response {
    let mut r = (status, v.to_string()).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    r
}

impl FakeOrthanc {
    pub async fn start() -> anyhow::Result<FakeOrthanc> {
        let fake = FakeOrthanc {
            url: String::new(),
            password: ownhealth_core::ids::token_hex(16),
            held: Arc::default(),
            delay: Arc::default(),
            pair_lookups: Arc::default(),
            waiting: Arc::default(),
            metadata_fails: Arc::default(),
            misread: Arc::default(),
            misread_patient: Arc::default(),
            misread_except_series: Arc::default(),
            fail_delete_at: Arc::default(),
            deletes: Arc::default(),
            lose_store_reply_at: Arc::default(),
            store_status: Arc::default(),
            overwrite: Arc::default(),
            refused_sop: Arc::default(),
            lookup_response: Arc::default(),
            after_store: Arc::default(),
            down: Arc::default(),
        };
        let me = fake.clone();
        let app = Router::new().fallback(move |req: Request| {
            let me = me.clone();
            async move { me.answer(req).await }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(FakeOrthanc { url, ..fake })
    }

    pub fn config(&self) -> ownhealth_core::orthanc::OrthancConfig {
        ownhealth_core::orthanc::OrthancConfig {
            url: self.url.clone(),
            user: USER.into(),
            password: self.password.clone(),
        }
    }

    pub fn held(&self) -> Held {
        self.held.lock().map(|h| h.clone()).unwrap_or_default()
    }

    /// The PatientIDs Orthanc holds a study for under `uid`.
    pub fn patients_under(&self, uid: &str) -> BTreeSet<String> {
        self.held()
            .studies
            .values()
            .filter(|(u, _)| u == uid)
            .map(|(_, p)| p.clone())
            .collect()
    }

    pub fn posts(&self) -> usize {
        self.held()
            .log
            .iter()
            .filter(|(m, p)| m == "POST" && p == "/instances")
            .count()
    }

    pub fn warmed(&self) -> Vec<String> {
        self.held()
            .log
            .iter()
            .filter(|(m, p)| m == "GET" && p.ends_with("/metadata"))
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// Every study, series and instance gone, as an Orthanc that starts on an empty volume (a restore
    /// on a new box: Orthanc's storage is not in the backup).
    pub fn wipe(&self) {
        if let Ok(mut h) = self.held.lock() {
            h.studies.clear();
            h.instances.clear();
            h.instance_files.clear();
        }
    }

    /// The SeriesInstanceUIDs Orthanc holds under study `uid`.
    pub fn series_under(&self, uid: &str) -> BTreeSet<String> {
        let held = self.held();
        held.instances
            .values()
            .filter(|(study, _)| held.studies.get(study).is_some_and(|(u, _)| u == uid))
            .map(|(_, series)| series.clone())
            .collect()
    }

    /// How many instances Orthanc holds under study `uid`.
    pub fn instances_under(&self, uid: &str) -> usize {
        let held = self.held();
        held.instances
            .values()
            .filter(|(study, _)| held.studies.get(study).is_some_and(|(u, _)| u == uid))
            .count()
    }

    /// A study Orthanc holds already, as another writer left it.
    pub fn hold_study(&self, uid: &str, patient: &str) {
        if let Ok(mut h) = self.held.lock() {
            h.studies
                .insert(orthanc_id(&[patient, uid]), (uid.to_string(), patient.to_string()));
        }
    }

    fn authorized(&self, req: &Request) -> bool {
        let want = format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("{USER}:{}", self.password)
            )
        );
        req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some(want.as_str())
    }

    async fn answer(&self, req: Request) -> Response {
        if self.down.load(Ordering::SeqCst) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if !self.authorized(&req) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let (method, path) = (req.method().clone(), req.uri().path().to_string());
        let query = req.uri().query().unwrap_or_default().to_string();
        let quiet = path == "/tools/lookup" || (method == Method::GET && path.starts_with("/studies/"));
        if !quiet && let Ok(mut h) = self.held.lock() {
            h.log.push((method.to_string(), path.clone()));
        }
        let body = to_bytes(req.into_body(), 1 << 28).await.unwrap_or_default();
        match (method, path.as_str()) {
            (Method::POST, "/tools/lookup") => {
                self.pair().await;
                if let Some(response) = self.lookup_response.lock().ok().and_then(|r| r.clone()) {
                    return json_answer(StatusCode::OK, response);
                }
                let uid = String::from_utf8_lossy(&body).trim().to_string();
                let held = self.held();
                let studies = held
                    .studies
                    .iter()
                    .filter(|(_, (u, _))| *u == uid)
                    .map(|(id, _)| json!({"ID": id, "Path": format!("/studies/{id}"), "Type": "Study"}));
                let instances = held
                    .instance_files
                    .iter()
                    .filter(|(_, (sop, _))| *sop == uid)
                    .map(|(id, _)| json!({"ID": id, "Path": format!("/instances/{id}"), "Type": "Instance"}));
                let found = studies.chain(instances).collect();
                json_answer(StatusCode::OK, Value::Array(found))
            }
            (Method::POST, "/instances") => {
                let delay = self.delay.lock().map(|d| *d).unwrap_or_default();
                tokio::time::sleep(delay).await;
                let stored = self.store(body);
                if let Some(after) = self.after_store.lock().ok().and_then(|mut f| f.take()) {
                    after();
                }
                if self.lose_store_reply_at.load(Ordering::SeqCst) == self.posts() {
                    use axum::body::{Body, Bytes};
                    let interrupted = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "lost store reply");
                    return Response::new(Body::from_stream(futures::stream::iter([
                        Ok::<_, std::io::Error>(Bytes::from_static(b"{")),
                        Err(interrupted),
                    ])));
                }
                stored
            }
            (Method::POST, "/tools/find") => {
                let query: Value = serde_json::from_slice(&body).unwrap_or_default();
                let id = query["ParentStudy"].as_str().unwrap_or_default();
                let held = self.held();
                if query["Level"] == "Instance" {
                    let sop = query["Query"]["SOPInstanceUID"].as_str().unwrap_or_default();
                    let found = held
                        .instance_files
                        .iter()
                        .filter(|(instance, (uid, _))| {
                            uid == sop && held.instances.get(*instance).is_some_and(|(study, _)| study == id)
                        })
                        .map(|(instance, _)| instance)
                        .collect::<Vec<_>>();
                    return json_answer(StatusCode::OK, json!(found));
                }
                let mut series: BTreeMap<&str, usize> = BTreeMap::new();
                for (study, uid) in held.instances.values() {
                    if study == id {
                        *series.entry(uid).or_default() += 1;
                    }
                }
                let since = query["Since"].as_u64().unwrap_or_default() as usize;
                let limit = query["Limit"].as_u64().unwrap_or_default() as usize;
                let out: Vec<Value> = series
                    .into_iter()
                    .skip(since)
                    .take(limit)
                    .map(|(s, count)| {
                        let modality = held.modalities.get(s).cloned().unwrap_or_default();
                        json!({"MainDicomTags": {"SeriesInstanceUID": s, "Modality": modality},
                            "RequestedTags": {"NumberOfSeriesRelatedInstances": count.to_string()}})
                    })
                    .collect();
                json_answer(StatusCode::OK, Value::Array(out))
            }
            (Method::GET, "/studies") => {
                let param = |name: &str| {
                    query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
                        .and_then(|v| v.parse::<usize>().ok())
                };
                let ids: Vec<String> = self
                    .held()
                    .studies
                    .keys()
                    .skip(param("since").unwrap_or_default())
                    .take(param("limit").unwrap_or(usize::MAX))
                    .cloned()
                    .collect();
                json_answer(StatusCode::OK, json!(ids))
            }
            (Method::GET, p) if p.starts_with("/studies/") && p.ends_with("/series") => {
                let id = p.trim_start_matches("/studies/").trim_end_matches("/series");
                let held = self.held();
                let Some((uid, patient)) = held.studies.get(id) else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let series: BTreeSet<&String> = held
                    .instances
                    .values()
                    .filter(|(study, _)| study == id)
                    .map(|(_, s)| s)
                    .collect();
                let out: Vec<Value> = series
                    .into_iter()
                    .map(|s| {
                        json!({"ID": orthanc_id(&[patient, uid, s]), "ParentStudy": id,
                               "MainDicomTags": {"SeriesInstanceUID": s,
                                   "Modality": held.modalities.get(s).cloned().unwrap_or_default()}})
                    })
                    .collect();
                json_answer(StatusCode::OK, Value::Array(out))
            }
            (Method::DELETE, p) if p.starts_with("/studies/") => {
                let id = p.trim_start_matches("/studies/");
                if let Ok(mut h) = self.held.lock() {
                    if h.studies.remove(id).is_none() {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let gone: Vec<String> = h
                        .instances
                        .iter()
                        .filter(|(_, (study, _))| study == id)
                        .map(|(i, _)| i.clone())
                        .collect();
                    for i in gone {
                        h.instances.remove(&i);
                        h.instance_files.remove(&i);
                    }
                }
                json_answer(StatusCode::OK, json!({}))
            }
            (Method::DELETE, p) if p.starts_with("/series/") => {
                let id = p.trim_start_matches("/series/");
                if let Ok(mut h) = self.held.lock() {
                    let studies = h.studies.clone();
                    let gone: Vec<(String, String)> = h
                        .instances
                        .iter()
                        .filter(|(_, (study, series))| {
                            studies
                                .get(study)
                                .is_some_and(|(uid, patient)| orthanc_id(&[patient, uid, series]) == id)
                        })
                        .map(|(i, (study, _))| (i.clone(), study.clone()))
                        .collect();
                    if gone.is_empty() {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    for (i, study) in gone {
                        h.instances.remove(&i);
                        h.instance_files.remove(&i);
                        if !h.instances.values().any(|(s, _)| *s == study) {
                            h.studies.remove(&study);
                        }
                    }
                }
                json_answer(StatusCode::OK, json!({}))
            }
            (Method::GET, p) if p.starts_with("/studies/") => {
                let id = p.trim_start_matches("/studies/");
                match self.held().studies.get(id) {
                    Some((uid, patient)) => json_answer(
                        StatusCode::OK,
                        json!({"ID": id, "MainDicomTags": {"StudyInstanceUID": uid},
                               "PatientMainDicomTags": {"PatientID": patient}}),
                    ),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }
            (Method::GET, p) if p.starts_with("/instances/") && p.ends_with("/file") => {
                let id = p.trim_start_matches("/instances/").trim_end_matches("/file");
                match self.held().instance_files.get(id) {
                    Some((_, data)) => data.clone().into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }
            (Method::DELETE, p) if p.starts_with("/instances/") => {
                let attempted = self.deletes.fetch_add(1, Ordering::SeqCst) + 1;
                if self.fail_delete_at.load(Ordering::SeqCst) == attempted {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                let id = p.trim_start_matches("/instances/");
                if let Ok(mut h) = self.held.lock() {
                    h.instance_files.remove(id);
                    if let Some((study, _)) = h.instances.remove(id)
                        && !h.instances.values().any(|(s, _)| *s == study)
                    {
                        h.studies.remove(&study);
                    }
                }
                json_answer(StatusCode::OK, json!({}))
            }
            (Method::GET, p) if p.starts_with("/dicom-web/studies/") && p.ends_with("/metadata") => {
                if self.metadata_fails.lock().map(|f| *f).unwrap_or(false) {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                let Some((uid, series)) = p
                    .strip_prefix("/dicom-web/studies/")
                    .and_then(|rest| rest.strip_suffix("/metadata"))
                    .and_then(|rest| rest.split_once("/series/"))
                else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let held = self.held();
                if !held
                    .instances
                    .values()
                    .any(|(study, s)| s == series && held.studies.get(study).is_some_and(|(u, _)| u == uid))
                {
                    return StatusCode::NOT_FOUND.into_response();
                }
                json_answer(StatusCode::OK, json!([]))
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    /// With `pair_lookups` set, the first lookup waits up to a second for a second one; then pairing
    /// ends.
    async fn pair(&self) {
        if !self.pair_lookups.load(Ordering::SeqCst) {
            return;
        }
        let (count, notify) = &*self.waiting;
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if count.fetch_add(1, Ordering::SeqCst) + 1 >= 2 {
            self.pair_lookups.store(false, Ordering::SeqCst);
            notify.notify_waiters();
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), notified).await;
        self.pair_lookups.store(false, Ordering::SeqCst);
    }

    fn store(&self, body: axum::body::Bytes) -> Response {
        // Orthanc parses the whole file: one cut in its pixel data is refused
        let Ok(obj) = OpenFileOptions::new().from_reader(body.as_ref()) else {
            return json_answer(StatusCode::BAD_REQUEST, json!({"Message": "Bad file format"}));
        };
        let text = |tag| {
            obj.get(tag)
                .and_then(|e| e.to_str().ok())
                .map(|s| s.trim_end_matches(['\0', ' ']).to_string())
                .unwrap_or_default()
        };
        let (mut patient, mut uid, series, sop) = (
            text(tags::PATIENT_ID),
            text(tags::STUDY_INSTANCE_UID),
            text(tags::SERIES_INSTANCE_UID),
            text(tags::SOP_INSTANCE_UID),
        );
        if self.refused_sop.lock().ok().and_then(|s| s.clone()).as_ref() == Some(&sop) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        if let Ok(m) = self.misread.lock()
            && let Some((from, to)) = m.as_ref()
            && *from == uid
        {
            uid = to.clone();
        }
        let misread_except_series = self.misread_except_series.lock().ok().and_then(|s| s.clone());
        if misread_except_series.as_ref() != Some(&series)
            && let Ok(m) = self.misread_patient.lock()
            && let Some((from, to)) = m.as_ref()
            && *from == patient
        {
            patient = to.clone();
        }
        let modality = text(tags::MODALITY);
        let study = orthanc_id(&[&patient, &uid]);
        let id = orthanc_id(&[&patient, &uid, &series, &sop]);
        let Ok(mut h) = self.held.lock() else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let new = !h.instances.contains_key(&id) || self.overwrite.load(Ordering::SeqCst);
        h.studies.insert(study.clone(), (uid, patient));
        h.modalities.insert(series.clone(), modality);
        h.instances.insert(id.clone(), (study.clone(), series));
        if new {
            h.instance_files.insert(id.clone(), (sop, body));
        }
        let mut reply = json!({"ID": id, "ParentStudy": study, "ParentSeries": "", "ParentPatient": "",
                               "Status": if new { "Success" } else { "AlreadyStored" }});
        match self.store_status.lock().expect("store status").as_ref() {
            Some(Value::Null) => {
                reply.as_object_mut().expect("store reply").shift_remove("Status");
            }
            Some(status) => reply["Status"] = status.clone(),
            None => {}
        }
        json_answer(StatusCode::OK, reply)
    }
}
