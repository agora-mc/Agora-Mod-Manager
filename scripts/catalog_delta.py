#!/usr/bin/env python3
"""Catalog deltas (MASTER_SPEC §33).

A nightly catalog differs from the previous one by a few KB, but the bundle is
megabytes. Beside ``registry.bundle`` each release therefore carries:

* ``registry.header``: the bundle's signed prefix (magic, header length,
  header, signature) with no database;
* ``registry.delta-<sha16>.zst``: a zstd frame that rebuilds this release's
  database using an earlier database (whose SHA-256 starts with ``sha16``) as a
  raw-content dictionary, the same thing ``zstd --patch-from`` writes.

The client applies the delta for the catalog it already verified, appends the
result to the signed prefix and verifies that bundle exactly as it would a full
download. A missing or broken delta only costs the full download.
"""

from __future__ import annotations

import hashlib
from pathlib import Path

MAGIC = b"AGORA-CATALOG-V1"
SIGNATURE_LEN = 64
HEADER_ASSET = "registry.header"
DELTA_PREFIX = "registry.delta-"
DELTA_SUFFIX = ".zst"
DELTA_LEVEL = 19


def split_bundle(bundle: bytes) -> tuple[bytes, bytes]:
    """Return ``(signed_prefix, database)`` of a catalog bundle."""
    if len(bundle) < len(MAGIC) + 4 + SIGNATURE_LEN or not bundle.startswith(MAGIC):
        raise ValueError("not a catalog bundle")
    header_len = int.from_bytes(bundle[len(MAGIC):len(MAGIC) + 4], "little")
    payload_start = len(MAGIC) + 4 + header_len + SIGNATURE_LEN
    if header_len == 0 or payload_start > len(bundle):
        raise ValueError("truncated catalog bundle")
    return bundle[:payload_start], bundle[payload_start:]


def delta_asset_name(reference_db: bytes) -> str:
    return f"{DELTA_PREFIX}{hashlib.sha256(reference_db).hexdigest()[:16]}{DELTA_SUFFIX}"


def make_delta(reference_db: bytes, new_db: bytes) -> bytes:
    import zstandard

    dictionary = zstandard.ZstdCompressionDict(
        reference_db, dict_type=zstandard.DICT_TYPE_RAWCONTENT
    )
    compressor = zstandard.ZstdCompressor(
        level=DELTA_LEVEL, dict_data=dictionary, write_content_size=True
    )
    return compressor.compress(new_db)


def apply_delta(reference_db: bytes, delta: bytes) -> bytes:
    import zstandard

    dictionary = zstandard.ZstdCompressionDict(
        reference_db, dict_type=zstandard.DICT_TYPE_RAWCONTENT
    )
    return zstandard.ZstdDecompressor(dict_data=dictionary).decompress(delta)


def write_delta_assets(
    new_bundle: bytes, previous_bundles: list[bytes], out_dir: Path
) -> list[Path]:
    """Write ``registry.header`` and one delta per distinct earlier database.

    Each delta is checked by applying it before it is kept, so a release never
    publishes a delta that rebuilds the wrong bytes.
    """
    prefix, new_db = split_bundle(new_bundle)
    out_dir.mkdir(parents=True, exist_ok=True)
    header_path = out_dir / HEADER_ASSET
    header_path.write_bytes(prefix)
    written = [header_path]
    seen: set[str] = set()
    for old_bundle in previous_bundles:
        try:
            _, old_db = split_bundle(old_bundle)
        except ValueError:
            continue
        name = delta_asset_name(old_db)
        if old_db == new_db or name in seen:
            continue
        seen.add(name)
        delta = make_delta(old_db, new_db)
        if apply_delta(old_db, delta) != new_db:
            raise RuntimeError(f"{name} does not rebuild the new catalog")
        path = out_dir / name
        path.write_bytes(delta)
        written.append(path)
    return written


def is_delta_asset(name: str) -> bool:
    return name == HEADER_ASSET or (name.startswith(DELTA_PREFIX) and name.endswith(DELTA_SUFFIX))
