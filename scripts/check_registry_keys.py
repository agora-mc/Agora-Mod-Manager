#!/usr/bin/env python3
"""Refuse a release build with fewer than two compiled catalog keys.

A shipped client needs a primary key for catalogs and an offline recovery key
for a rotation or revocation when the primary is lost or compromised. One key
cannot be replaced in clients that are already installed.

The list is read from the same JSON the client compiles in, with the same
build-time overrides:

    AGORA_REGISTRY_PUBKEYS   comma-separated keys; the first is primary, the rest
                             are recovery keys (takes precedence)
    AGORA_REGISTRY_PUBKEY    one primary key (legacy single-key builds)

Run this before a release build. It exits non-zero for a placeholder, a short
list, an invalid key, or a list with no primary or no recovery role.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_KEYS = REPO_ROOT / "crates" / "agora-core" / "registry-keys.json"
PLACEHOLDER = "REPLACE_WITH_OFFLINE_RECOVERY_PUBLIC_KEY"
ROLES = ("primary", "recovery")


def _from_env() -> list[dict[str, str]] | None:
    many = os.environ.get("AGORA_REGISTRY_PUBKEYS", "").strip()
    if many:
        return [
            {"role": "primary" if index == 0 else "recovery", "public_key_hex": key.strip()}
            for index, key in enumerate(many.split(","))
            if key.strip()
        ]
    one = os.environ.get("AGORA_REGISTRY_PUBKEY", "").strip()
    if one:
        return [{"role": "primary", "public_key_hex": one}]
    return None


def _from_source(path: Path) -> list[dict[str, str]]:
    raw = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(raw, list):
        raise ValueError("the key file must hold an array")
    keys: list[dict[str, str]] = []
    for entry in raw:
        if not isinstance(entry, dict):
            raise ValueError("each key entry must be an object")
        role = entry.get("role")
        key = entry.get("public_key_hex")
        if not isinstance(role, str) or not isinstance(key, str):
            raise ValueError("each key entry needs a role and public_key_hex")
        keys.append({"role": role, "public_key_hex": key})
    return keys


def check(keys: list[dict[str, str]]) -> list[str]:
    problems: list[str] = []
    if len(keys) < 2:
        problems.append(
            "fewer than two compiled keys: a shipped client cannot rotate a lost or "
            "compromised key"
        )
    roles = [key["role"] for key in keys]
    for role in ROLES:
        if role not in roles:
            problems.append(f"no {role} key is compiled in")
    for index, key in enumerate(keys):
        value = key["public_key_hex"].strip()
        if value == PLACEHOLDER:
            problems.append(
                f"key {index} is still the recovery placeholder; generate the offline "
                "recovery key and put its public half in crates/agora-core/registry-keys.json"
            )
            continue
        if len(value) != 64 or any(ch not in "0123456789abcdefABCDEF" for ch in value):
            problems.append(
                f"key {index} is not a 32-byte Ed25519 public key in hex"
            )
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--path",
        type=Path,
        default=DEFAULT_KEYS,
        help=f"key file to inspect (default: {DEFAULT_KEYS})",
    )
    args = parser.parse_args()

    try:
        keys = _from_env()
        source = "the environment"
        if keys is None:
            keys = _from_source(args.path)
            source = str(args.path)
    except (OSError, ValueError) as exc:
        print(f"Could not read the registry keys: {exc}", file=sys.stderr)
        return 1

    problems = check(keys)
    if problems:
        print(
            "This build does not carry the catalog trust a released client needs:",
            file=sys.stderr,
        )
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        print(
            "\nGenerate the offline key with:\n"
            "  python scripts/generate_registry_key.py "
            "--private-key-out <a path outside this repository>\n"
            "and put only its printed public key in the recovery slot.",
            file=sys.stderr,
        )
        return 1

    print(
        f"OK: {len(keys)} compiled catalog keys from {source} "
        f"({', '.join(key['role'] for key in keys)})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
