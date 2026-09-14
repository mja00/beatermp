# Parity with a real host: what is left, and how to close it

State as of commit `0ed12de` (map variants, race settings, LobbyChangeMap).

## Done

Join handshake, multi-client lobby, garage sync, Ready, leave/timeout, ping,
reliable transport (acks, retransmits, chunking both ways), racing (start,
grid confirm, 20 Hz car state relay, finishes, results, back to lobby, garage
re-sync), late joiners waiting in the lobby, mid-race disconnects, garage
visits (host and other players), map variants, laps/night/rain, map rotation
announced with LobbyChangeMap. All of it byte-checked against captures in
`crates/codec/tests/fixtures/`.

Since then the whole `NetworkEvent` enum has been recovered from the binary
without Ghidra: `tools/re/network_events.py` reads the bincode deserialiser's
dispatch tables and the serde name strings, so every discriminant, its body
shape and (for tuple variants) its real name are known. See
"Recovered: the NetworkEvent enum" below.

## Re-checked against game build 25292963 (2026-09-14 update)

Nothing in the protocol moved. Re-derived from the new binary: the
`NetworkEvent` jump table is still 49 variants `0..=48` with the same shapes
and tuple names; `broadcast_equivalent` (now `0x3e37a0`) still maps exactly
`6->7, 8->9, 11->12, 16->17, 19->20, 25->26, 27->28, 29->30, 31->32, 39->40,
41->42, 43->46, 44->47, 45->48` plus verbatim `0`, i.e. `codec::broadcast_twin`
unchanged; `Server::run` still checks the greeting against `0x101`; the serde
field tables (`CarDeriative` 24, `LocalServerInfo` 6, `AvatarState` 4) are
identical; `tools/re/spawns.py` reproduces `crates/server/maps.txt` byte for
byte. A live join with the updated client (`join_client.sh`) completed the
handshake, lobby, race start, grid confirm and RaceGo, with the same payload
sizes (ClientInfo 49 B, GarageState 448 B, SpawnCar 472 B chunked 450+22).

Only addresses moved, which broke the two RE tools that hard-coded them.
`network_events.py` now resolves `visit_enum`, its jump table and the variant
count from the symbol table and the disassembly; `extract_fields.py` reads the
`R_X86_64_RELATIVE` addends (the binary is a PIE, so the string-table pointers
are zero in the file) instead of anchoring on a literal blob marker. The
listen-address literal is at `0x9b639` in this build, not `0x9bab2`.

## Left

Checklist against a real host, roughly highest value first. Captures in
`crates/codec/tests/fixtures` show which events are reliable: every
client -> host action is reliable except `SyncCarState(11)`,
`UpdateAvatarState(29)`, `Ping(21)`/`Pong(22)`; `SyncCarStateBroadcast(12)`,
`Ping`/`Pong` and `UpdateAvatarStateBroadcast(30)` go out unreliable. The
unattributed events below (8, 19, 25, 27, 36, 37, 38, 39, 40, 43..48, 0, 9, 20,
26, 28) never appear in any capture, so the binary is the only source.

1. **Send mode / ordering -- resolved for captured events.** Parsing the
   fixtures by envelope kind *and* `ordered_index` shows the server's choices
   already match a real host: ordered `1,3,7,10,18`, `17` after a race, `30`,
   `32`, `34`, `35`, `42`; unordered `2,5,13,14,24` and `17` during the
   handshake; unreliable `11,12,21,22`. The decompiler's per-event mode in
   `broadcast_event` (`+0x18`/`+0x20`/`+0x28` plus a trailing bool) does not
   recover cleanly and the bool is *not* the ordered bit (`42` is ordered but
   sent with flag 0), so it was not used. The twins that never appear in a
   capture (8/19/25/27/39/43..48) stay reliable+ordered.

2. **Unattributed variants -- mapped; no server action needed.** The host's
   receive switch in `NetworkManager::update` (`switch(uVar49)`, ~line 2020)
   pairs each received event with what it does:
   `16 CarDeriative -> into_car + insert HashMap<ClientId,Car> -> 17`,
   `19 -> 20` (19 is what `menu::send_car` sends instead of
   `CarDeriative(16)` when the player has no car selected), `21 Ping -> unicast
   22 Pong`, `22 Pong -> ignored`, `23 Disconnect -> remove_client -> 24`,
   `25 -> spawn_player_avatar + insert HashMap<ClientId,AvatarInfo> -> 26`,
   `27 -> 28`, `29 -> 30`, `31 -> 32`, `33 -> Game::form_garage_state_response`,
   `34 -> forwarded to the waiting visitor`, `36 -> a 192-byte
   GarageStateCommit-shaped body, consumed locally (not rebroadcast)`,
   `37 -> unicast 38 to one client`, `39 -> apply_car_event -> 40`,
   `41 -> 42`, `43/44/45 -> shared_fn::cart_start / cart_move / cart_end`.
   So only events in `broadcast_equivalent` are ever broadcast, and `37 -> 38`
   is a *targeted reply* (rightly absent from `broadcast_twin`). The relay
   fallback now mirrors that: it re-tags twin events, forwards only the one
   verbatim event (`broadcast_verbatim`: variant `0`, a `String`), and consumes
   everything else instead of putting a client-role event on the wire. So `36`
   (a client's 192-byte `GarageStateCommit`, consumed by the host's case `0x24`)
   and `37` are dropped rather than relayed. `37`'s unicast `38` reply is not
   implemented: it is an `InputState::action_got_active` on a string near
   "Clear avatars", and neither the action nor `38`'s meaning is pinned.
3. **Host-side rules -- read; the remaining approximations are deliberate for a
   headless host.**
   - RaceEnd: a host sends `3` from `NetworkManager::ui_render` when its player
     presses a key in the results overlay (`hub::render_race_results` +
     `InputState::any_key_down`, which sets the flag `update` consumes). A
     headless host has no player, so the 5 s timer stands.
   - Ready/start: the host's Start Race button calls `menu::lobby_send_cars`;
     no all-ready gate was found on that path. "Start when every joined client
     is Ready" is the server's substitute for the absent button.
   - Late joiners wait in the lobby for the next race (`second_join_host`).
   - Timeout: `Server::run` only re-sends the greeting after 10 s of silence;
     it has no client-timeout constant. The server's 15 s stands.
   - `DisconnectReason` values, from `display` (`0x3d63c0`): `none`, `kick`,
     `timed_out`, `host_left`, `ban`, `player_limit`. `Disconnect(23)` carries
     one as its `u32` body; captures only ever show `0` (a clean quit). The
     server ignores it and always answers `PlayerLeft`, as a host does.
   - Results/money are settled: `score_and_reward` writes money (`+0x938`) and a
     results string (`+0x3d8`) and sends nothing.
4. **`NetworkCarState` -- layout recovered; field names are gone.** The 94-byte
   state is 14 fields. The deserializer (`0x6ca030`) and the captured constant
   together fix the layout: `0..28` pose
   `Isometry<f32, UnitQuaternion<f32>, 3>` (rotation xyzw, position xyz),
   `28..40`/`40..52` `[f32; 3]`, `52..64` three `u32`, `64..76` three `f32`,
   `76..84` `usize`, `84` `bool` (held at the grid), `85` `bool`,
   `86..90` `f32`, `90..94` `f32` (the sender clock; `285.54` in the capture,
   and offset 84 is `1` while held at the grid). serde field names were
   stripped from the binary, so the server keeps positional constants
   (`CAR_STATE_GRID_HOLD`, `CAR_STATE_CLOCK`); the grid flag is now patched as
   the single byte it is, not four.
5. **`handle_unreliable` twins -- done.** Both receive paths now share
   `relay_twin`, which re-tags any twin-bearing client event with the sender's
   `PlayerId` (reliably, matching what the host's `broadcast_equivalent` path
   emits regardless of how the frame arrived); the unreliable fallback only
   relays verbatim when there is no twin. No capture showed such an event
   arriving unreliably, so this was robustness, not a known bug.
6. **CPU opponents / a driving host car -- blocked on the physics, not the AI.**
   The singleplayer AI (`game::ai`) is only the *driver*. `spawn_ai` calls
   `car::spawn_car_raw` and bolts an `Ai` component onto a full `Car` (the
   `EncodedCar` field list carries `ai_parameters`, `drive_wheels`,
   `steering_wheels`, engine/suspension/gears), and `AiController` runs a worker
   thread (`pathfind3`, `calculate_clean_nodes`, `path_postprocess`,
   `is_point_drivable`, `dist_to_finish`, `AiWaypoint`, `SkillLevel`) over an
   `AiSceneInfo` built from the map's waypoint + collision data, fed the local
   player's line through `AiStateUpdate` (see `AiController>::update`). Turning
   its controls into the 94-byte `NetworkCarState` is the vehicle physics
   (`game::car`, `tire::Tire::combined`, suspension, `physics::SceneStaticCollision`),
   which needs baked collision geometry the server has none of. Copying the AI
   alone would not produce CPU opponents; the headless *real client* (Approach 1
   in `README.md`) already hosts them because it runs the real physics. A
   headless approximation would need a baked AI-waypoint graph plus a fabricated
   motion model -- documented approximation, not parity.

## Recovered: the client -> broadcast routing

`NetworkEvent::broadcast_equivalent` (binary `0x4d67a0`) is the switch a real
host uses to turn a received client event into what it sends the rest of the
lobby. It takes the sender's `(client_id, player_index)` and, for each variant
that has a twin, writes `twin_discriminant` followed by the sender's `PlayerId`
and the original body -- exactly `encode_with_sender_disc`. Any variant not in
the switch panics, i.e. the host never broadcasts it.

| received | twin | notes |
|---|---|---|
| 6 Ready | 7 PlayerSetStatusBroadcast | `bool` |
| 8 | 9 | bodyless client request; broadcast is the bare `PlayerId` |
| 11 SyncCarState | 12 SyncCarStateBroadcast | `NetworkCarState` |
| 16 CarDeriative | 17 LobbyChangeCarBroadcast | 536-byte car body |
| 19 | 20 | bodyless |
| 25 | 26 | bodyless |
| 27 | 28 | bodyless |
| 29 UpdateAvatarState | 30 UpdateAvatarStateBroadcast | `AvatarState` |
| 31 UpdateLocation | 32 UpdateLocationBroadcast | location tag |
| 39 CarEvent | 40 CarEventBroadcast | |
| 41 StopGarageVisit | 42 StopGarageVisitBroadcast | |
| 43 PushCartStarted | 46 PushCartStartedBroadcast | |
| 44 PushCartMoved | 47 PushCartMovedBroadcast | |
| 45 PushCartEnd | 48 PushCartEndBroadcast | |
| 0 | 0 | a `String` copied as-is, *without* the `PlayerId` (relayed unchanged) |

Variants 8, 19, 25 and 27 are bodyless client requests whose broadcasts carry
only the sender's id; their serde names are not recoverable (no tuple-name
string), so they are unnamed. Discriminants 9, 20, 26 and 28 are their twins.

`crates/codec::broadcast_twin` encodes this table and
`crates/server/src/main.rs` uses it in the relay fallback, so a client event
with a twin is re-tagged instead of forwarded in its client role. This
supersedes the earlier per-event guesses for `CarEvent` and `PushCartMoved`.

## Recovered: the NetworkEvent enum (49 variants)

`NetworkEvent` is bincode: `u32 discriminant` (the declaration index, `0..=48`)
then the body. Discriminant equals declaration index for every id the captures
show, so the wire ids and the jump table agree. `tools/re/network_events.py`
regenerates the table below from the binary (needs `objdump`/`readelf`/`nm`).

Shape legend: `unit` = no body; `newtype T` = `T`; `struct T (n)` = `T`'s `n`
fields; `tuple(n)` = `n` fields, with the serde name where it is recoverable.

| id | shape | name / notes |
|---|---|---|
| 0 | newtype String | |
| 1 | struct RacePreset (5) | StartRace; body is the preset, not loose words |
| 2 | unit | RaceGo |
| 3 | newtype bool | RaceEnd |
| 4 | tuple(2) | CrossedFinish (`Entity`, `f64`) |
| 5 | tuple(3) | CarCrossedFinish (`PlayerId`, `Entity`, `f64`) |
| 6 | newtype bool | Ready |
| 7 | tuple(2) | **PlayerSetStatusBroadcast** (codec: ReadyBroadcast) |
| 8 | unit | |
| 9 | newtype ClientId | |
| 10 | tuple(4) | **SpawnCarBroadcast** (codec: SpawnCar; first field `CarOwner`) |
| 11 | tuple(2) | **SyncCarState** (codec: CarState; `NetworkCarState`, `Entity`) |
| 12 | tuple(3) | **SyncCarStateBroadcast** (codec: CarStateBroadcast) |
| 13 | tuple(2) | **ClientConnected** (`PlayerId`, `ClientInfo`) |
| 14 | struct ServerInfo (6) | |
| 15 | newtype ClientInfo | |
| 16 | struct CarDeriative (24) | **the 420-byte client car body** (codec: GarageState) |
| 17 | tuple(2) | **LobbyChangeCarBroadcast** (`PlayerId`, car; codec: GarageStateCommit) |
| 18 | tuple(2) | LobbyChangeMap |
| 19 | unit | "no car selected", sent by `menu::send_car` instead of 16 |
| 20 | newtype ClientId | |
| 21 | unit | Ping |
| 22 | unit | Pong |
| 23 | newtype u32 | Disconnect; the `u32` is a `DisconnectReason` |
| 24 | newtype ClientId | PlayerLeft |
| 25 | unit | |
| 26 | struct SerKey (2) | PlayerId-shaped; client -> host |
| 27 | unit | |
| 28 | newtype ClientId | |
| 29 | struct AvatarState (4) | UpdateAvatarState |
| 30 | tuple(2) | UpdateAvatarStateBroadcast |
| 31 | newtype u32 | UpdateLocation (`Location` tag; only `0` from clients) |
| 32 | (PlayerId, Location) | UpdateLocationBroadcast; visitor inlined, name from rodata |
| 33 | struct SerKey (2) | RequestVisitGarage (`PlayerId owner`) |
| 34 | tuple(4) | VisitGarageResponse |
| 35 | (PlayerId, PlayerId) | garage visit broadcast (codec: GarageVisitBroadcast) |
| 36 | struct GarageStateCommit (5) | client -> host; consumed locally (case `0x24`), never broadcast |
| 37 | newtype ClientId | client -> host; host unicasts `38` to one client |
| 38 | newtype ClientId | |
| 39 | tuple(2) | **CarEvent** (horn/lights; first field is `CarEvent`) |
| 40 | tuple(3) | **CarEventBroadcast** |
| 41 | unit | StopGarageVisit |
| 42 | (PlayerId) | StopGarageVisitBroadcast |
| 43 | newtype Entity | PushCartStarted (body from `broadcast_equivalent`) |
| 44 | tuple(2) | **PushCartMoved** |
| 45 | newtype Entity | |
| 46 | tuple(2) | **PushCartStartedBroadcast** |
| 47 | tuple(3) | **PushCartMovedBroadcast** |
| 48 | tuple(2) | **PushCartEndBroadcast** |

`SerKey` is `slotmap`'s key encoding, `(u32 index, u32 version)` -- i.e. a bare
`PlayerId`/`ClientId`. `ClientId` is a newtype over the same pair or a single
`u32` depending on the site; the tool reports whichever monomorphisation the
arm calls.

`tools/re/network_events.py` reports a newtype over an inline scalar as `unit`
(the arm reads the `u32`/`bool` with no call to anchor on), so `23`, `31` and
`43` above are corrected from captures / `broadcast_equivalent` rather than from
the tool. Discriminants and tuple names are unaffected.

The `ClientNetworkEvent` names (`CarResetBroadcast`,
`LobbyMakeSpectatorBroadcast`, `ClientDisconnected`, `EnterCarBroadcast`,
`RequestVisitGarage`, `StopGarageVisitBroadcast`) are a separate enum; only
`RequestVisitGarage` and `StopGarageVisitBroadcast` have an attributed
NetworkEvent discriminant so far (33 and 42).

## Proposal: decompile the host code instead of capturing it

The Linux `beaterCore` binary (`file`: ELF x86-64, **not stripped**) keeps
full Rust symbols, so the host logic is addressable by name. Relevant
functions (`nm -C -S --size-sort beaterCore | rg game::network`):

| Function | Size | Why |
|---|---|---|
| `network_manager::NetworkManager::update` (Module impl) | 68 KB | the event loop; the host's `match NetworkEvent` arms are here |
| `menu::MultiplayerLobby::ui_render` | 28 KB | which lobby buttons send which events |
| `network_manager::NetworkManager::ui_render` | 13 KB | in-race / results UI and what it sends |
| `Server::run` (NetworkServer impl) | 4 KB | socket loop, greeting, timeouts |
| `PacketFactory::process_packet` / `send_data_reliable` | 3 KB / 1 KB | transport, already known from captures |
| `network_manager::post_disconnect`, `remove_client` | 2 KB each | leave handling, `DisconnectReason` |
| `shared_fn::score_and_reward` | 1.4 KB | results and money |
| `menu::lobby_send_cars`, `request_visit_garage`, `spawn_player_avatar` | ~1 KB each | race start, garage visit, avatars |

Type symbols also exist for `events::NetworkEvent`, `CarEvent`, `CarOwner`,
`DisconnectReason`, `Location`, `NetworkCarState`, `AvatarState`,
`BasicPlayerInfo`, `LargePacketBuffer`. bincode does not embed variant names,
but the `NetworkEvent` deserialize `visit_u32` jump table gives the full
discriminant list and `drop_glue::<NetworkEvent>` gives each variant's payload
layout.

Plan:

1. `ghidra` is installed (`/usr/bin/ghidra`). Run `analyzeHeadless` on
   `beaterCore` once, then export decompiled C for `beaterCore::game::network::*`;
   keep the export outside the repo (`/tmp` or `tools/re/out/`, gitignored).
   `tools/re/export_decomp.py` does the export. Decompilation needs PyGhidra, not
   a Java post-script: this Ghidra build has no Java script compiler (no JDT
   bundle), so `-postScript Foo.java` fails with "Failed to get OSGi bundle".
2. ~~Read `NetworkEvent` deserialize to list every discriminant and body
   shape; fill the gaps in the table in `bodies.md`.~~ **Done** without Ghidra:
   `tools/re/network_events.py` recovers all 49 discriminants, shapes and the
   tuple-variant names; see the table above.
3. Read `NetworkManager::update` arm by arm for the host role: for each
   event, who receives the reply (sender, target, everyone else, everyone) and
   what state changes. Compare with `crates/server/src/main.rs::handle_event`.
   **Partly done**: the client -> broadcast transform is fully recovered from
   `broadcast_equivalent` and implemented (`codec::broadcast_twin`); what is
   left is each event's *other* side effects and the reliable/ordered mode it
   is sent with.
4. Read `NetworkCarState` deserialize to name the 94 bytes; replace the
   offset patches in `live_host_state` with named fields. The struct has 14
   fields; the deserialiser monomorphisation is
   `...deserialize_struct::<NetworkCarState ...>` at `0x5ca030`.
5. ~~Read `score_and_reward` and the results path in `ui_render` to confirm the
   host sends nothing the clients do not already compute.~~ **Done**:
   `score_and_reward` (0x641c20) computes the reward locally and writes money
   (`0x938`) and a results string (`0x3d8`); no send. Clients compute their own.
6. Verify each change the usual way: capture fixture where one exists, else a
   live two-client session with `tools/udpsniff/join_client.sh`. The
   `broadcast_twin` table is locked by a unit test.

Rust release output decompiles noisily (inlined hashbrown/hecs, panics
everywhere), but with symbols the network functions are tractable; the
alternative is one staged capture per unknown event. The `update` function is
68 KB / ~16k lines of C and needs `DecompileOptions.setDefaultTimeout` raised
(the default "Response buffer size exceeded" is a timeout, not a real buffer
limit).
