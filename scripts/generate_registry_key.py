#!/usr/bin/env python3
"""Generate an Ed25519 catalog key pair for the offline recovery key.

The private key is written only to the path the owner names, which must be
outside this repository. The public key is printed to stdout; that is the only
half that belongs in `crates/agora-core/registry-keys.json` or the Actions
variables.

    python scripts/generate_registry_key.py --private-key-out D:/keys/agora-recovery.key

Do not commit the private key, paste it into an issue, or keep it on the CI
host. The compiled catalog is signed by the primary key; when the primary is lost
or compromised, the recovery key signs a certificate that introduces the
replacement primary, and the replacement's catalog carries the revocation.
"""

from __future__ import annotations

import argparse
import os
import stat
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

def _is_inside_repo(path: Path) -> bool:
    try:
        path.resolve().relative_to(REPO_ROOT.resolve())
        return True
    except ValueError:
        return False


def main() -> int:
    try:
        import nacl.encoding  # type: ignore
        import nacl.signing  # type: ignore
    except ImportError:
        print("PyNaCl is required (pip install -r compiler/requirements.txt).", file=sys.stderr)
        return 1

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--private-key-out", type=Path, required=True)
    parser.add_argument(
        "--force",
        action="store_true",
        help="overwrite an existing private key file",
    )
    args = parser.parse_args()

    destination = args.private_key_out
    if _is_inside_repo(destination):
        print(
            "Refusing to write the private key inside the repository. Choose a path "
            "outside it.",
            file=sys.stderr,
        )
        return 1
    if destination.exists() and not args.force:
        print(
            f"{destination} already exists; pass --force to replace it.",
            file=sys.stderr,
        )
        return 1

    signing = nacl.signing.SigningKey.generate()
    public = signing.verify_key.encode(encoder=nacl.encoding.RawEncoder)
    private = bytes(signing)

    destination.parent.mkdir(parents=True, exist_ok=True)
    # 0600 where the OS supports it; Windows ACLs are the owner's job.
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    descriptor = os.open(destination, flags, stat.S_IRUSR | stat.S_IWUSR)
    try:
        with os.fdopen(descriptor, "w", encoding="ascii") as handle:
            handle.write(private.hex() + "\n")
    except OSError as exc:
        print(f"Could not write {destination}: {exc}", file=sys.stderr)
        return 1

    print(public.hex())
    print(
        f"Private key written to {destination}. Put the public key above in the "
        "recovery slot; never commit the private key.",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
