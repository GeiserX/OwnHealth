"""Content-addressed immutable blob store on disk: blobs/sha256/ab/cd/<full sha256 hex>.

The key is the hash of the bytes, so the same file stored twice is kept once. A blob is never
modified after it lands. Access control lives on the rows that reference a hash (document,
genome_dataset); a hash on its own grants nothing.
"""

from __future__ import annotations

import hashlib
import os
import secrets
from collections.abc import Iterator
from pathlib import Path
from typing import BinaryIO

_CHUNK = 1024 * 1024


class BlobNotFound(FileNotFoundError):
    pass


def _hex(sha256: str | bytes) -> str:
    value = sha256.hex() if isinstance(sha256, bytes | bytearray | memoryview) else sha256.lower()
    if len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
        raise ValueError("not a sha256 hex digest")
    return value


class BlobStore:
    """put / get / exists / delete over a directory. Hashes are hex strings; bytes are accepted too."""

    def __init__(self, root: Path | str):
        self.root = Path(root)
        (self.root / "tmp").mkdir(parents=True, exist_ok=True)
        (self.root / "sha256").mkdir(parents=True, exist_ok=True)

    def path(self, sha256: str | bytes) -> Path:
        h = _hex(sha256)
        return self.root / "sha256" / h[0:2] / h[2:4] / h

    def put_stream(self, stream: BinaryIO) -> tuple[str, int]:
        """Stream into the store. Returns (sha256 hex, size). Same bytes twice: one file."""
        tmp = self.root / "tmp" / secrets.token_hex(16)
        digest = hashlib.sha256()
        size = 0
        try:
            with open(tmp, "wb") as out:
                while chunk := stream.read(_CHUNK):
                    digest.update(chunk)
                    out.write(chunk)
                    size += len(chunk)
                out.flush()
                os.fsync(out.fileno())
            h = digest.hexdigest()
            target = self.path(h)
            if target.exists():
                tmp.unlink()
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                os.replace(tmp, target)
                os.chmod(target, 0o440)
            return h, size
        except BaseException:
            tmp.unlink(missing_ok=True)
            raise

    def put_bytes(self, data: bytes) -> str:
        import io

        h, _ = self.put_stream(io.BytesIO(data))
        return h

    def put_file(self, path: Path | str) -> tuple[str, int]:
        with open(path, "rb") as f:
            return self.put_stream(f)

    def exists(self, sha256: str | bytes) -> bool:
        return self.path(sha256).exists()

    def open(self, sha256: str | bytes) -> BinaryIO:
        p = self.path(sha256)
        if not p.exists():
            raise BlobNotFound(_hex(sha256))
        return open(p, "rb")

    def get_bytes(self, sha256: str | bytes) -> bytes:
        with self.open(sha256) as f:
            return f.read()

    def iter_chunks(self, sha256: str | bytes) -> Iterator[bytes]:
        with self.open(sha256) as f:
            while chunk := f.read(_CHUNK):
                yield chunk

    def delete(self, sha256: str | bytes) -> bool:
        """Only for a hard purge or the GC job, after checking no row references the hash."""
        p = self.path(sha256)
        if not p.exists():
            return False
        os.chmod(p, 0o640)
        p.unlink()
        return True


def sha256_hex_to_bytes(h: str) -> bytes:
    return bytes.fromhex(_hex(h))
