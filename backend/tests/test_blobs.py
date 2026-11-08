import hashlib
import io

import pytest

from ownhealth.blobs import BlobNotFound, BlobStore


def test_put_dedupes_and_uses_sharded_path(tmp_path):
    store = BlobStore(tmp_path)
    data = b"synthetic lab report"
    h1 = store.put_bytes(data)
    h2, size = store.put_stream(io.BytesIO(data))
    assert h1 == h2 == hashlib.sha256(data).hexdigest()
    assert size == len(data)
    p = store.path(h1)
    assert p == tmp_path / "sha256" / h1[:2] / h1[2:4] / h1
    assert len(list((tmp_path / "sha256").rglob("*"))) == 3  # two dirs and one file: stored once
    assert store.get_bytes(bytes.fromhex(h1)) == data
    assert list((tmp_path / "tmp").iterdir()) == []


def test_missing_and_bad_hashes(tmp_path):
    store = BlobStore(tmp_path)
    with pytest.raises(BlobNotFound):
        store.get_bytes("0" * 64)
    with pytest.raises(ValueError):
        store.path("../../etc/passwd")
    assert store.delete(store.put_bytes(b"x")) is True
