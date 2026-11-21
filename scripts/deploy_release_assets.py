#!/usr/bin/env python3
"""Deploy the signed registry database and web export release assets.

Creates (or updates) a tagged release named registry-YYYY-MM-DD, uploads the
four user-facing files as assets, and prunes old registry-* releases to keep only the
latest 7.

Usage:
    python scripts/deploy_release_assets.py

Requires:
    GITHUB_TOKEN      — GitHub access token (set automatically in Actions)
    GITHUB_REPOSITORY — Repo slug in owner/repo format (set automatically in Actions)
    registry.db       — The compiled database file (in the working directory)
    registry.db.sig   — The Ed25519 signature file (in the working directory)
    registry.bundle   — The signed catalog bundle the launcher installs
    registry-web.json — The web registry export (in the working directory)
    registry-web.json.sig — The web export signature (in the working directory)

Each release also carries ``registry.header`` and ``registry.delta-*.zst``
(MASTER_SPEC §33), built here from the bundles of the releases it keeps, so a
client holding any of them downloads a few KB instead of the whole bundle.
Delta failures are logged and skipped: the full bundle is always published.
"""

from __future__ import annotations

import datetime
import logging
import os
import sys
from pathlib import Path
from urllib.parse import quote

import requests

import catalog_delta

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
logger = logging.getLogger("deploy_release_assets")

API_BASE = "https://api.github.com"
MAX_RELEASES_TO_KEEP = 7
RELEASE_ASSETS = (
    Path("registry.bundle"),
    Path("registry.db"),
    Path("registry.db.sig"),
    Path("registry-web.json"),
    Path("registry-web.json.sig"),
)


def main() -> int:
    token = os.environ.get("GITHUB_TOKEN")
    repo = os.environ.get("GITHUB_REPOSITORY")

    if not token:
        logger.error("GITHUB_TOKEN is not set.")
        return 1
    if not repo:
        logger.error("GITHUB_REPOSITORY is not set.")
        return 1

    for asset_path in RELEASE_ASSETS:
        if not asset_path.exists():
            logger.error("%s not found in working directory.", asset_path)
            return 1

    tag = f"registry-{datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%d')}"
    logger.info("Deploying to %s with tag %s", repo, tag)

    headers = {
        "Authorization": f"token {token}",
        "Accept": "application/vnd.github+json",
        "User-Agent": "AgoraRegistryDeploy/1.0",
        "X-GitHub-Api-Version": "2022-11-28",
    }

    # Check if release already exists for this tag.
    release = get_release_by_tag(headers, repo, tag)

    if release is None:
        release = create_release(headers, repo, tag)
    else:
        logger.info("Release %s already exists (id=%s), updating assets.", tag, release["id"])
        # Remove old assets with same names.
        delete_existing_assets(headers, repo, release)

    for asset_path in RELEASE_ASSETS:
        upload_asset(headers, release, asset_path)

    for asset_path in build_delta_assets(headers, repo, tag):
        upload_asset(headers, release, asset_path)

    logger.info("Upload complete.")

    # Prune old registry-* releases.
    prune_old_releases(headers, repo)

    logger.info("Done.")
    return 0


def get_release_by_tag(headers: dict, repo: str, tag: str) -> dict | None:
    url = f"{API_BASE}/repos/{repo}/releases/tags/{quote(tag)}"
    resp = requests.get(url, headers=headers)
    if resp.status_code == 200:
        return resp.json()
    if resp.status_code == 404:
        return None
    logger.error("Failed to check release by tag %s: HTTP %d %s", tag, resp.status_code, resp.text)
    sys.exit(1)


def create_release(headers: dict, repo: str, tag: str) -> dict:
    url = f"{API_BASE}/repos/{repo}/releases"
    body = {
        "tag_name": tag,
        "name": f"Registry {tag.split('-', 1)[1]}",
        "body": "Nightly registry database build.",
        "draft": False,
        "prerelease": False,
    }
    resp = requests.post(url, headers=headers, json=body)
    if resp.status_code not in (200, 201):
        logger.error("Failed to create release: HTTP %d %s", resp.status_code, resp.text)
        sys.exit(1)
    release = resp.json()
    logger.info("Created release %s (id=%s)", tag, release["id"])
    return release


def delete_existing_assets(headers: dict, repo: str, release: dict) -> None:
    for asset in release.get("assets", []):
        name = asset.get("name", "")
        if name in {path.name for path in RELEASE_ASSETS} or catalog_delta.is_delta_asset(name):
            url = f"{API_BASE}/repos/{repo}/releases/assets/{asset['id']}"
            resp = requests.delete(url, headers=headers)
            if resp.status_code == 204:
                logger.info("Deleted old asset %s", name)
            else:
                logger.warning("Could not delete old asset %s: HTTP %d", name, resp.status_code)


def upload_asset(headers: dict, release: dict, path: Path) -> None:
    upload_url = release["upload_url"].replace("{?name,label}", "")
    filename = path.name
    url = f"{upload_url}?name={quote(filename)}"

    with path.open("rb") as fh:
        resp = requests.post(
            url,
            headers={**headers, "Content-Type": "application/octet-stream"},
            data=fh,
        )

    if resp.status_code not in (200, 201):
        logger.error("Failed to upload %s: HTTP %d %s", filename, resp.status_code, resp.text)
        sys.exit(1)

    size = path.stat().st_size
    logger.info("Uploaded %s (%d bytes)", filename, size)


DELTA_DIR = Path("catalog-deltas")


def build_delta_assets(headers: dict, repo: str, tag: str) -> list[Path]:
    """Deltas from the bundles of the releases kept beside this one."""
    try:
        resp = requests.get(f"{API_BASE}/repos/{repo}/releases?per_page=100", headers=headers)
        resp.raise_for_status()
        releases = [
            r for r in resp.json()
            if r.get("tag_name", "").startswith("registry-") and r.get("tag_name") != tag
        ]
        releases.sort(key=lambda r: r.get("tag_name", ""), reverse=True)
        previous: list[bytes] = []
        for old in releases[:MAX_RELEASES_TO_KEEP]:
            asset = next((a for a in old.get("assets", []) if a.get("name") == "registry.bundle"), None)
            if asset is None:
                continue
            got = requests.get(
                f"{API_BASE}/repos/{repo}/releases/assets/{asset['id']}",
                headers={**headers, "Accept": "application/octet-stream"},
            )
            if got.status_code == 200:
                previous.append(got.content)
            else:
                logger.warning("Could not fetch %s's bundle: HTTP %d", old["tag_name"], got.status_code)
        written = catalog_delta.write_delta_assets(
            Path("registry.bundle").read_bytes(), previous, DELTA_DIR
        )
        logger.info("Built %d catalog delta(s) from %d earlier bundle(s).", len(written) - 1, len(previous))
        return written
    except Exception as exc:  # noqa: BLE001 - deltas are an optimisation; the bundle still ships
        logger.warning("Skipping catalog deltas: %s", exc)
        return []


def prune_old_releases(headers: dict, repo: str) -> None:
    url = f"{API_BASE}/repos/{repo}/releases?per_page=100"
    resp = requests.get(url, headers=headers)
    if resp.status_code != 200:
        logger.warning("Could not list releases for pruning: HTTP %d", resp.status_code)
        return

    releases = resp.json()
    registry_releases = [r for r in releases if r.get("tag_name", "").startswith("registry-")]
    registry_releases.sort(key=lambda r: r.get("tag_name", ""), reverse=True)

    if len(registry_releases) <= MAX_RELEASES_TO_KEEP:
        return

    to_delete = registry_releases[MAX_RELEASES_TO_KEEP:]
    for old in to_delete:
        tag = old["tag_name"]
        release_id = old["id"]

        # Delete the release.
        url = f"{API_BASE}/repos/{repo}/releases/{release_id}"
        resp = requests.delete(url, headers=headers)
        if resp.status_code == 204:
            logger.info("Pruned old release %s", tag)
        else:
            logger.warning("Could not delete release %s: HTTP %d", tag, resp.status_code)

        # Delete the tag ref.
        ref_url = f"{API_BASE}/repos/{repo}/git/refs/tags/{quote(tag)}"
        resp = requests.delete(ref_url, headers=headers)
        if resp.status_code == 204:
            logger.info("Deleted tag %s", tag)
        elif resp.status_code != 404:
            logger.warning("Could not delete tag ref %s: HTTP %d", tag, resp.status_code)


if __name__ == "__main__":
    raise SystemExit(main())
