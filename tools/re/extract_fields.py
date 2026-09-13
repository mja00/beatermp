#!/usr/bin/env python3
"""Recover serde-derived struct layouts from a Rust binary.

serde's derive emits, for every struct/enum it serialises, a static
``&'static [&'static str]`` table of field (or variant) names, referenced from
the generated ``Serialize``/``Deserialize`` impl. rustc merges string literals
into one rodata blob and represents each ``&str`` as a (pointer, length) pair,
so a field table is a contiguous run of 16-byte entries whose pointer lands
inside the blob and whose bytes at that pointer equal the name.

Scanning for those runs recovers the field names *in declaration order*, which
is exactly the order bincode writes them in. Field names alone are not enough
to decode a packet -- widths come from the type declarations -- but they turn
"here is a 40-byte blob" into "this is ServerInfo.host, ServerInfo.map, ...".

Usage:
    extract_fields.py <binary> [--json out.json] [--blob <substring>]

Prints one line per recovered table:
    <address>  <n>  name1 name2 name3 ...
"""

from __future__ import annotations

import argparse
import json
import struct
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


def blob_vaddr(elf: Elf, marker: bytes) -> tuple[int, int] | None:
    """Locate a marker in the file and return (vaddr, file_offset)."""
    foff = elf.data.find(marker)
    if foff < 0:
        return None
    for p_vaddr, p_offset, p_filesz, _flags, _i in elf.segments:
        if p_offset <= foff < p_offset + p_filesz:
            return p_vaddr + (foff - p_offset), foff
    return None


def collect_strings(elf: Elf, blob_vaddr: int, blob_len: int) -> dict[int, str]:
    """Enumerate every plausible (ptr,len) string description anchored in the blob.

    Returns {file_offset_of_16byte_entry: text}. Only entries whose pointer lies
    inside the blob and whose declared length actually matches the bytes are kept.
    """
    found: dict[int, str] = {}
    for p_vaddr, p_offset, p_filesz, _flags, _i in elf.segments:
        if _flags & 0x2:  # skip writable; serde tables are const
            continue
        end = p_offset + p_filesz - 16
        for off in range(p_offset, end, 8):
            ptr, ln = struct.unpack_from("<QQ", elf.data, off)
            if not (blob_vaddr <= ptr < blob_vaddr + blob_len):
                continue
            if ln == 0 or ln > 64:
                continue
            text_off = elf.vaddr_to_offset(ptr)
            if text_off is None:
                continue
            raw = elf.data[text_off : text_off + ln]
            if len(raw) != ln:
                continue
            try:
                text = raw.decode("utf-8")
            except UnicodeDecodeError:
                continue
            # Field names are plain identifiers; reject anything else so we do
            # not latch onto unrelated (ptr,len) pairs that happen to align.
            if not text.replace("_", "").isalnum():
                continue
            if not (text[0].isalpha() or text[0] == "_"):
                continue
            found[off] = text
    return found


def group_entries(elf: Elf, entries: dict[int, str]) -> list[tuple[int, list[str]]]:
    """Merge adjacent 16-byte entries into ordered tables."""
    tables: list[tuple[int, list[str]]] = []
    for off in sorted(entries):
        if tables and off == tables[-1][0] + 16 * len(tables[-1][1]):
            tables[-1][1].append(entries[off])
        else:
            tables.append((off, [entries[off]]))
    return tables


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("binary")
    ap.add_argument("--json", help="write results as JSON to this path")
    ap.add_argument(
        "--blob",
        default="Packetidgeneration",
        help="marker locating the merged string blob (default: %(default)s)",
    )
    ap.add_argument("--min", type=int, default=2, help="minimum fields per table")
    args = ap.parse_args()

    elf = Elf(args.binary)
    marker = args.blob.encode()
    loc = blob_vaddr(elf, marker)
    if loc is None:
        raise SystemExit(f"marker {marker!r} not found in {args.binary}")
    bv, _boff = loc

    # The merged blob is contiguous; bound it by walking forward while bytes
    # remain printable, which is where all merged literals live.
    blob_len = 0
    while bv + blob_len < bv + 200_000:
        off = elf.vaddr_to_offset(bv + blob_len)
        if off is None:
            break
        ch = elf.data[off]
        if ch == 0 or not (32 <= ch < 127):
            break
        blob_len += 1

    entries = collect_strings(elf, bv, blob_len)
    tables = [(off, names) for off, names in group_entries(elf, entries) if len(names) >= args.min]

    results = []
    for off, names in tables:
        vaddr = None
        for p_vaddr, p_offset, p_filesz, _flags, _i in elf.segments:
            if p_offset <= off < p_offset + p_filesz:
                vaddr = p_vaddr + (off - p_offset)
                break
        results.append({"vaddr": vaddr, "offset": off, "fields": names})
        print(f"0x{vaddr or 0:x}  {len(names):3d}  {' '.join(names)}")

    if args.json:
        with open(args.json, "w") as fh:
            json.dump(results, fh, indent=2)
        print(f"\nwrote {len(results)} tables to {args.json}", file=sys.stderr)
    else:
        print(f"\n{len(results)} tables", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
