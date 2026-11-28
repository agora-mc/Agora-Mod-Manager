#!/usr/bin/env python3
"""Publish a published desktop release's ``latest.json`` to the dedicated updater release.

The repository hosts three independent release streams (catalog ``registry-*``,
desktop ``vX.Y.Z``, and official plugins ``official-*-v*``), so GitHub's
``/releases/latest`` endpoint moves to whichever stream published last. The
desktop updater therefore reads ``latest.json`` from a dedicated ``updater``
prerelease (``releases/download/updater/latest.json``) that only published
``vX.Y.Z`` desktop releases update.

The platform download URLs and minisign signatures inside ``latest.json`` stay
unchanged and continue to point at ``releases/download/vX.Y.Z/...``.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Sequence

DESKTOP_TAG_PATTERN = re.compile(r"^v(?P<major>\d+)\.(?P<minor>\d+)\.(?P<patch>\d+)$")
DEFAULT_UPDATER_TAG = "updater"
REQUIRED_SIGNED_PLATFORMS = (
    "windows-x86_64",
    "windows-x86_64-msi",
    "windows-x86_64-nsis",
)


class UpdaterPublishError(RuntimeError):
    """The release cannot safely update the desktop updater manifest."""


def parse_desktop_version(tag: str) -> tuple[int, int, int]:
    """Return ``(major, minor, patch)`` for a ``vX.Y.Z`` tag, or raise."""
    match = DESKTOP_TAG_PATTERN.fullmatch(tag)
    if not match:
        raise UpdaterPublishError(
            f"Tag {tag!r} is not a desktop release tag (expected vX.Y.Z)."
        )
    return (
        int(match.group("major")),
        int(match.group("minor")),
        int(match.group("patch")),
    )


def parse_plain_version(version: str) -> tuple[int, int, int]:
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version.strip())
    if not match:
        raise UpdaterPublishError(f"Manifest version {version!r} is not X.Y.Z.")
    return tuple(int(part) for part in match.groups())  # type: ignore[return-value]


def validate_updater_manifest(manifest: dict, tag: str) -> None:
    """Verify ``latest.json`` carries valid signatures and URLs for ``tag``."""
    expected_tuple = parse_desktop_version(tag)
    expected_version = ".".join(str(n) for n in expected_tuple)

    raw_version = manifest.get("version")
    if not isinstance(raw_version, str) or raw_version.lstrip("v") != expected_version:
        raise UpdaterPublishError(
            f"latest.json version {raw_version!r} does not match release tag {tag!r}."
        )

    platforms = manifest.get("platforms")
    if not isinstance(platforms, dict) or not platforms:
        raise UpdaterPublishError("latest.json has no platform entries.")

    for required in REQUIRED_SIGNED_PLATFORMS:
        entry = platforms.get(required)
        if not isinstance(entry, dict) or not isinstance(entry.get("signature"), str) or not entry["signature"].strip():
            raise UpdaterPublishError(f"latest.json is missing a signed {required} entry.")

    expected_segment = f"/releases/download/{tag}/"
    for key, entry in platforms.items():
        if not isinstance(entry, dict):
            raise UpdaterPublishError(f"latest.json platform {key!r} is not an object.")
        sig = entry.get("signature")
        if not isinstance(sig, str) or not sig.strip():
            raise UpdaterPublishError(f"latest.json platform {key!r} has no signature.")
        url = entry.get("url")
        if not isinstance(url, str) or expected_segment not in url:
            raise UpdaterPublishError(
                f"latest.json platform {key!r} URL {url!r} does not point at {tag}."
            )


def run_gh(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(["gh", *args], capture_output=True, text=True)
    if check and result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or f"exit code {result.returncode}"
        raise UpdaterPublishError(f"gh {' '.join(args)} failed: {detail}")
    return result


def fetch_release_metadata(repo: str, tag: str) -> dict:
    proc = run_gh(
        "release",
        "view",
        tag,
        "--repo",
        repo,
        "--json",
        "tagName,isDraft,isPrerelease,assets",
    )
    return json.loads(proc.stdout)


def ensure_updater_release(repo: str, updater_tag: str) -> None:
    view = run_gh("release", "view", updater_tag, "--repo", repo, "--json", "tagName", check=False)
    if view.returncode == 0:
        return
    run_gh(
        "release",
        "create",
        updater_tag,
        "--repo",
        repo,
        "--prerelease",
        "--title",
        "Desktop updater manifest",
        "--notes",
        "Internal Tauri updater manifest (`latest.json`) for published `vX.Y.Z` desktop releases. "
        "Download installers from the `vX.Y.Z` releases.",
    )


def existing_updater_version(repo: str, updater_tag: str, workdir: Path) -> tuple[int, int, int] | None:
    target = workdir / "current-updater-latest.json"
    dl = run_gh(
        "release",
        "download",
        updater_tag,
        "--repo",
        repo,
        "--pattern",
        "latest.json",
        "--output",
        str(target),
        check=False,
    )
    if dl.returncode != 0 or not target.is_file():
        return None
    try:
        data = json.loads(target.read_text(encoding="utf-8"))
        raw = data.get("version")
        if isinstance(raw, str):
            return parse_plain_version(raw.lstrip("v"))
    except (ValueError, UpdaterPublishError):
        return None
    return None


def publish_updater_manifest(
    repo: str,
    tag: str,
    updater_tag: str = DEFAULT_UPDATER_TAG,
    allow_downgrade: bool = False,
) -> None:
    target_version = parse_desktop_version(tag)
    if tag == updater_tag:
        raise UpdaterPublishError(f"Source tag {tag!r} cannot be the updater release tag.")

    meta = fetch_release_metadata(repo, tag)
    if meta.get("isDraft"):
        raise UpdaterPublishError(f"Release {tag} is still a draft; publish it before updating {updater_tag}.")
    if meta.get("isPrerelease"):
        raise UpdaterPublishError(f"Release {tag} is a prerelease and cannot update {updater_tag}.")

    assets = [a.get("name") for a in meta.get("assets", []) if isinstance(a, dict)]
    if "latest.json" not in assets:
        raise UpdaterPublishError(f"Release {tag} does not carry latest.json.")

    with tempfile.TemporaryDirectory() as tmp:
        workdir = Path(tmp)
        manifest_path = workdir / "latest.json"
        run_gh(
            "release",
            "download",
            tag,
            "--repo",
            repo,
            "--pattern",
            "latest.json",
            "--output",
            str(manifest_path),
        )
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        validate_updater_manifest(manifest, tag)

        ensure_updater_release(repo, updater_tag)
        if not allow_downgrade:
            current_version = existing_updater_version(repo, updater_tag, workdir)
            if current_version is not None and current_version > target_version:
                raise UpdaterPublishError(
                    f"Refusing to replace newer updater manifest {'.'.join(map(str, current_version))} "
                    f"with older {tag} (pass --allow-downgrade to override)."
                )

        run_gh("release", "upload", updater_tag, str(manifest_path), "--repo", repo, "--clobber")
    print(f"Published {tag} latest.json to {updater_tag} release.")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--repo", required=True, help="GitHub repository, owner/name")
    parser.add_argument("--tag", required=True, help="Published desktop release tag, vX.Y.Z")
    parser.add_argument(
        "--updater-tag",
        default=DEFAULT_UPDATER_TAG,
        help=f"Dedicated updater release tag (default: {DEFAULT_UPDATER_TAG})",
    )
    parser.add_argument(
        "--allow-downgrade",
        action="store_true",
        help="Allow replacing a newer updater manifest with an older vX.Y.Z release",
    )
    args = parser.parse_args(argv)

    try:
        publish_updater_manifest(
            args.repo,
            args.tag,
            updater_tag=args.updater_tag,
            allow_downgrade=args.allow_downgrade,
        )
    except UpdaterPublishError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
