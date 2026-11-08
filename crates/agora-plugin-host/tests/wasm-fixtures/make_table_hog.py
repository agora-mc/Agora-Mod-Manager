#!/usr/bin/env python3
"""Emit wasm/table_hog.wasm: a WASI command that builds a table in its start.

The module declares a table of 4,000,000 funcref entries and no linear
memory, so it does nothing but ask the host for table storage. The sandbox
sets a linear-memory cap but no table cap (see src/wasm.rs), which is what
the review test exercises. Keep this generator beside the built module so the
binary is reproducible.
"""
import struct

FUNCREF = 0x70
TABLE_MIN = 4_000_000


def uleb(n: int) -> bytes:
    out = bytearray()
    while True:
        byte = n & 0x7F
        n >>= 7
        if n:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def section(section_id: int, body: bytes) -> bytes:
    return bytes([section_id]) + uleb(len(body)) + body


type_section = section(1, uleb(1) + b"\x60\x00\x00")
function_section = section(3, uleb(1) + uleb(0))
table_section = section(4, uleb(1) + bytes([FUNCREF]) + b"\x00" + uleb(TABLE_MIN))
export_section = section(
    7, uleb(1) + uleb(len("_start")) + b"_start" + b"\x00" + uleb(0)
)
code_section = section(10, uleb(1) + uleb(2) + b"\x00\x0b")

module = b"\x00asm\x01\x00\x00\x00" + b"".join(
    [type_section, function_section, table_section, export_section, code_section]
)
with open("wasm/table_hog.wasm", "wb") as handle:
    handle.write(module)
print(f"wrote wasm/table_hog.wasm ({len(module)} bytes)")
