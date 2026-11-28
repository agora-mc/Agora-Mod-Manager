#!/usr/bin/env python3
"""Write a catalog key rotation certificate.

A catalog bundle names the key that signed it. A client accepts a new key only
when the bundle also carries a certificate: a statement, signed by a *recovery*
key, that says "this id belongs to this public key with this role". Sign the
first catalog with the new key, pass the certificate with
``AGORA_REGISTRY_KEY_CERTIFICATES``, and clients that trust the recovery key
accept it. A revocation must be a separate statement signed by a recovery key,
made with ``scripts/make_registry_revocation.py``.

Usage:

    python scripts/make_registry_key_certificate.py \\
        --new-public-key <hex> \\
        --out key-certificate.json

The old private key comes from ``--signing-key`` or the environment
(``AGORA_REGISTRY_OLD_PRIVATE_KEY``, then ``ED25519_OLD_PRIVATE_KEY``). It is
never written anywhere. Treat the output as public: it contains no secret.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path

try:
    import nacl.encoding  # type: ignore
    import nacl.signing  # type: ignore
except ImportError:
    nacl = None  # type: ignore

CERT_DOMAIN = b"AGORA-CATALOG-KEY-CERT-V1\n"


def key_id(public_key: bytes) -> str:
    return hashlib.sha256(public_key).hexdigest()[:16]


def parse_seed(raw: str) -> bytes:
    seed = bytes.fromhex(raw.strip())
    if len(seed) == 32:
        return seed
    if len(seed) == 64:
        return seed[:32]
    raise ValueError("the signing key must be 32 or 64 bytes of hex")


def main() -> int:
    if nacl is None:
        print("PyNaCl is required (pip install -r compiler/requirements.txt).", file=sys.stderr)
        return 1
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--new-public-key", required=True, help="hex Ed25519 public key")
    parser.add_argument(
        "--role",
        choices=("primary", "recovery"),
        default="primary",
        help="role of the introduced key (default: primary)",
    )
    parser.add_argument(
        "--signing-key",
        default=os.environ.get("AGORA_REGISTRY_OLD_PRIVATE_KEY")
        or os.environ.get("ED25519_OLD_PRIVATE_KEY"),
        help="old private key in hex (default: AGORA_REGISTRY_OLD_PRIVATE_KEY)",
    )
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    if not args.signing_key:
        print(
            "An old private key is required: pass --signing-key or set "
            "AGORA_REGISTRY_OLD_PRIVATE_KEY.",
            file=sys.stderr,
        )
        return 1
    try:
        new_public = bytes.fromhex(args.new_public_key.strip())
        if len(new_public) != 32:
            raise ValueError("the new public key must be 32 bytes of hex")
        old_key = nacl.signing.SigningKey(parse_seed(args.signing_key))
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        return 1

    old_public = old_key.verify_key.encode(encoder=nacl.encoding.RawEncoder)
    certificate = {
        "key_id": key_id(new_public),
        "public_key": new_public.hex(),
        "role": args.role,
        "signed_by": key_id(old_public),
    }
    message = (
        CERT_DOMAIN
        + certificate["key_id"].encode("ascii")
        + b"\n"
        + certificate["public_key"].encode("ascii")
        + b"\n"
        + certificate["role"].encode("ascii")
        + b"\n"
        + certificate["signed_by"].encode("ascii")
    )
    certificate["signature"] = old_key.sign(
        message, encoder=nacl.encoding.RawEncoder
    )[:64].hex()

    args.out.write_text(json.dumps([certificate], indent=2) + "\n", encoding="utf-8")
    print(f"Wrote key certificate for {certificate['key_id']} to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
