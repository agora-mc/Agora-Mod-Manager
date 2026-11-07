#!/usr/bin/env python3
"""Build a reproducible package zip for an official plugin.

An official plugin lives at ``packages/official/<name>/`` with an
``agora-plugin.json`` at its root. The registry compiler trusts the SHA-256 and
size recorded in ``registry/official-plugins/<plugin-id>.json``, so the bytes
attached to a GitHub release must be exactly the bytes hashed here. Every zip
entry is sorted, stored uncompressed, and given a fixed timestamp and mode, so
two builds of the same checkout produce the same file even when the files'
mtimes differ.

Usage::

    python scripts/build_official_plugin.py skyrim-se
    python scripts/build_official_plugin.py skyrim-se --out dist/official-plugins
    python scripts/build_official_plugin.py skyrim-se --update-registry
    python scripts/build_official_plugin.py skyrim-se --verify-registry

``--verify-registry`` builds the zip again and refuses when the checked-in
registry entry does not describe those bytes, which is what keeps the entry
from becoming a hand-typed value that drifts from the release asset.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import stat
import sys
import zipfile
from pathlib import Path
from typing import Any

import official_plugin_digest

REPO_ROOT = Path(__file__).resolve().parent.parent
OFFICIAL_ROOT = REPO_ROOT / "packages" / "official"
REGISTRY_ROOT = REPO_ROOT / "registry" / "official-plugins"
DEFAULT_OUT = REPO_ROOT / "dist" / "official-plugins"
DEFAULT_REPO = "agora-mc/Agora-Mod-Manager"
MIN_AGORA_VERSION = "0.1.0"
NAME_RE = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
# 1980-01-01 00:00:00 is the earliest timestamp a ZIP can represent.
FIXED_DATE = (1980, 1, 1, 0, 0, 0)
FIXED_MODE = stat.S_IFREG | 0o644


def package_dir(name_or_path: str) -> Path:
    """Resolve a package name (``skyrim-se``) or a path to its folder."""
    candidate = Path(name_or_path)
    if candidate.is_dir():
        return candidate.resolve()
    if NAME_RE.fullmatch(name_or_path):
        return OFFICIAL_ROOT / name_or_path
    raise SystemExit(f"{name_or_path!r} is not a package name or folder")


def read_json(path: Path) -> dict[str, Any]:
    try:
        with path.open("r", encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"cannot read {path}: {error}") from error
    if not isinstance(data, dict):
        raise SystemExit(f"{path}: expected a JSON object")
    return data


def plugin_manifest(folder: Path) -> dict[str, Any]:
    manifest = read_json(folder / "agora-plugin.json")
    for key in ("id", "name", "version"):
        value = manifest.get(key)
        if not isinstance(value, str) or not value.strip():
            raise SystemExit(f"{folder}/agora-plugin.json: {key} is required")
    if not NAME_RE.fullmatch(folder.name):
        raise SystemExit(f"{folder}: the folder name must be lowercase letters, digits and hyphens")
    return manifest


def game_ids(folder: Path, manifest: dict[str, Any]) -> list[str]:
    contributions = manifest.get("contributions") or {}
    if not isinstance(contributions, dict):
        raise SystemExit(f"{folder}: contributions must be an object")
    packages = contributions.get("gamePackages") or []
    if not isinstance(packages, list) or not packages:
        raise SystemExit(f"{folder}: the plugin contributes no game package")
    ids: list[str] = []
    for contribution in packages:
        if not isinstance(contribution, dict) or not isinstance(contribution.get("path"), str):
            raise SystemExit(f"{folder}: every gamePackages entry needs a path")
        path = (folder / contribution["path"]).resolve()
        try:
            path.relative_to(folder.resolve())
        except ValueError as error:
            raise SystemExit(f"{folder}: game package {path} leaves the plugin") from error
        package = read_json(path)
        games = package.get("games")
        if not isinstance(games, list) or not games:
            raise SystemExit(f"{path}: the game package defines no games")
        for game in games:
            if not isinstance(game, dict) or not isinstance(game.get("id"), str):
                raise SystemExit(f"{path}: every game needs an id")
            if game["id"] in ids:
                raise SystemExit(f"{folder}: game id {game['id']!r} is listed twice")
            ids.append(game["id"])
    return ids


def release_tag(name: str, version: str) -> str:
    return f"official-{name}-v{version}"


def zip_name(name: str, version: str) -> str:
    return f"{name}-{version}.zip"


def package_files(folder: Path) -> list[Path]:
    files: list[Path] = []
    for path in sorted(folder.rglob("*"), key=lambda p: p.relative_to(folder).as_posix()):
        if path.is_symlink():
            raise SystemExit(f"{path}: links are not allowed in an official plugin")
        if path.is_file():
            files.append(path)
    return files


def build_zip(folder: Path, out_dir: Path, version: str) -> Path:
    """Write ``<folder name>-<version>.zip`` and return its path."""
    out_dir.mkdir(parents=True, exist_ok=True)
    destination = out_dir / zip_name(folder.name, version)
    temporary = destination.with_suffix(".zip.tmp")
    with zipfile.ZipFile(temporary, "w", allowZip64=True) as archive:
        for path in package_files(folder):
            relative = path.relative_to(folder).as_posix()
            info = zipfile.ZipInfo(relative, date_time=FIXED_DATE)
            # Stored, not deflated: zlib output can differ between Python
            # builds, and the registry pins one hash for every platform.
            info.compress_type = zipfile.ZIP_STORED
            info.create_system = 3
            info.external_attr = FIXED_MODE << 16
            info.internal_attr = 0
            info.extra = b""
            archive.writestr(info, path.read_bytes())
    temporary.replace(destination)
    return destination


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tree_sha256(path: Path) -> str:
    """The tree digest the app checks an installed folder against.

    Computed from the built zip's own bytes so the entry and the release asset
    describe exactly the same files. Shared with the app's tree digest through
    a common test vector (compiler/test_compile.py and plugins/official.rs).
    """
    files = official_plugin_digest.files_of_zip(path.read_bytes())
    return official_plugin_digest.tree_digest(files)


def describe(folder: Path, out_dir: Path, repo: str = DEFAULT_REPO) -> dict[str, Any]:
    manifest = plugin_manifest(folder)
    version = manifest["version"]
    archive = build_zip(folder, out_dir, version)
    tag = release_tag(folder.name, version)
    return {
        "name": folder.name,
        "id": manifest["id"],
        "version": version,
        "tag": tag,
        "zip": archive,
        "sha256": sha256(archive),
        "tree_sha256": tree_sha256(archive),
        "size": archive.stat().st_size,
        "package_url": (
            f"https://github.com/{repo}/releases/download/{tag}/{archive.name}"
        ),
    }


def registry_entry(built: dict[str, Any], folder: Path) -> dict[str, Any]:
    manifest = plugin_manifest(folder)
    entry: dict[str, Any] = {
        "id": built["id"],
        "name": manifest["name"].strip(),
    }
    description = manifest.get("description")
    if isinstance(description, str) and description.strip():
        entry["description"] = description.strip()
    entry.update(
        {
            "game_ids": game_ids(folder, manifest),
            "min_agora_version": MIN_AGORA_VERSION,
            "current": {
                "version": built["version"],
                "package_url": built["package_url"],
                "sha256": built["sha256"],
                "tree_sha256": built["tree_sha256"],
                "size": built["size"],
            },
        }
    )
    return entry


def registry_path(plugin_id: str) -> Path:
    return REGISTRY_ROOT / f"{plugin_id}.json"


def render(entry: dict[str, Any]) -> str:
    return json.dumps(entry, indent=2, ensure_ascii=False) + "\n"


def verify_registry(built: dict[str, Any], folder: Path) -> dict[str, Any]:
    entry = registry_entry(built, folder)
    path = registry_path(entry["id"])
    if not path.is_file():
        raise SystemExit(f"{path} is missing; run with --update-registry")
    existing = read_json(path)
    for key in ("id", "name", "description", "game_ids", "min_agora_version"):
        if existing.get(key) != entry.get(key):
            raise SystemExit(
                f"{path}: {key} is {existing.get(key)!r}, but the package says "
                f"{entry.get(key)!r}"
            )
    current = existing.get("current")
    if not isinstance(current, dict):
        raise SystemExit(f"{path}: current is missing")
    for key in ("version", "package_url", "sha256", "tree_sha256", "size"):
        if current.get(key) != entry["current"][key]:
            raise SystemExit(
                f"{path}: current.{key} is {current.get(key)!r}, but the built zip "
                f"says {entry['current'][key]!r}; run with --update-registry"
            )
    return entry


def update_registry(built: dict[str, Any], folder: Path) -> Path:
    entry = registry_entry(built, folder)
    path = registry_path(entry["id"])
    existing = read_json(path) if path.is_file() else None
    if isinstance(existing, dict) and isinstance(existing.get("current"), dict):
        if existing["current"].get("version") == entry["current"]["version"]:
            if "previous" in existing:
                entry["previous"] = existing["previous"]
        else:
            entry["previous"] = existing["current"]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(render(entry), encoding="utf-8")
    return path


def summary(built: dict[str, Any]) -> dict[str, Any]:
    return {
        "name": built["name"],
        "id": built["id"],
        "version": built["version"],
        "tag": built["tag"],
        "zip": str(built["zip"]),
        "sha256": built["sha256"],
        "tree_sha256": built["tree_sha256"],
        "size": built["size"],
        "package_url": built["package_url"],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", help="package name under packages/official, or its folder")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help="where to write the zip")
    parser.add_argument("--repo", default=DEFAULT_REPO, help="GitHub repository for the asset URL")
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--update-registry", action="store_true")
    action.add_argument("--verify-registry", action="store_true")
    args = parser.parse_args(argv)

    folder = package_dir(args.package)
    built = describe(folder, args.out, args.repo)
    if args.update_registry:
        path = update_registry(built, folder)
        print(f"Wrote {path}", file=sys.stderr)
    elif args.verify_registry:
        verify_registry(built, folder)
        print(f"Verified {registry_path(built['id'])} against {built['zip']}", file=sys.stderr)
    print(json.dumps(summary(built), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
