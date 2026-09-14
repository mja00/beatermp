#!/usr/bin/env python3
"""Recover serde-derived struct layouts from a Rust binary.

serde's derive emits, for every struct/enum it serialises, a static
``&'static [&'static str]`` table of field (or variant) names, referenced from
the generated ``Serialize``/``Deserialize`` impl. rustc merges string literals
into rodata and represents each ``&str`` as a (pointer, length) pair, so a
field table is a contiguous run of 16-byte entries pointing at identifiers.

The binary is a PIE, so those pointers are zero in the file and live as
``R_X86_64_RELATIVE`` addends instead; the in-file pairs are read as well so a
non-PIE build still works.

Scanning for those runs recovers the field names *in declaration order*, which
is exactly the order bincode writes them in. Field names alone are not enough
to decode a packet -- widths come from the type declarations -- but they turn
"here is a 40-byte blob" into "this is ServerInfo.host, ServerInfo.map, ...".

Usage:
    extract_fields.py <binary> [--json out.json] [--min N]

Prints one line per recovered table:
    <address>  <n>  name1 name2 name3 ...
"""

from __future__ import annotations

import argparse
import json
import re
import struct
import subprocess
import sys

PT_LOAD = 1


class Elf:
    """Just enough ELF64 parsing to map vaddr -> file offset."""

    def __init__(self, path: str) -> None:
        self.path = path
        with open(path, "rb") as fh:
            self.data = fh.read()

        if self.data[:4] != b"\x7fELF" or self.data[4] != 2:
            raise SystemExit(f"{path}: not a 64-bit ELF")

        (self.e_phoff,) = struct.unpack_from("<Q", self.data, 0x20)
        (self.e_phentsize,) = struct.unpack_from("<H", self.data, 0x36)
        (self.e_phnum,) = struct.unpack_from("<H", self.data, 0x38)

        self.segments: list[tuple[int, int, int, int, int]] = []
        for i in range(self.e_phnum):
            off = self.e_phoff + i * self.e_phentsize
            p_type, p_flags = struct.unpack_from("<II", self.data, off)
            if p_type != PT_LOAD:
                continue
            p_offset, p_vaddr, _, p_filesz = struct.unpack_from("<QQQQ", self.data, off + 8)
            self.segments.append((p_vaddr, p_offset, p_filesz, p_flags, i))

    def read_vaddr(self, vaddr: int, size: int) -> bytes | None:
        for p_vaddr, p_offset, p_filesz, _flags, _i in self.segments:
            if p_vaddr <= vaddr and vaddr + size <= p_vaddr + p_filesz:
                start = p_offset + (vaddr - p_vaddr)
                return self.data[start : start + size]
        return None

    def vaddr_to_offset(self, vaddr: int) -> int | None:
        for p_vaddr, p_offset, p_filesz, _flags, _i in self.segments:
            if p_vaddr <= vaddr < p_vaddr + p_filesz:
                return p_offset + (vaddr - p_vaddr)
        return None

    def rodata_ranges(self) -> list[tuple[int, int]]:
        """vaddr ranges of non-writable loadable segments that are not executable."""
        out = []
        for p_vaddr, _p_offset, p_filesz, flags, _i in self.segments:
            writable = flags & 0x2
            executable = flags & 0x1
            if not writable and not executable:
                out.append((p_vaddr, p_filesz))
        return out


def relative_relocs(path: str) -> dict[int, int]:
    """{entry vaddr: addend} for every R_X86_64_RELATIVE relocation."""
    out: dict[int, int] = {}
    txt = subprocess.run(["readelf", "-rW", path], capture_output=True, text=True).stdout
    for line in txt.splitlines():
        m = re.match(r"([0-9a-f]{8,})\s+\S+\s+R_X86_64_RELATIVE\s+([0-9a-f]+)\s*$", line.strip())
        if m:
            out[int(m.group(1), 16)] = int(m.group(2), 16)
    return out


def identifier_at(elf: Elf, ptr: int, length: int) -> str | None:
    """The identifier a (ptr,len) string description points at, if it is one."""
    if not 0 < length <= 64:
        return None
    if not any(lo <= ptr and ptr + length <= lo + n for lo, n in elf.rodata_ranges()):
        return None
    raw = elf.read_vaddr(ptr, length)
    if raw is None:
        return None
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return None
    # Field names are plain identifiers; reject anything else so we do not
    # latch onto unrelated (ptr,len) pairs that happen to align.
    if not text.replace("_", "").isalnum():
        return None
    if not (text[0].isalpha() or text[0] == "_"):
        return None
    return text


def collect_strings(elf: Elf, relocs: dict[int, int]) -> dict[int, str]:
    """{vaddr of the 16-byte entry: identifier} for every string description."""
    found: dict[int, str] = {}
    for vaddr, addend in relocs.items():  # PIE: pointer is the reloc addend
        raw = elf.read_vaddr(vaddr + 8, 8)
        if raw is None:
            continue
        (length,) = struct.unpack("<Q", raw)
        text = identifier_at(elf, addend, length)
        if text is not None:
            found[vaddr] = text
    for p_vaddr, p_offset, p_filesz, _flags, _i in elf.segments:  # non-PIE
        for off in range(p_offset, p_offset + p_filesz - 16, 8):
            ptr, length = struct.unpack_from("<QQ", elf.data, off)
            if ptr == 0:
                continue
            text = identifier_at(elf, ptr, length)
            if text is not None:
                found[p_vaddr + (off - p_offset)] = text
    return found


def group_entries(entries: dict[int, str]) -> list[tuple[int, list[str]]]:
    """Merge adjacent 16-byte entries into ordered tables."""
    tables: list[tuple[int, list[str]]] = []
    for vaddr in sorted(entries):
        if tables and vaddr == tables[-1][0] + 16 * len(tables[-1][1]):
            tables[-1][1].append(entries[vaddr])
        else:
            tables.append((vaddr, [entries[vaddr]]))
    return tables


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("binary")
    ap.add_argument("--json", help="write results as JSON to this path")
    ap.add_argument("--min", type=int, default=2, help="minimum fields per table")
    args = ap.parse_args()

    elf = Elf(args.binary)
    entries = collect_strings(elf, relative_relocs(args.binary))
    tables = [(v, names) for v, names in group_entries(entries) if len(names) >= args.min]

    results = []
    for vaddr, names in tables:
        results.append({"vaddr": vaddr, "offset": elf.vaddr_to_offset(vaddr), "fields": names})
        print(f"0x{vaddr:x}  {len(names):3d}  {' '.join(names)}")

    if args.json:
        with open(args.json, "w") as fh:
            json.dump(results, fh, indent=2)
        print(f"\nwrote {len(results)} tables to {args.json}", file=sys.stderr)
    else:
        print(f"\n{len(results)} tables", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
