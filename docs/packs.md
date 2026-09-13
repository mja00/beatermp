# BeaterCore packs and data formats

Every container/package format the game reads or writes: mods and Workshop
items, scenes, saves, replays, settings, localization and the data files behind
them. Wire protocols are in `docs/protocols.md`.

Paths use two roots:

- **game dir**: where `beaterCore` is installed (`assets/...`).
- **profile dir**: `$XDG_DATA_HOME/beaterCore` (normally
  `~/.local/share/beaterCore`), holding `settings.json5`, `pbs.ron`, `saves/`,
  `replays/` and `mods/`.

## 1. The mod VFS

Mods are directories, not archives. `Vfs::new` (`0x334bd0`) returns the whole
view:

1. `<base>/mods/` and `<base>/mods/local/` are the mod roots; `mods/local/` is
   created if missing (so a fresh profile gets an empty `mods/local/`).
2. Every immediate subdirectory of `mods/` is a mod: symlinks are skipped,
   non-directories are ignored, and the directory name is the mod key.
3. `<moddir>/description.json5` is parsed as a `ModDescription` (JSON5).
4. `mod_hash(dir)` is computed and a `ModInfo { description, hash, path }` is
   inserted into a `HashMap<String, ModInfo>`, keyed by the directory name.

`Vfs::get_path` and `Vfs::read_dir` resolve a path through the enabled mods and
then the base `./assets/` tree; `Vfs::read_dir_mods_only` lists only the files a
mod contributes (used to enumerate a mod's contents). Enabled/disabled state is
`settings.json5 -> mods_status: { "<mod name>": bool }`.

## 2. Mod pack format

```text
<mods root>/<mod name>/
    description.json5      required; metadata
    <any asset files>      ron / json5 / ogg / glb / png / txt / md ...
    thumbnail.png          Workshop preview only (uploader)
```

`description.json5` (`ModDescription`, via `json5::from_str`):

```json5
{
    title: "My Mod",        // String; shown in the Mods tab
    category: "General",    // General | Map | Vehicle | Music | Localization
}
```

Evidence: `<ModDescription as Serialize>` (`0x31f120`) emits exactly the keys
`title`, `category`; `ModCategory::visit_str` (`0x422a20`) accepts `General`,
`Map`, `Vehicle`, `Music`, `Localization`. A description that fails to parse
logs `Failed to parse mod description!`; a mod with no title falls back to
`Unnamed beaterCore Mod`.

`mod_hash` (`0x3450d0`) is a content hash over the whole mod directory tree:

- every regular file, recursively (directories are traversed, symlinks skipped);
- each file hashed with **FNV-1a 64: `acc = 0; for byte in file: acc = (acc ^
  byte) * 0x100000001b3`** (note the offset basis is `0`, not the usual
  `0xcbf29ce484222325`);
- the per-file hashes are **added together (wrapping u64)** into one value.

Multiplayer advertises mods by `ModName` (a bincode enum, see `docs/protocols.md`
section 2): variant `0` is a bare `String` (local mod name), variant `1` is
`(String, u64)` (name, Workshop published-file id). Only enabled mods are
hashed and sent; the map travels in `ClientInfo`/`ServerInfo`.

## 3. Steam Workshop item pack

A published Workshop item is exactly the mod folder above. The ship-tool
`tools/workshop_uploader/workshop_uploader`:

- sets the item content to the folder (`ISteamUGC_SetItemContent(content_path)`),
  which contains `description.json5` and the assets;
- sets the preview (`SetItemPreview(.../thumbnail.png)`), warning
  `thumbnail.png is missing` if absent;
- takes title/description from `description.json5` and recognises the asset
  extensions `ron json5 ogg glb png txt md`.

On the client, `UGC::item_install_info(id)` locates the installed folder; if it
is missing, `Game::add_workshop_item_mod` (`0x525220`) calls
`download_item(id, high_priority = true)`, then parses
`<install_dir>/description.json5`, hashes the folder and inserts it into the VFS
keyed by the published-file-id string. Subscribe/uninstall events come from the
`ItemInstalled_t` / `UserSubscribedItemsListChanged_t` callbacks.

## 4. Scene pack

```text
assets/scenes/<scene>/
    info.ron               SceneDescription (RON)
    scene.ron              entity list
    height_map.png         terrain heightfield
    texture_map.png        splat weights
    foliage_map.png        foliage density
    rocks_map.png          rock density
    sprites/  terrain_textures/
```

`info.ron` is a `SceneDescription`:

```ron
(
    terrain_description: Some((
        textures: ("terrain_sandy_dirt", "terrain_dirt", "terrain_grass", "terrain_road"),
        height: 150.0,
        grip: (0.7, 0.8, 0.6, 1.1),
        surface_types: (Dirt, Dirt, Grass, Asphalt),
        foliage_texture: "foliage_summer_01",
    )),
    fog: (color: (0.4, 0.6, 0.6, 0.5), density: 0.01),
    paths: {
        Default: (
            points: [
                (pos: (68.0, 1.0, 227.0), next: [1], attached_checkpoint: Some(4294968880),
                 ai_speed: 1.0, ai_max_speed: 0.0, width: 10),
                ...
            ],
        ),
        // more VariantName keys: Reverse, Alternative, TimeAttack, TimeAttackReverse
    },
    water: None,
)
```

- `paths` is keyed by `VariantName`: `Default`, `Reverse`, `Alternative`,
  `TimeAttack`, `TimeAttackReverse`. On the wire and in the UI these are 1-based
  (`1..5`); `StartRace`/`LobbyChangeMap`/`ServerInfo` carry the number.
- The deserialiser also accepts `terrain_description`, `checkpoint_order`,
  `max_players`, `fog`, `path`, `paths`, `water` (7 fields; visitor `0x5180d0`).
  `checkpoint_order`, `max_players` and `path` are omitted from the shipped
  `forest_long/info.ron`, so they are optional/defaulted.
- `tools/re/spawns.py` reads these `paths[Variant].points[].pos` values to build
  `crates/server/maps.txt` (grid poses per variant), which is why `beatermp`
  needs no game install.

`scene.ron` is the static-entity dump:

```ron
(
    entities: [
        (
            components: [
                Transform((isometry: (rotation: (x,y,z,w), translation: (x,y,z)), scale: None)),
                PresetName(("sprite_bush_dry")),
                Impostor((texture: "sprite_bush_dry", scale: 1.0)),
            ],
            id: 4294968576,
        ),
        ...
    ],
)
```

Garage interiors are scenes too (`assets/scenes/garages/{info.ron,scene.ron}`),
and the in-game map editor reads/writes the same pack under `./scenes/`
(`Create` / `Create New Variant` in the editor UI).

## 5. Save game and personal bests

- `saves/<name>.ron` and `saves/autosave.ron` are RON `SaveFile`s. `autosave.ron`
  is written at shutdown and loaded at boot.
- `SaveFile` has 25 fields; order recovered from the RON field visitor
  (`0x518510`) and confirmed by the keys present in a real `autosave.ron`:

  | # | field | # | field |
  |---|---|---|---|
  | 0 | garages | 13 | progress |
  | 1 | players_held_item | 14 | event_place |
  | 2 | delivery_items | 15 | unlocked_rewards |
  | 3 | garbage_items | 16 | leaderboards |
  | 4 | garage_items | 17 | tutorial_state |
  | 5 | furniture | 18 | post_tutorial_tips |
  | 6 | entities | 19 | npc_relationships |
  | 7 | purchased_cars_today | 20 | quest_states |
  | 8 | purchased_items_today | 21 | sponsor_state |
  | 9 | selected_car | 22 | npc_encounter |
  | 10 | time | 23 | story_point |
  | 11 | money | 24 | game_completed |
  | 12 | stage | | |

- `pbs.ron` is a separate `PersonalBests { global_pbs: { "<scene>": [PersonalBest] },
  car_pbs: { "<car>": [PersonalBest] } }`. A `PersonalBest` observed on disk is
  `( time: <seconds>, )`; the deserialiser also knows a `score` field name.
- `leaderboards` in the save is `HashMap<LeaderboardDay, HashMap<LeaderboardParticipant,
  LeaderboardEntry>>` (`LeaderboardDay { event, day }`).

## 6. Replay pack

Replays are bincode, written to `<profile>/replays/<name>.replay`
(`Replay::save`, `0x62b340`; it creates `replays/` and forces the extension to
`replay`). The default name template is `replay_save%d_%m_%Y_%H_%M`; the save
prompt is `press_x_to_save_replay`.

`Replay` (4 fields, from `<Replay as Serialize>`, `0x6269d0`):

```text
[String: <name/map>]
[Vec<(String, ReplayBuffer)>]                 # per-car buffers
[Vec<(f64, (String, Isometry<f32>, String))>] # timed car transforms
[u32]
```

`ReplayBuffer` (4 fields, `0x626880`):

```text
[u8; 4]                       # flags (serialised as four bytes)
[f64]                         # time
[Vec<(f64, CarState)>]        # position samples
[Vec<(f64, ReplayAction)>]    # input/action samples
```

`ReplayAction` is an enum (e.g. `ChangeTire` is a 3-element tuple variant);
`ReplayError` values include `replay_error_missing_map` and
`replay_error_unable_to_decode`. Note this `CarState` is `game::replay::CarState`
(9 fields), distinct from the network `NetworkCarState`.

## 7. Settings

- `settings.json5` (JSON5) is `GameSettingsV0018`; the shipped/edited keys are:

  `file_version, auto_gear_change, auto_clutch, steering_aid, particles,
  volume, music_volume, pacenotes_volume, ui_volume, checkpoint_volume,
  controls {}, fullscreen, vsync, dithering { pixel_scale,
  dithering_intensity }, camera_smoothing, player_name, mouse_sensitivity,
  language, fps_limit, disable_wishlist_reminder, map_movement_tooltip,
  difficulty, foliage_density, foliage_render_distance, terrain_render_distance,
  antialiasing, render_scale, anisotropy_filtering_samples, character_fov,
  hood_camera_fov, orbit_camera_fov, lighting_quality, mods_status {},
  invert_y, invert_x, display_crosshair`.

- `settings.ron` is the legacy name (still referenced in strings alongside
  `settings.json5`); the JSON5 file is current.

## 8. Localization

`assets/en_US.ron` is the built-in default; extra languages live in
`assets/localization/{es_ES,it_IT,pl_PL,ru_RU}.ron`:

```ron
(
    language_name: "English",
    strings: {
        "abandon_race": "Abandon race",
        "abandon_race_1st": "Are you sure you want to {}?",   // {} = format slot
        ...
    },
)
```

`{}` is the runtime substitution slot. Paths `localization/*` and
`strings.json5` appear in the VFS for overrides; the `en_US.ron` strings use
`#[icon_*]` markers for inline button glyphs.

## 9. Other shipped data

- `assets/cars/<car>/car.ron` — `CarDefinition` RON, preceded by
  `#![enable(unwrap_variant_newtypes)]`. Top-level keys include `engine`
  (`flywheel_mass, flywheel_radius, linear_friction, power_mod, torque_curve
  { points: [(rpm, torque)] }, upshift_point, downshift_point,
  constant_friction, displacement`), `suspension` (array of `{ travel,
  stiffness, damping, frame_point }`), `wheels` (`Option<WheelBase>` x4),
  `spawn_height, mass, gear, gears, drive_wheels, steering_wheels,
  handbrake_wheels, suspension_links, hood_pos, engine_pos, exhaust_pos,
  collider, collider_offset, trunk_pos, name, base_price,
  unlocked_by_finishing_game, colors, ai_parameters`. See
  `assets/cars/zam_kitten/car.ron`.
- `assets/campaign_events_{1400,1600,1800,2000}.json5` — JSON5 campaign
  definitions: event name -> `{ days: [ { races: [ { map, laps } ], reward } ],
  opponents: [ { FromName } ], unlocked_by, class, map_pos, ... }`.
- `assets/dialogs/*.json5` — JSON5 dialogue graphs:
  `{ states: { <state>: { response: "<loc key>", options: [ ["<loc key>",
  { SwitchState: "<state>" } ] ] } } }`.
- `assets/entities`, `assets/models`, `assets/textures`, `assets/livery`,
  `assets/rims`, `assets/particles`, `assets/decals`, `assets/brushes`,
  `assets/foliage` — plain file trees referenced by the RON/JSON5 data.
- Standard container formats (no custom packing/compression anywhere):
  `.glb` (glTF 2.0 binary), `.ogg` (Vorbis), `.png`, GLSL `.vert`/`.frag` with
  compiled `.spv` SPIR-V, `.ttf`/`.otf` fonts.

## 10. Open questions

- **`Replay` field meanings.** The field shapes and order are proven by the
  serialiser, but field 0 (a lone `String`), `ReplayBuffer`'s leading 4 bytes
  and `Replay`'s trailing `u32` have no recovered names or observed non-empty
  samples. No `.replay` file was present on this machine.
- **`PersonalBest` fields.** Only `time` appears in the on-disk `pbs.ron`; the
  recovered `score` name belongs to the same data family but was not observed.
- **Mod VFS overlay precedence.** That mods are directories with a
  `description.json5` and a content hash is proven; the exact per-path
  precedence against `./assets/` (mods-win vs. first-match) was not traced
  through `Vfs::get_path`.
- **There is no `.pak`/archive format** anywhere in the install: every asset is a
  loose file, and the only "pack" concept is the mod/Workshop folder.

## 11. Evidence index

| Format | Evidence |
|---|---|
| Mod dirs, description.json5, mod_hash | `Vfs::new` `0x334bd0`, `mod_hash` `0x3450d0`, `read_dir_mods_only` `0x434850` |
| ModDescription title/category | `0x31f120`, `0x422a20`; string `./mods/description.json5` |
| Workshop install | `Game::add_workshop_item_mod` `0x525220`; uploader strings |
| Scene info.ron | `assets/scenes/forest_long/info.ron`; visitor `0x5180d0` |
| scene.ron | `assets/scenes/forest_long/scene.ron` |
| SaveFile 25 fields | visitor `0x518510`; `~/.local/share/beaterCore/saves/autosave.ron` |
| PersonalBests | visitor `0x3f5780`; `~/.local/share/beaterCore/pbs.ron` |
| Replay | `Replay::save` `0x62b340`; serialise `0x6269d0`, `0x626880` |
| Settings | `assets`/profile `settings.json5`; rodata `GameSettings` fields |
| Localization | `assets/en_US.ron`; `assets/localization/` |
| CarDefinition | `assets/cars/zam_kitten/car.ron` |
