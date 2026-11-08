"""Apple Health export.zip: streaming parse, samples, rollups, source priority, clinical records, ECG."""

import io
import tracemalloc
import zipfile
from datetime import UTC, date, datetime, timedelta

import pytest
from sqlalchemy import text

from ownhealth import wearables
from ownhealth.core.records import active_medications
from ownhealth.ingest.apple_health import import_apple_health, rebuild_rollups
from ownhealth.ingest.apple_health.parse import ExportReader
from ownhealth.ingest.apple_health.synthetic import ExportSpec, demo_clinical, iter_xml, write_export
from ownhealth.jobs import PermanentJobError

ZIP = "application/zip"
ALEX_DOB = date(1979, 5, 14)
END = date(2026, 11, 7)


def _zip(tmp_path, spec, name="export.zip"):
    return write_export(tmp_path / name, spec).read_bytes()


def _handmade(records: str, dob="1979-05-14") -> bytes:
    xml = (
        '<?xml version="1.0" encoding="UTF-8"?>\n<HealthData locale="es_ES">\n'
        f' <ExportDate value="2026-11-07 08:00:00 +0100"/>\n <Me HKCharacteristicTypeIdentifierDateOfBirth="{dob}"/>\n'
        f"{records}</HealthData>\n"
    )
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("apple_health_export/export.xml", xml)
    return buf.getvalue()


def _rec(typ, start, end, value, unit="count", device="iPhone"):
    dev = f"&lt;&lt;HKDevice: 0x1&gt;, name:{device}, model:{device}&gt;"
    return (
        f' <Record type="{typ}" sourceName="x" unit="{unit}" device="{dev}" startDate="{start}"'
        f' endDate="{end}" value="{value}"/>\n'
    )


def _expected_steps(spec):
    """Per local date and device: the step totals straight from the generated XML."""
    out: dict = {}
    reader = ExportReader(io.BytesIO("".join(iter_xml(spec)).encode()))
    for s in reader.samples():
        if s.metric == "steps":
            d = (s.t + timedelta(minutes=s.tz_offset_min)).date()
            out.setdefault(s.device, {}).setdefault(d, 0)
            out[s.device][d] += s.v
    return out


def test_export_imports_samples_clinical_records_and_ecg(conn, family, blobs, store_doc, tmp_path):
    p = family.profiles["alex"]
    spec = ExportSpec(days=14, end=END, birth_date=ALEX_DOB, clinical=demo_clinical(), ecg_count=2)
    doc = store_doc(conn, family, "alex", _zip(tmp_path, spec), "export.zip", ZIP)
    s = import_apple_health(conn, blobs, document_id=doc)
    n = conn.execute(text("select count(*) from wearable_sample where profile_id = :p"), {"p": p}).scalar()
    # per day: 16 Watch + 14 iPhone steps, 4 sleep stages (one Awake, skipped), in bed, resting HR, HRV; VO2max weekly
    assert n == 14 * (16 + 14 + 3 + 1 + 1 + 1) + 2
    assert s.accepted == n + 2 and s.queued == 0  # + the clinical lab result and prescription
    devices = conn.execute(
        text("select device from wearable_source where profile_id = :p order by device"), {"p": p}
    ).scalars()
    assert list(devices) == ["Apple Watch", "iPhone"]
    # the samples are citable: one apple_health src names this export
    cites = conn.execute(
        text("select document_id from src where profile_id = :p and kind = 'apple_health'"), {"p": p}
    ).scalars()
    assert list(cites) == [doc]
    row = (
        conn.execute(
            text(
                "select t, tz_offset_min from wearable_sample where profile_id = :p and metric = 'hr_resting'"
                " order by t"
            ),
            {"p": p},
        )
        .mappings()
        .first()
    )
    assert row["t"] == datetime(2026, 10, 25, 6, 5, tzinfo=UTC) and row["tz_offset_min"] == 60  # 07:05 +0100
    # clinical records follow the FHIR path, with Apple as the label
    [med] = active_medications(conn, p)
    assert med["name"] == "Cetirizine 10 mg tablet" and med["accepted_by"] == "import"
    ferritin = conn.execute(
        text(
            "select o.loinc, s.kind, s.label from observation o join src s on s.id = o.src_id"
            " where o.profile_id = :p and o.loinc = '2276-4'"
        ),
        {"p": p},
    ).one()
    assert ferritin.kind == "fhir_import" and ferritin.label.startswith("Apple Health")
    ecg = conn.execute(
        text("select doc_type, mime, collected_at from document where profile_id = :p and doc_type = 'ecg'"), {"p": p}
    ).all()
    assert len(ecg) == 2 and all(e.mime == "text/csv" and e.collected_at is not None for e in ecg)
    flags = conn.execute(text("select flags from document where id = :d"), {"d": doc}).scalar()
    assert flags["apple_health"]["samples"] == n and flags["apple_health"]["not_imported"] == {
        "skipped:sleep_awake": 14
    }


def test_priority_takes_the_watch_and_never_sums_watch_and_iphone(conn, family, blobs, store_doc, tmp_path):
    p = family.profiles["alex"]
    spec = ExportSpec(days=10, end=END, birth_date=ALEX_DOB)
    import_apple_health(conn, blobs, document_id=store_doc(conn, family, "alex", _zip(tmp_path, spec), "e.zip", ZIP))
    want = _expected_steps(spec)
    since, until = wearables.day_start(END - timedelta(days=9)), wearables.day_start(END)
    pts = wearables.series(conn, p, "steps", since=since, until=until)
    assert len(pts) == 10
    assert {pt["t"].date(): pt["value"] for pt in pts} == want["Apple Watch"]
    assert {pt["device"] for pt in pts} == {"Apple Watch"}
    # the person prefers the phone: same days, the phone's totals
    ids = dict(conn.execute(text("select device, id from wearable_source where profile_id = :p"), {"p": p}).all())
    wearables.set_priority(conn, p, "steps", [ids["iPhone"], ids["Apple Watch"]])
    pts = wearables.series(conn, p, "steps", since=since, until=until)
    assert {pt["t"].date(): pt["value"] for pt in pts} == want["iPhone"]
    # a day the Watch was not worn falls back to the next source instead of going empty
    wearables.set_priority(conn, p, "steps", [ids["Apple Watch"], ids["iPhone"]])
    gap = END - timedelta(days=3)
    conn.execute(
        text(
            "delete from wearable_sample where profile_id = :p and source_id = :s and metric = 'steps'"
            " and t >= :a and t < :b"
        ),
        {"p": p, "s": ids["Apple Watch"], "a": wearables.day_start(gap), "b": wearables.day_start(gap + timedelta(1))},
    )
    rebuild_rollups(conn, profile_id=p, metric="steps")
    by_day = {pt["t"].date(): pt for pt in wearables.series(conn, p, "steps", since=since, until=until)}
    assert by_day[gap]["device"] == "iPhone" and by_day[gap]["value"] == want["iPhone"][gap]
    assert by_day[END]["device"] == "Apple Watch"


def test_rollup_statistics_local_day_and_sleep_on_waking(conn, family, blobs, store_doc):
    p = family.profiles["alex"]
    hr = "".join(
        _rec(
            "HKQuantityTypeIdentifierHeartRate",
            f"2026-10-02 10:{m:02d}:00 +0200",
            f"2026-10-02 10:{m:02d}:00 +0200",
            v,
            "count/min",
            "Apple Watch",
        )
        for m, v in zip(range(0, 50, 5), range(60, 70), strict=True)
    )
    late = _rec("HKQuantityTypeIdentifierStepCount", "2026-10-02 00:30:00 +0200", "2026-10-02 00:40:00 +0200", 120)
    sleep = "".join(
        _rec(
            "HKCategoryTypeIdentifierSleepAnalysis", a, b, f"HKCategoryValueSleepAnalysis{v}", "", "Apple Watch"
        ).replace(' unit=""', "")
        for a, b, v in [
            ("2026-10-01 23:00:00 +0200", "2026-10-02 03:00:00 +0200", "AsleepCore"),
            ("2026-10-02 03:00:00 +0200", "2026-10-02 03:20:00 +0200", "Awake"),
            ("2026-10-02 03:20:00 +0200", "2026-10-02 06:50:00 +0200", "AsleepREM"),
        ]
    )
    doc = store_doc(conn, family, "alex", _handmade(hr + late + sleep), "e.zip", ZIP)
    import_apple_health(conn, blobs, document_id=doc)
    day = datetime(2026, 10, 2, tzinfo=UTC)
    [h] = wearables.series(conn, p, "hr", since=day, until=day + timedelta(days=1), resolution="hour")
    assert h["t"] == datetime(2026, 10, 2, 8, tzinfo=UTC)  # 10:00 +0200
    assert (h["n"], h["min"], h["max"], h["mean"], h["median"]) == (10, 60, 69, 64.5, 64.5)
    assert (round(h["p10"], 2), round(h["p90"], 2)) == (60.9, 68.1)
    assert h["coverage"] == pytest.approx(10 / 12)  # 10 of 12 five-minute slots
    [d] = wearables.series(conn, p, "hr", since=day, until=day)
    assert d["coverage"] == pytest.approx(1 / 24)
    # 00:30 local on 2 Oct is 22:30 UTC on 1 Oct: it counts on the local date
    [steps] = wearables.series(conn, p, "steps", since=day - timedelta(days=1), until=day)
    assert steps["t"] == day and steps["value"] == 120
    # the night that ends on 2 Oct: 240 + 210 minutes asleep, the Awake stage left out, on the waking date
    [night] = wearables.series(conn, p, "sleep_asleep", since=day - timedelta(days=1), until=day)
    assert night["t"] == day and night["value"] == 450 and night["coverage"] == 1.0


def _sleep(start, end, stage):
    rec = _rec(
        "HKCategoryTypeIdentifierSleepAnalysis", start, end, f"HKCategoryValueSleepAnalysis{stage}", "", "Apple Watch"
    )
    return rec.replace(' unit=""', "")


def test_a_night_with_stages_before_midnight_counts_whole_on_the_waking_date(conn, family, blobs, store_doc):
    """A Watch writes many short stages; those that end before 00:00 still belong to the night that
    ends the next morning. Two nights more than 2 h apart stay two nights."""
    p = family.profiles["alex"]
    nights = (
        _sleep("2026-11-05 22:45:00 +0100", "2026-11-05 23:40:00 +0100", "AsleepCore")
        + _sleep("2026-11-05 23:40:00 +0100", "2026-11-06 00:30:00 +0100", "AsleepDeep")
        + _sleep("2026-11-06 00:30:00 +0100", "2026-11-06 06:45:00 +0100", "AsleepREM")
        + _sleep("2026-11-06 22:30:00 +0100", "2026-11-06 23:50:00 +0100", "AsleepCore")
        + _sleep("2026-11-06 23:50:00 +0100", "2026-11-07 06:30:00 +0100", "AsleepREM")
    )
    doc = store_doc(conn, family, "alex", _handmade(nights), "e.zip", ZIP)
    import_apple_health(conn, blobs, document_id=doc)
    pts = wearables.series(
        conn, p, "sleep_asleep", since=datetime(2026, 11, 1, tzinfo=UTC), until=datetime(2026, 11, 8, tzinfo=UTC)
    )
    assert {x["t"].date().isoformat(): x["value"] for x in pts} == {"2026-11-06": 480.0, "2026-11-07": 480.0}


def test_reimport_is_idempotent_and_wrong_profile_is_refused(conn, family, blobs, store_doc, tmp_path):
    p = family.profiles["alex"]
    spec = ExportSpec(days=5, end=END, birth_date=ALEX_DOB)
    data = _zip(tmp_path, spec)
    doc = store_doc(conn, family, "alex", data, "e.zip", ZIP)
    first = import_apple_health(conn, blobs, document_id=doc)
    assert import_apple_health(conn, blobs, document_id=doc).as_dict() == first.as_dict()
    # a later export repeats the old samples: none counted twice
    spec2 = ExportSpec(days=6, end=END + timedelta(days=1), birth_date=ALEX_DOB)
    later = import_apple_health(
        conn, blobs, document_id=store_doc(conn, family, "alex", _zip(tmp_path, spec2, "b.zip"), "b.zip", ZIP)
    )
    assert later.reasons["duplicate"] > 0
    total = conn.execute(text("select count(*) from wearable_sample where profile_id = :p"), {"p": p}).scalar()
    assert total == first.accepted + later.accepted
    # Alex's export uploaded to Sam's profile: the birth dates differ, nothing is written
    sam = family.profiles["sam"]
    doc_sam = store_doc(conn, family, "sam", data, "e.zip", ZIP)
    with pytest.raises(PermanentJobError, match="birth date"):
        import_apple_health(conn, blobs, document_id=doc_sam)
    n = conn.execute(text("select count(*) from wearable_sample where profile_id = :p"), {"p": sam}).scalar()
    assert n == 0  # and the job's transaction rolls back the rest


def test_parser_memory_stays_flat_as_the_file_grows():
    """Peak Python memory of the streaming parser: ~10x more records must not cost ~10x more memory."""

    def peak(days):
        spec = ExportSpec(days=days, hr_every_min=10, end=END, birth_date=ALEX_DOB)

        class Stream(io.RawIOBase):  # feed the XML as it is generated, never as one string
            def __init__(self):
                self.it, self.buf = (c.encode() for c in iter_xml(spec)), b""

            def readable(self):
                return True

            def readinto(self, b):
                while len(self.buf) < len(b):
                    try:
                        self.buf += next(self.it)
                    except StopIteration:
                        break
                n = min(len(b), len(self.buf))
                b[:n], self.buf = self.buf[:n], self.buf[n:]
                return n

        tracemalloc.start()
        count = sum(1 for _ in ExportReader(io.BufferedReader(Stream())).samples())
        _, top = tracemalloc.get_traced_memory()
        tracemalloc.stop()
        return count, top

    small_n, small = peak(20)
    big_n, big = peak(200)
    assert big_n > 9 * small_n
    assert big < small * 1.5 + 512 * 1024, (small, big)


def test_upload_dispatch_runs_the_job(committed_family, engine, settings, blobs, tmp_path):
    from ownhealth.core.documents import StoredFile, dispatch_upload
    from ownhealth.jobs import JobContext, drain, load_handlers

    fam = committed_family
    data = _zip(tmp_path, ExportSpec(days=3, end=END, birth_date=date(1981, 2, 2), first_name="Sam"))
    sha = blobs.put_bytes(data)
    with engine.begin() as c:
        out = dispatch_upload(
            c,
            blobs,
            household_id=fam.household_id,
            profile_id=fam.profiles["sam"],
            files=[StoredFile("export.zip", sha, len(data))],
        )
    [item] = out["items"]
    assert item["kind"] == "apple_health_zip"
    load_handlers()
    drain(JobContext(engine, settings, blobs), kinds=["import.apple_health"])
    with engine.connect() as c:
        job = c.execute(text("select state, result from job where id = :j"), {"j": item["job_id"]}).one()
        assert job.state == "done" and job.result["accepted"] > 0
        n = c.execute(
            text("select count(*) from wearable_sample where profile_id = :p"), {"p": fam.profiles["sam"]}
        ).scalar()
        assert n == job.result["accepted"]


def _bomb(member: str, size: int) -> bytes:
    """A small zip holding one huge, highly compressible member (and a valid export.xml)."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("apple_health_export/export.xml", _handmade_xml())
        with z.open(member, "w", force_zip64=True) as out:
            chunk = b"0" * (1024 * 1024)
            for _ in range(size // len(chunk)):
                out.write(chunk)
    return buf.getvalue()


def _handmade_xml() -> str:
    with zipfile.ZipFile(io.BytesIO(_handmade(""))) as z:
        return z.read("apple_health_export/export.xml").decode()


@pytest.mark.parametrize(
    "member",
    ["apple_health_export/electrocardiograms/ecg_2026-11-01.csv", "apple_health_export/clinical-records/Big.json"],
)
def test_an_inflating_member_stops_the_import_without_being_read(conn, family, blobs, store_doc, member):
    doc = store_doc(conn, family, "alex", _bomb(member, 50 * 1024 * 1024), "bomb.zip", ZIP)
    tracemalloc.start()
    try:
        with pytest.raises(PermanentJobError, match="over 20 MB|over 5 MB"):
            import_apple_health(conn, blobs, document_id=doc)
        peak = tracemalloc.get_traced_memory()[1]
    finally:
        tracemalloc.stop()
    assert peak < 30 * 1024 * 1024  # the 50 MB member was never read whole
