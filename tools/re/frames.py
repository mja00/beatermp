#!/usr/bin/env python3
"""Print every game datagram in a udpsniff capture as one annotated line.

Usage: frames.py <capture.txt> [--fd N] [--from LINE] [--to LINE] [--kinds]

The envelope decode mirrors crates/codec: kind byte, then per kind
  0 Data      [u64 len][payload][u32 seq][u8][u64 unix][tail...]
  1 Heartbeat [u32 id][f32]
  2 Ack       [u32 seq]
  3 Greeting  [u32 id]
Anything the envelope does not account for is printed verbatim as `tail=`
so that undocumented fields stand out instead of being silently dropped.
`--kinds` prints a tally instead of the per-frame listing.
"""
from __future__ import annotations

import argparse
import struct
from collections import Counter


def decode(raw: bytes) -> str:
    kind = raw[0]
    if kind == 1:
        ident, clock = struct.unpack_from("<If", raw, 1)
        return f"HB id={ident} clock={clock:.3f} tail={raw[9:].hex()}"
    if kind == 2:
        return f"ACK seq={struct.unpack_from('<I', raw, 1)[0]} tail={raw[5:].hex()}"
    if kind == 3:
        return f"GREET id={struct.unpack_from('<I', raw, 1)[0]} tail={raw[5:].hex()}"
    if kind != 0:
        return f"KIND{kind} {raw.hex()}"
    (plen,) = struct.unpack_from("<Q", raw, 1)
    payload = raw[9 : 9 + plen]
    o = 9 + plen
    seq, flag, unix = struct.unpack_from("<IBQ", raw, o)
    tail = raw[o + 13 :]
    disc = struct.unpack_from("<I", payload, 0)[0] if len(payload) >= 4 else -1
    return (
        f"DATA disc={disc} seq={seq} flag={flag} len={plen} tail={tail.hex()} "
        f"payload={payload.hex()}"
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("capture")
    ap.add_argument("--fd", type=int)
    ap.add_argument("--from", dest="start", type=int, default=1)
    ap.add_argument("--to", dest="end", type=int, default=1 << 62)
    ap.add_argument("--kinds", action="store_true")
    ap.add_argument("--no-hb", action="store_true", help="skip heartbeats")
    a = ap.parse_args()

    tally: Counter[str] = Counter()
    with open(a.capture) as f:
        for n, line in enumerate(f, 1):
            if n < a.start or n > a.end:
                continue
            parts = line.split()
            if len(parts) < 6:
                continue
            ts, direction, fd, size, peer, hexdata = parts[:6]
            if a.fd is not None and int(fd) != a.fd:
                continue
            if len(hexdata) % 2:
                # Interleaved writes from two threads can truncate a line.
                continue
            raw = bytes.fromhex(hexdata)
            if len(raw) < 5 or raw[0] > 3:
                continue
            try:
                text = decode(raw)
            except struct.error:
                text = f"SHORT {raw.hex()}"
            if a.no_hb and text.startswith("HB"):
                continue
            if a.kinds:
                key = " ".join(text.split()[:2]) + (" " + text.split()[3] if text.startswith("DATA") else "")
                tally[f"{direction} {key}"] += 1
            else:
                print(f"{n} {ts} {direction} fd={fd} {text}")
    for key, count in sorted(tally.items(), key=lambda kv: -kv[1]):
        print(f"{count:6d} {key}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
