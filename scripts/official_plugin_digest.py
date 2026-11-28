#!/usr/bin/env python3
"""Print the three values an official-plugin catalog entry pins for a package.

    python scripts/official_plugin_digest.py path/to/plugin.zip
    python scripts/official_plugin_digest.py path/to/plugin-folder

Prints JSON with ``sha256`` (of the zip; folders have none), ``size`` (bytes of
the zip) and ``tree_sha256`` (the digest of the files the package unpacks to).
Paste them into ``registry/official-plugins/<id>.json`` under ``current`` or
``previous``. See REGISTRY_CURATION_REFERENCE.md, "Official plugins".

The tree digest must equal ``tree_digest`` / ``zip_tree_digest`` in
``crates/agora-core/src/plugins/official.rs`` byte for byte: the app checks an
installed plugin's folder against this value, as pinned in the signed registry.
A shared test vector in ``compiler/test_compile.py`` and
``crates/agora-core/src/plugins/official.rs`` keeps the two in step.

The digest covers, for every file and every folder above a file: its path with
``/`` separators, whether it is a file or a folder, and (for a file) the SHA-256
of its contents. Entries are sorted by path, and each contributes the line
``kind NUL path NUL hash LF`` to a SHA-256. Empty folders do not count, because
unpacking a package never creates one.
"""

from __future__ import annotations

import hashlib
import json
import sys
import zipfile
from pathlib import Path

# The limits the app enforces on a package (plugins/install.rs).
MAX_ENTRIES = 2_000
MAX_ENTRY_BYTES = 16 * 1024 * 1024
MAX_TOTAL_BYTES = 64 * 1024 * 1024


def _normalise(name: str) -> str:
    """The path a package entry unpacks to, or SystemExit if the app would refuse it."""
    if not name or "\0" in name:
        raise SystemExit(f"unusable entry name {name!r}")
    unified = name.replace("\\", "/")
    if unified.startswith("/") or (len(unified) >= 2 and unified[1] == ":"):
        raise SystemExit(f"{name!r} is an absolute path")
    parts = []
    for part in unified.split("/"):
        if part == "..":
            raise SystemExit(f"{name!r} climbs out of the package")
        if ":" in part:
            raise SystemExit(f"{name!r} has a drive-style component")
        if part in ("", "."):
            continue
        parts.append(part)
    if not parts:
        raise SystemExit(f"unusable entry name {name!r}")
    return "/".join(parts)


def tree_digest(files: dict[str, bytes]) -> str:
    """Digest of ``{relative path: contents}``; folders are the ancestors of the files."""
    entries: list[tuple[str, str, str]] = []
    folders: set[str] = set()
    for path, content in files.items():
        parts = path.split("/")
        for depth in range(1, len(parts)):
            folders.add("/".join(parts[:depth]))
        entries.append((path, "file", hashlib.sha256(content).hexdigest()))
    entries.extend((folder, "dir", "") for folder in folders)
    entries.sort()
    digest = hashlib.sha256()
    for path, kind, content_hash in entries:
        digest.update(f"{kind}\0{path}\0{content_hash}\n".encode("utf-8"))
    return digest.hexdigest()


def files_of_zip(data: bytes) -> dict[str, bytes]:
    import io

    archive = zipfile.ZipFile(io.BytesIO(data))
    infos = archive.infolist()
    if len(infos) > MAX_ENTRIES:
        raise SystemExit(f"the package holds {len(infos)} entries; the limit is {MAX_ENTRIES}")
    files: dict[str, bytes] = {}
    total = 0
    for info in infos:
        if info.is_dir():
            continue
        content = archive.read(info)
        if len(content) > MAX_ENTRY_BYTES:
            raise SystemExit(f"{info.filename!r} is larger than {MAX_ENTRY_BYTES} bytes")
        total += len(content)
        if total > MAX_TOTAL_BYTES:
            raise SystemExit(f"the package expands past {MAX_TOTAL_BYTES} bytes")
        # A later entry with the same path replaces an earlier one, as unpacking does.
        files[_normalise(info.filename)] = content
    return files


def files_of_folder(root: Path) -> dict[str, bytes]:
    return {
        path.relative_to(root).as_posix(): path.read_bytes()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def describe(path: Path) -> dict[str, object]:
    if path.is_dir():
        return {"tree_sha256": tree_digest(files_of_folder(path))}
    data = path.read_bytes()
    return {
        "sha256": hashlib.sha256(data).hexdigest(),
        "size": len(data),
        "tree_sha256": tree_digest(files_of_zip(data)),
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    path = Path(argv[1])
    if not path.exists():
        print(f"{path} does not exist", file=sys.stderr)
        return 1
    print(json.dumps(describe(path), indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
