#!/usr/bin/env python3
"""Extract per-map, per-variant grid spawn poses from the game's scene files.

Writes the table `beatermp` bakes in (`crates/server/maps.txt`): one
`<map> <variant> <count>` header per raceable path followed by `<count>` lines
of `rx ry rz rw x y z`, in the order the scene lists its `SpawnPoint`
entities. A path is raceable if `info.ron` gives it checkpoints and the scene
has at least two spawn points for it.

Spawn points live either in the scene's shared `entities` list or, when the
scene has `variant_entities`, under the section named after the path variant
(Default, Reverse, TimeAttack, ...). Sections are not in variant order, so the
name must be honoured rather than taking the first spawns in the file; a
variant section without spawn points shares the scene's.

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
PATH = re.compile(r"^        ([A-Za-z]+): \($", re.M)
CHECKPOINTS = re.compile(r"checkpoint_order: \[\s*\d+")


def sections(scene_ron: str) -> dict[str, str]:
    """Split scene.ron into the shared `entities` text and one text per
    `variant_entities` section, keyed by section name ("" for shared)."""
    out: dict[str, list[str]] = {}
    current = None
    for line in scene_ron.splitlines():
        if line == "    entities: [":
            current = ""
        elif line.startswith("        ") and line.endswith(": [") and current is None:
            current = line.strip()[:-3]
        elif line in ("    ],", "        ],") and current is not None:
            current = None
            continue
        if current is not None:
            out.setdefault(current, []).append(line)
    return {k: "\n".join(v) for k, v in out.items()}


def path_variants(info_ron: str) -> list[tuple[str, str]]:
    """`(variant, body)` for each entry in `paths`, in file order. Older
    scenes have no `paths` map, just one unnamed path: call it Default."""
    starts = [(m.start(), m.group(1)) for m in PATH.finditer(info_ron)]
    if not starts:
        return [("Default", info_ron)]
    out = []
    for i, (at, name) in enumerate(starts):
        end = starts[i + 1][0] if i + 1 < len(starts) else len(info_ron)
        out.append((name, info_ron[at:end]))
    return out


def main() -> int:
    game = Path(sys.argv[1] if len(sys.argv) > 1 else DEFAULT_GAME)
    scenes = sorted(p for p in (game / "assets" / "scenes").iterdir() if p.is_dir())
    for scene in scenes:
        info = scene / "info.ron"
        ron = scene / "scene.ron"
        if not info.exists() or not ron.exists():
            continue
        parts = sections(ron.read_text())
        shared = TRANSFORM.findall(parts.get("", ""))
        for variant, body in path_variants(info.read_text()):
            if not CHECKPOINTS.search(body):
                continue
            spawns = TRANSFORM.findall(parts.get(variant, "")) or shared
            if len(spawns) < 2:
                continue
            print(scene.name, variant, len(spawns))
            for rotation, translation in spawns:
                values = [v.strip() for v in (rotation + "," + translation).split(",")]
                print(" ".join(values))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
