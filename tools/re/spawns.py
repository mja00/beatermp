#!/usr/bin/env python3
"""Extract per-map grid spawn poses from the game's scene files.

Writes the table `beatermp` bakes in (`crates/server/maps.txt`): one
`map <count>` header per raceable scene followed by `<count>` lines of
`rx ry rz rw x y z`, in the order the scene lists its `SpawnPoint` entities.
A scene is raceable if its `info.ron` has a path with checkpoints and the
scene has at least two spawn points.

Usage: spawns.py [game_dir] > crates/server/maps.txt
"""

import re
import sys
from pathlib import Path

DEFAULT_GAME = "/mnt/data-drive/SteamLibrary/steamapps/common/BeaterCore"

TRANSFORM = re.compile(
    r"rotation: \(([^)]*)\),\s*translation: \(([^)]*)\),\s*\),\s*scale: None,\s*\)\),"
    r"\s*PresetName\(\(\"Spawn Point\"\)\),\s*SpawnPoint\(\(\)\)",
)
CHECKPOINTS = re.compile(r"checkpoint_order: \[\s*\d+")


def main() -> int:
    game = Path(sys.argv[1] if len(sys.argv) > 1 else DEFAULT_GAME)
    scenes = sorted(p for p in (game / "assets" / "scenes").iterdir() if p.is_dir())
    for scene in scenes:
        info = scene / "info.ron"
        ron = scene / "scene.ron"
        if not info.exists() or not ron.exists():
            continue
        if not CHECKPOINTS.search(info.read_text()):
            continue
        spawns = TRANSFORM.findall(ron.read_text())
        if len(spawns) < 2:
            continue
        print(scene.name, len(spawns))
        for rotation, translation in spawns:
            values = [v.strip() for v in (rotation + "," + translation).split(",")]
            print(" ".join(values))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
