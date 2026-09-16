#!/usr/bin/env python3
"""Compute a parked-host pose beside each raceable path, off the racing line.

Writes the table `beatermp` bakes in (`crates/server/parking.txt`): one
`<map> <variant> <count>` header followed by one `<count>`-long block of full
`rx ry rz rw x y z` poses (same layout as `maps.txt`), one per grid slot. Slot
`i` is where the phantom parks when `i` clients race: the anchor is grid slot
`i`, moved sideways until it clears every path segment.

The search is geometric, from data already on disk:
- the racing line is `info.ron`'s path waypoints, each with its `width`;
- clearance means `distance(point, segment) > width/2 + CAR_MARGIN` for every
  segment, plus `SPAWN_CLEAR` from every grid spawn (no overlapping cars);
- the offset direction is perpendicular to the anchor's facing, both sides
  tried, nearest acceptable distance first;
- height and rotation come from the anchor spawn: the phantom may settle a
  little if the shoulder differs, which is harmless -- only parking it far
  under the terrain corrupted client physics (NaN states, instant finishes);
- candidates outside the waypoint extent plus `EDGE_MARGIN` are rejected, so
  the pose stays on the map.

A map/variant with no acceptable shoulder is omitted; the server then falls
back to the normal grid pose.

Usage: parking.py [game_dir] > crates/server/parking.txt
"""

import math
import re
import sys
from pathlib import Path

DEFAULT_GAME = "/mnt/data-drive/SteamLibrary/steamapps/common/BeaterCore"
REPO = Path(__file__).resolve().parent.parent.parent

# Road half-width + this, in metres, before a spot is "off the racing line".
CAR_MARGIN = 3.0
# Minimum distance from every grid spawn, so the phantom never overlaps a car.
SPAWN_CLEAR = 4.0
# How far outside the waypoint bounding box a candidate may sit.
EDGE_MARGIN = 30.0
# Search: offsets perpendicular to the anchor's facing, near to far.
MIN_OFFSET = 5.0
MAX_OFFSET = 60.0
OFFSET_STEP = 0.5
# Small along-track nudges, so a dead-blocked shoulder can be escaped.
LONGITUDINAL = [0.0, 2.0, -2.0, 4.0, -4.0, 6.0, -6.0]


def load_maps_txt(path: Path):
    """`[(map, variant, [spawn poses])]` exactly as `spawns.py` wrote them."""
    entries = []
    lines = path.read_text().splitlines()
    i = 0
    while i < len(lines):
        head = lines[i].split()
        if len(head) == 3:
            count = int(head[2])
            rows = [[float(v) for v in row.split()] for row in lines[i + 1 : i + 1 + count]]
            entries.append((head[0], head[1], rows))
            i += 1 + count
        else:
            i += 1
    return entries


def balanced_end(text: str, start: int) -> int:
    """Index just past the `]` closing the `[` at `start` (nesting-aware)."""
    depth = 1
    for i in range(start, len(text)):
        if text[i] == "[":
            depth += 1
        elif text[i] == "]":
            depth -= 1
            if depth == 0:
                return i + 1
    return len(text)


def path_segments(info_ron: str):
    """`(ax, ay, az, bx, bz, half_width)` for consecutive waypoint pairs.

    Waypoints form a graph (`points` + `next` indices), but consecutive file
    order follows the road closely enough for clearance purposes; a rare
    out-of-order scene only makes the check conservative on that stretch.
    """
    blocks = [info_ron[m.end() : balanced_end(info_ron, m.end())] for m in re.finditer(r"points: \[", info_ron)]
    segs = []
    for block in blocks:
        pts = [
            tuple(float(v) for v in m.group(1).split(","))
            for m in re.finditer(r"pos: \(([^)]*)\)", block)
        ]
        widths = [float(w) for w in re.findall(r"width: (\d+)", block)]
        for i in range(len(pts) - 1):
            (ax, ay, az), (bx, by, bz) = pts[i], pts[i + 1]
            half = widths[min(i, len(widths) - 1)] / 2.0 if widths else 5.0
            segs.append((ax, az, bx, bz, half))
    return segs


def segment_distance(px, pz, ax, az, bx, bz) -> float:
    vx, vz = bx - ax, bz - az
    length2 = vx * vx + vz * vz
    t = max(0.0, min(1.0, ((px - ax) * vx + (pz - az) * vz) / length2)) if length2 else 0.0
    return math.hypot(px - (ax + t * vx), pz - (az + t * vz))


def clearance(x, z, segs) -> float:
    return min((segment_distance(x, z, ax, az, bx, bz) - half for ax, az, bx, bz, half in segs), default=1e9)

def forward(pose):
    """Rotate the local forward axis (0, 0, -1) by the pose quaternion."""
    x, y, z, w = pose[0:4]
    fx = 2.0 * (x * z + w * y)
    fz = 1.0 - 2.0 * (x * x + y * y)
    return -fx, -fz

def pick_parking(spawns, segs):
    """Parking pose for a phantom anchored at `spawns[-1]`, or None."""
    anchor = spawns[-1]
    ax, ay, az = anchor[4], anchor[5], anchor[6]
    fx, fz = forward(anchor)
    # Perpendicular (left of facing).
    px, pz = -fz, fx
    xs = [s[0] for s in segs] + [s[2] for s in segs]
    zs = [s[1] for s in segs] + [s[3] for s in segs]
    lo_x, hi_x = min(xs) - EDGE_MARGIN, max(xs) + EDGE_MARGIN
    lo_z, hi_z = min(zs) - EDGE_MARGIN, max(zs) + EDGE_MARGIN

    best = None
    for along in LONGITUDINAL:
        for sign in (1.0, -1.0):
            dist = MIN_OFFSET
            while dist <= MAX_OFFSET:
                x = ax + along * fx + sign * dist * px
                z = az + along * fz + sign * dist * pz
                if lo_x <= x <= hi_x and lo_z <= z <= hi_z:
                    road = clearance(x, z, segs)
                    cars = min(
                        math.hypot(x - s[4], z - s[6]) for s in spawns
                    )
                    if road >= CAR_MARGIN and cars >= SPAWN_CLEAR:
                        score = dist + abs(along)
                        if best is None or score < best[0]:
                            best = (score, [*anchor[:4], x, ay, z])
                        break
                dist += OFFSET_STEP
    return best[1] if best else None


def main() -> int:
    game = Path(sys.argv[1] if len(sys.argv) > 1 else DEFAULT_GAME)
    for name, variant, spawns in load_maps_txt(REPO / "crates/server/maps.txt"):
        # One parking pose per possible client count: the phantom takes the
        # slot after the last racer, so emit one row per grid slot (wrapping
        # like grid_pose does). Rows repeat the same shoulder when slots wrap.
        scene = game / "assets" / "scenes" / name / "info.ron"
        if not scene.exists():
            continue
        segs = path_segments(scene.read_text())
        if not segs:
            continue
        poses = []
        for slot in range(len(spawns)):
            poses.append(pick_parking(spawns[: slot + 1], segs) or spawns[slot])
        if all(p == s for p, s in zip(poses, spawns)):
            continue  # nothing found anywhere; server falls back to the grid
        print(name, variant, len(poses))
        for pose in poses:
            print(" ".join(f"{v:.6g}" for v in pose))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
