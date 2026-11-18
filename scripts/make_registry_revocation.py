#!/usr/bin/env python3
"""Write a catalog key revocation statement.

Only a recovery key may revoke another key. The statement names the key id to
retire and the recovery key id that signs it; the client verifies the signature
against a recovery key it already trusts. A bare list of ids in a catalog is not
accepted.

Usage:

    python scripts/make_registry_revocation.py \\
        --revoked-public-key <hex> \\
        --signing-key <recovery private key hex> \\
        --out revocation.json

or pass ``--revoked-key-id <16 hex>`` when the id is already known. The signing
key comes from ``--signing-key``, or the environment
(``AGORA_REGISTRY_RECOVERY_PRIVATE_KEY``). It is never written anywhere. Pass
the resulting file to the compiler with ``AGORA_REGISTRY_REVOCATIONS``.
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
    print("PyNaCl is required (pip install -r compiler/requirements.txt).", file=sys.stderr)
    raise SystemExit(1)

REVOKE_DOMAIN = b"AGORA-CATALOG-KEY-REVOKE-V1\n"


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
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument("--revoked-key-id", help="16 hex key id to revoke")
    target.add_argument("--revoked-public-key", help="hex Ed25519 public key to revoke")
    parser.add_argument(
        "--signing-key",
        default=os.environ.get("AGORA_REGISTRY_RECOVERY_PRIVATE_KEY")
        or os.environ.get("ED25519_RECOVERY_PRIVATE_KEY"),
        help="recovery private key in hex (default: AGORA_REGISTRY_RECOVERY_PRIVATE_KEY)",
    )
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    if not args.signing_key:
        print(
            "A recovery private key is required: pass --signing-key or set "
            "AGORA_REGISTRY_RECOVERY_PRIVATE_KEY.",
            file=sys.stderr,
        )
        return 1
    try:
        if args.revoked_key_id:
            revoked_id = args.revoked_key_id.strip().lower()
            if len(revoked_id) != 16 or any(ch not in "0123456789abcdef" for ch in revoked_id):
                raise ValueError("--revoked-key-id must be 16 hex characters")
        else:
            revoked_public = bytes.fromhex(args.revoked_public_key.strip())
            if len(revoked_public) != 32:
                raise ValueError("the revoked public key must be 32 bytes of hex")
            revoked_id = key_id(revoked_public)
        recovery = nacl.signing.SigningKey(parse_seed(args.signing_key))
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        return 1

    recovery_public = recovery.verify_key.encode(encoder=nacl.encoding.RawEncoder)
    signed_by = key_id(recovery_public)
    if revoked_id == signed_by:
        print("A recovery key may not revoke itself.", file=sys.stderr)
        return 1

    message = (
        REVOKE_DOMAIN
        + revoked_id.encode("ascii")
        + b"\n"
        + signed_by.encode("ascii")
    )
    statement = {
        "revoked_key_id": revoked_id,
        "signed_by": signed_by,
        "signature": recovery.sign(message, encoder=nacl.encoding.RawEncoder)[:64].hex(),
    }
    args.out.write_text(json.dumps([statement], indent=2) + "\n", encoding="utf-8")
    print(f"Wrote revocation of {revoked_id} to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
