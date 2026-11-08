import io
import zipfile

from ownhealth.core.documents import sniff


def test_a_corrupt_zip_is_unknown_not_a_crash(tmp_path):
    good = io.BytesIO()
    with zipfile.ZipFile(good, "w") as z:
        z.writestr("apple_health_export/export.xml", "<HealthData/>")
    ok = tmp_path / "ok.zip"
    ok.write_bytes(good.getvalue())
    assert sniff("export.zip", ok.read_bytes()[:16], ok) == "apple_health_zip"
    bad = tmp_path / "bad.zip"
    bad.write_bytes(good.getvalue()[:40])  # local header present, central directory gone
    assert sniff("export.zip", bad.read_bytes()[:16], bad) == "unknown"


def test_a_src_id_with_a_trailing_newline_is_refused():
    from ownhealth.core.ids import is_src_id

    assert is_src_id("src_abc") and not is_src_id("src_abc\n")
