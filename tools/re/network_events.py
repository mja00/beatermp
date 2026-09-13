#!/usr/bin/env python3
"""Recover the `beaterCore::game::network::events::NetworkEvent` enum from the game binary.

The enum is serialised by bincode as `u32 discriminant` + body.  serde-drive emits
a jump table for the variant index, and the discriminator equals the variant's
declaration index (verified against the captured wire ids in
`crates/codec/tests/fixtures`).  This tool reads the deserialiser's dispatch:

* `visit_u32` maps the wire `u32` to a 1-byte serde field index (identity for
  every id this project has seen on the wire).
* `visit_enum` maps that field index to an arm; each arm either tags a unit /
  newtype / struct variant inline or calls `VariantAccess::tuple_variant` with a
  visitor whose embedded serde error string names the variant
  (`tuple variant NetworkEvent::X with N elements`).

The string is a `&'static str` in `.data.rel.ro`, so its pointer lives in an
`R_X86_64_RELATIVE` relocation; we resolve it to print the name.

Usage:
    network_events.py <binary> [--json out.json]

Requires `binutils` (`objdump`, `readelf`, `nm`) on PATH.

Caveat: a variant that is a newtype over an inline scalar (e.g. `Disconnect`'s
`DisconnectReason`, `UpdateLocation`'s `Location`) has no call to anchor on, so
it is reported as `unit`; correct those from captures. Tuple variants and
discriminants are unaffected.
"""

from __future__ import annotations

import argparse
import json
import re
import struct
import subprocess
import sys

# `NetworkEvent as Deserialize>::deserialize::__Visitor as Visitor>::visit_enum`
VISIT_ENUM = 0x3D14C0
VISIT_ENUM_END = 0x3D20BB
# jump table at the top of visit_enum, indexed by the serde field byte
DISPATCH_TABLE = 0x9B33C
N_VARIANTS = 49


class Elf:
    """Minimal ELF64 reader: vaddr<->file offset for PT_LOAD, plus RELATIVE relocs."""

    def __init__(self, path: str) -> None:
        self.path = path
        with open(path, "rb") as fh:
            self.data = fh.read()
        if self.data[:4] != b"\x7fELF":
            raise SystemExit(f"{path}: not an ELF")
        e_phoff, = struct.unpack_from("<Q", self.data, 0x20)
        e_phentsize, = struct.unpack_from("<H", self.data, 0x36)
        e_phnum, = struct.unpack_from("<H", self.data, 0x38)
        self.segments: list[tuple[int, int, int]] = []
        for i in range(e_phnum):
            off = e_phoff + i * e_phentsize
            p_type, = struct.unpack_from("<I", self.data, off)
            if p_type != 1:
                continue
            p_offset, p_vaddr, _paddr, p_filesz = struct.unpack_from("<QQQQ", self.data, off + 8)
            self.segments.append((p_vaddr, p_offset, p_filesz))

    def read(self, vaddr: int, size: int) -> bytes | None:
        for v, o, n in self.segments:
            if v <= vaddr and vaddr + size <= v + n:
                return self.data[o + (vaddr - v):o + (vaddr - v) + size]
        return None

    def relative_relocs(self) -> dict[int, int]:
        """{offset: addend} for every R_X86_64_RELATIVE relocation."""
        out: dict[int, int] = {}
        txt = subprocess.run(["readelf", "-rW", self.path], capture_output=True, text=True).stdout
        for line in txt.splitlines():
            m = re.match(r"([0-9a-f]{8,})\s+\S+\s+R_X86_64_RELATIVE\s+([0-9a-f]+)\s*$", line.strip())
            if m:
                out[int(m.group(1), 16)] = int(m.group(2), 16)
        return out


def disassemble(path: str, lo: int, hi: int) -> list[tuple[int, str]]:
    out = subprocess.run(
        ["objdump", "-d", f"--start-address=0x{lo:x}", f"--stop-address=0x{hi:x}", path],
        capture_output=True, text=True).stdout
    lines = []
    for line in out.splitlines():
        m = re.match(r"\s*([0-9a-f]+):\t([0-9a-f ]+)\t(.*)", line)
        if m:
            lines.append((int(m.group(1), 16), m.group(3).strip()))
    return lines


def symbol_table(path: str) -> dict[int, str]:
    syms: dict[int, str] = {}
    out = subprocess.run(["nm", "-C", "-S", path], capture_output=True, text=True).stdout
    for line in out.splitlines():
        m = re.match(r"([0-9a-f]+)\s+\S+\s+\S\s+(.*)", line)
        if m:
            syms[int(m.group(1), 16)] = m.group(2)
    return syms


def expected_string(elf: Elf, relocs: dict[int, int], addr: int) -> str | None:
    """Read a `&'static str` whose pointer is a RELATIVE relocation at `addr`."""
    if addr not in relocs:
        return None
    raw = elf.read(addr + 8, 8)
    if raw is None:
        return None
    (length,) = struct.unpack("<Q", raw)
    if not 0 < length < 256:
        return None
    s = elf.read(relocs[addr], length)
    if s is None:
        return None
    try:
        return s.decode()
    except UnicodeDecodeError:
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("binary")
    ap.add_argument("--json", help="write the table as JSON to this path")
    args = ap.parse_args()

    elf = Elf(args.binary)
    relocs = elf.relative_relocs()
    syms = symbol_table(args.binary)
    sym_addrs = sorted(syms)
    lines = disassemble(args.binary, VISIT_ENUM, VISIT_ENUM_END)
    by_addr = {a: i for i, (a, _) in enumerate(lines)}

    table = elf.read(DISPATCH_TABLE, N_VARIANTS * 4)
    if table is None:
        raise SystemExit(f"{args.binary}: no dispatch table at 0x{DISPATCH_TABLE:x}")
    arms = []
    for i in range(N_VARIANTS):
        (rel,) = struct.unpack_from("<i", table, i * 4)
        arms.append(DISPATCH_TABLE + rel)

    def resolved(addr: int) -> str:
        base = max((k for k in sym_addrs if k <= addr), default=None)
        return syms.get(base, "")

    arm_ends = sorted(set(arms))

    def tuple_name(fn: int) -> str | None:
        """The NetworkEvent expected string embedded in a tuple-variant visitor."""
        hi = min((q for q in sym_addrs if q > fn), default=fn + 0x900)
        for _, txt in disassemble(args.binary, fn, hi):
            m = re.search(r"lea\s+[^,]+,%rsi\s+# ([0-9a-f]+)", txt)
            if not m:
                continue
            cand = expected_string(elf, relocs, int(m.group(1), 16))
            if cand and cand.startswith("tuple variant NetworkEvent::"):
                return cand
        return None

    def arm_body(start: int) -> dict:
        """Decode one arm: element/field types, arity and (for tuple variants) name."""
        info: dict = {"kind": "unit", "type": None, "fields": None, "arity": None, "name": None}
        idx = by_addr.get(start)
        if idx is None:
            return info
        region_end = next((a for a in arm_ends if a > start), VISIT_ENUM_END)
        arity = None
        tuple_fn = None
        for k in range(idx, len(lines)):
            _, txt = lines[k]
            m = re.search(r"mov\s+\$0x([0-9a-f]+),%edx", txt)
            if m and info["kind"] == "unit":
                arity = int(m.group(1), 16)
            m = re.search(r"mov\s+\$0x([0-9a-f]+),%r9d", txt)
            if m and info["type"] is None:
                info["fields"] = int(m.group(1), 16)
            m = re.search(r"call\s+([0-9a-f]+)", txt)
            if m:
                s = resolved(int(m.group(1), 16))
                if "tuple_variant" in s and info["kind"] in ("unit", "tuple"):
                    info["kind"] = "tuple"
                    info["arity"] = arity if info["arity"] is None else info["arity"]
                    tuple_fn = int(m.group(1), 16)
                if info["kind"] in ("unit", "tuple") and "newtype_variant_seed" in s:
                    mm = re.search(r"PhantomData(.+?)>>", s)
                    info["kind"] = "newtype"
                    info["type"] = (mm.group(1) if mm else "?").strip()
                if info["kind"] in ("unit", "tuple") and "deserialize_struct" in s:
                    mm = re.search(r"(\w+) as serde_core::de::Deserialize", s)
                    info["kind"] = "struct"
                    info["type"] = mm.group(1) if mm else "?"
                if info["kind"] in ("unit", "tuple") and "deserialize_string" in s:
                    info["kind"] = "newtype"
                    info["type"] = "String"
            if txt.startswith("jmp") or txt == "ret":
                break

        if info["kind"] == "tuple":
            name = tuple_name(tuple_fn) if tuple_fn else None
        else:
            name = None
        if name is None:
            # A tuple visitor can be inlined into the arm; its expected string
            # still sits in the arm's cold path, so look for it in the region.
            for addr, txt in lines[idx:]:
                if addr >= region_end:
                    break
                m = re.search(r"lea\s+[^,]+,%rsi\s+# ([0-9a-f]+)", txt)
                if not m:
                    continue
                cand = expected_string(elf, relocs, int(m.group(1), 16))
                if cand and cand.startswith("tuple variant NetworkEvent::"):
                    name = cand
                    info["kind"] = "tuple"
                    break
        if name:
            info["name"] = name.replace("tuple variant NetworkEvent::", "").rsplit(" with ", 1)[0]
            am = re.search(r"with (\d+) element", name)
            if am:
                info["arity"] = int(am.group(1))
        return info

    rows = []
    for i, arm in enumerate(arms):
        body = arm_body(arm)
        body = {"discriminant": i, **body}
        rows.append(body)

    for r in rows:
        name = r["name"] or ""
        if r["kind"] == "tuple":
            detail = f"tuple({r['arity']})"
        elif r["kind"] == "struct":
            detail = f"struct {r['type']} ({r['fields']} fields)"
        else:
            detail = f"{r['kind']} {r['type'] or ''}".strip()
        print(f"{r['discriminant']:>3}  {detail:<34} {name}")

    if args.json:
        with open(args.json, "w") as fh:
            json.dump(rows, fh, indent=2)
        print(f"\nwrote {len(rows)} variants to {args.json}", file=sys.stderr)

    # Cross-check: every serde name literal in rodata should have been placed.
    # An inlined tuple visitor can leave its literal orphaned (no reference from
    # the arm), so surface those rather than silently dropping them.
    found = {m.group(1).decode() for m in re.finditer(
        rb"tuple variant NetworkEvent::(\w+) with \d+ elements?", elf.data)}
    placed = {r["name"] for r in rows if r["name"]}
    for name in sorted(found - placed):
        print(f"  ! {name}: name literal present but its dispatch arm was inlined", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
