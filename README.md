# beatermp

Hosting a BeaterCore (Steam AppID 3711050) multiplayer lobby without a player
sitting at the host machine. Two approaches, both worked out from the shipped
Linux binary and live packet captures:

1. **Headless real client** (works today, including races): run the game under
   a virtual X display, click through Host Multiplayer once, leave it running.
2. **`beatermp`**, a standalone UDP server in this repo that speaks the game's
   protocol (lobby, garage visits and racing, with a phantom host car).

## What the game gives you

Nothing. There is no dedicated server binary, no `--server`/`--headless`
argument (the only recognised argv are `vulkan` and `validation`), no
`SteamGameServer_Init` import and no Steam "Dedicated server" category. The
built-in server browser talks to `209.250.240.105:4321`, which is dead, so
players must use **Connect by IP**. Multiplayer is a plain UDP listen server on
`0.0.0.0:6237` opened by whichever client hosts.

## Approach 1: headless real client

Requirements: the game files, `Xvfb`, `xdotool`, ~600 MB RAM and ~1.3 cores
per host. Steam is **not** required: if `~/.steam/sdk64/steamclient.so` is not
reachable from `$HOME` the game skips Steam entirely and still hosts LAN games.

```sh
Xvfb :99 -screen 0 1280x720x24 -nolisten tcp -ac &

cd /path/to/steamapps/common/BeaterCore
env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
    DISPLAY=:99 SDL_VIDEODRIVER=x11 \
    XDG_DATA_HOME=/srv/beatercore-host \
    LD_LIBRARY_PATH=. SteamAppId=3711050 \
    ./beaterCore &
```

Notes:

- `SDL_VIDEODRIVER=dummy` does not work; winit panics without an X server, so
  Xvfb is mandatory. Run it on a display nobody else uses.
- `XDG_DATA_HOME` isolates the host profile. Seed
  `$XDG_DATA_HOME/beaterCore/` with `settings.json5` (sets `player_name`, the
  name shown over the host car), `pbs.ron`, `saves/` and `mods/local/` or
  first-run dialogs eat the scripted clicks. `tools/udpsniff/seed/` is a
  working seed.
- Wait until the window exists and `beaterCore_log.txt` contains
  `Steam Input: Available gamepads`, then drive the UI with `xdotool`
  (`windowfocus --sync`, `mousemove --window`, `click 1`): Host Multiplayer,
  tick LAN, Create server. `tools/udpsniff/capture_session.sh` does exactly
  this for a host on `:99` and a joining client on `:98`, and is the reference
  for the click coordinates (960x600 window).
- Forward **UDP 6237** to the machine. To change the port, patch the sole
  occurrence of the literal `"0.0.0.0:6237"` in `beaterCore` in place (same
  length, e.g. `"0.0.0.0:6238"`). Its file offset moves with every game build
  (`0x9b639` in build 25292963), so search for the string rather than seeking.
- The game ignores SIGTERM; stop it with `kill -9`.

The host is a normal player: it appears in the lobby, must tick Ready, and
someone must press Start Race on it. Automating that is more `xdotool`.

## Approach 2: `beatermp`

```sh
cargo build --release
./target/release/beatermp [--port 6237] [--name beatermp] [--map forest_long[:reverse][@3]]... [--laps 1] [--night] [--rain]
./target/release/beatermp --list-maps
```

Players Connect by IP to the port. The server shows up in the lobby as a
player named after `--name` (with a stock car); this is unavoidable because a
client stalls at "Connecting" unless the host entry and its garage are present.

Works: join handshake, multiple clients, garage sync between clients, Ready
toggles, leave (Disconnect and 15 s timeout both remove the player from other
lobbies), ping/pong, reliable delivery with acks and 1 Hz retransmits, and
racing: the race starts as soon as every joined client has ticked Ready (there
is no Start Race button on a client), each client's car state is relayed to
the others at 20 Hz, the countdown runs once everyone has confirmed at the
grid, finishes are relayed, and the race ends (results notepad, back to the
lobby, garage re-sync) once everyone has finished, 180 s after the first
finish, or when the lobby empties. Races can be run back to back; a racer who
disconnects mid-race is dropped from the finish count, and a client that joins
mid-race waits in the lobby until the next race. Garage visits work too:
"Visit garage" under the phantom host shows its stock garage, under another
player it fetches that player's garage through the server, and the visitor's
avatar, location and return to the hub are relayed to the others.

Maps: `--map` picks any scene and variant the game can race (`name`,
`name:variant` or `name:variant@laps`, variants being default, reverse,
alternative, timeattack and timeattackreverse where the scene has them).
Repeat it for a rotation that advances in that order after each race, each
entry with its own lap count (`@laps`) or the `--laps` default. Grid poses
come from `crates/server/maps.txt`, which `tools/re/spawns.py` extracts per
variant from the game's `scene.ron` files, so the server needs no game
install. After each race the server announces the next track with
LobbyChangeMap, exactly as a real host's Change Map does, so the lobby minimap
follows the rotation. `--laps`, `--night` and `--rain` set the race settings a
real host picks in its Race Settings panel.

Chat (`T` in the lobby or the race): lines are relayed between players as a
real host does, and the server posts its own in the game's system colour: the
upcoming track and lap count when a player joins and after every race, and
each finisher's place and time. Lines starting with `!` are commands:

| command | effect |
|---|---|
| `!maps` | the rotation with numbers and lap counts; `>` marks the upcoming race |
| `!next` | the upcoming race |
| `!vote N`, `!vote <map>` | vote for the next map by number or a unique fragment of its name |
| `!vote` | the current tally |
| `!help` | this list |

Votes are announced to everyone. A majority of the lobby switches the track at
once while nobody is racing; otherwise the leader (ties to the earlier entry)
replaces the rotation's next entry when the race ends, and the rotation
continues from there. Votes reset whenever the track changes and die with the
player who cast them.

Limits: the host car is a parked phantom. Clients take the front grid slots;
the host parks on a shoulder beside the next slot, computed by
`tools/re/parking.py` from the scene's path waypoints and widths and baked
into `crates/server/parking.txt`, so it is visible but off the racing line.
Eight scenes (`oval`, `shore_fun`, `short3`, `swamp`, `test_finish`,
`winter`, `winter2`, `winter_tiny`) have no clear shoulder within 60 m and
fall back to the next grid slot, where racers may encounter it on later laps.
The phantom keeps normal grid height: parking its chassis underground caused
detached wheels, NaN client physics and immediate zero-time finishes.
A client requires the host car to be present. Its car state is a captured
constant with the pose, grid flag and clock
patched live, and it "finishes" 1 ms behind the last real finisher so the
results table has no empty row. CPU opponents are not supported: a real host
spawns them as extra SpawnCars (leading `u32 1`) and simulates them itself,
which needs the game's physics.

## In-game Server list (Steam)

The game's Server list is a Steam Matchmaking lobby list; the dead
`209.250.240.105:4321` master is only for room codes and NAT punch. To make a
`beatermp` host discoverable, `beatermp-steam` (`crates/steam`) creates a
public or friends-only lobby as AppID 3711050 plus a P2P listen socket, then
relays Steam peers to a local `beatermp`, translating between the game's Steam
transport (raw `NetworkEvent`s, no `Frame` envelope) and beatermp's UDP frames:

```sh
./target/release/beatermp --name "My Server" &
cargo run -p beatermp-steam --features steam -- --name "My Server" --data-file players.jsonl
```

It needs the Steam client running and logged in with an account that owns
BeaterCore (the initialisation is opt-in; the default workspace build is
Steam-free). The bridge's `--name` is the lobby's `name` key, which is the only
lobby data the client reads: the Server list renders `{name} {members}/{limit}`
and falls back to `Unnamed server` when the key is missing. `beatermp`'s own
`--name` is unrelated, it names the phantom host player. `--data-file` appends
one JSON line per lobby/identity event: lobby creation, connects and
disconnects, and each client's name and enabled-mod count.

A client that clicks the entry is sent to `ConnectP2P(lobby owner)`, so **the
joining game must run on a different Steam account than the bridge**: Steam
returns an invalid connection handle for a P2P connect to your own identity,
and the game reports "Connection error: Unknown" without the bridge ever
seeing a connection request. Nothing has to be forwarded on the router, the
traffic rides Steam's relays. The recovered lobby-list and transport contract,
and what has been verified live, is in `docs/notes/steam-browser.md`.

## Protocol

Everything is bincode-style little endian. Strings are `u64 len` + UTF-8.
Reverse engineered from `LD_PRELOAD` captures (`tools/udpsniff`) and the
serde field-name tables in the binary (`tools/re/extract_fields.py`).
Implemented in `crates/codec`, pinned by round-trip tests over the captures in
`crates/codec/tests/fixtures`.

Datagram kinds (first byte):

| kind | name | layout |
|---|---|---|
| 0 | Reliable | `u64 len`, payload, `u32 seq`, `u8 resend`, `u64 sent_at` (unix s), `Option<u32> ordered_index`, `Option<Chunk{u32 id, u32 offset, u32 total, u16 count}>` |
| 1 | Unreliable | payload |
| 2 | Ack | `u32 seq` |
| 3 | Greeting | `u32 257`, echoed by the host |

Each side numbers its Reliable packets from 1 per peer, acks every one it
receives, and retransmits about once a second (bumping `resend`) until acked.
Payloads over 450 bytes are chunked.

Payloads are `NetworkEvent`s: `u32` discriminant then body. `PlayerId` is
`(u32 client_id, u32 player_index)`; the host is `(0xffffffff, 1)`, joiners
count from `(1, 1)`.

| id | event | direction | body |
|---|---|---|---|
| 0 | Chat | both, relayed verbatim | `string`; a client sends `"{name}: {text}"`, `#{RRRGGGBBB}`...`#{RES}` colours a span |
| 1 | StartRace | host -> all | map name, `u32 0`, `u32 laps`, `u32 night`, `u32 rain`, `u32 variant` (ordered) |
| 2 | RaceGo | host -> all | empty; the host's own grid confirm, starts the countdown |
| 3 | RaceEnd | host -> all | `u8 1`; everyone to the results notepad, then the lobby (ordered). Name inferred, not recovered |
| 4 | CrossedFinish | client -> host | `u32 entity`, `u32 generation`, `f64 race time` |
| 5 | CarCrossedFinish | host -> others | `PlayerId`, `u32 entity`, `u32 generation`, `f64 race time` (unordered) |
| 6 | Ready | client -> host | `u8`; both the lobby checkbox and the "press any button" grid confirm |
| 7 | ReadyBroadcast | host -> all | `PlayerId`, `u8` (ordered) |
| 10 | SpawnCar | host -> all | `u32 0`, `PlayerId`, `u32 entity`, `u32 generation`, garage body, `[f32; 7]` pose (rotation xyzw, position); 472 bytes, chunked |
| 11 | CarState | client -> host | 94-byte state, `u32 entity`, `u32 generation`; unreliable, ~20 Hz |
| 12 | CarStateBroadcast | host -> all | `PlayerId owner`, `u32 entity`, `u32 generation`, 94-byte state; unreliable, ~20 Hz |
| 13 | PlayerJoined | host -> lobby | `PlayerId`, ClientInfo body |
| 14 | ServerInfo | host -> joiner | `Vec<(PlayerId, name, [u8;5] avatar)>`, `PlayerId applicant`, `PlayerId host`, map, `u32 variant`, `Vec<String> mods` |
| 15 | ClientInfo | client -> host | name, `HashMap<ModName, u64>` of enabled mods (the `u64` is the empty-map count in every capture) |
| 16 | GarageState | client -> host | 420-byte car description |
| 17 | GarageStateCommit | host -> client | `PlayerId owner`, GarageState body |
| 18 | LobbyChangeMap | host -> lobby | map name, `u32 variant` (1 Default, 2 Reverse, 3 Alternative, 4 TimeAttack, 5 TimeAttackReverse); ordered |
| 21 | Ping | both | `f32` clock, unreliable, 1 Hz |
| 22 | Pong | both | `f32` echo |
| 23 | Disconnect | client -> host | `u32 0` |
| 24 | PlayerLeft | host -> all | `PlayerId` |
| 29 | UpdateAvatarState | client -> host | 48-byte avatar pose ending in the clock; unreliable, ~100 Hz while visiting a garage |
| 30 | UpdateAvatarStateBroadcast | host -> others | `PlayerId`, avatar pose; reliable, ~20 Hz |
| 31 | UpdateLocation | client -> host | `u32 location`, only `0` seen (hub/lobby) |
| 32 | UpdateLocationBroadcast | host -> others | `PlayerId`, `u32 location`; the host emits `2, PlayerId owner` when a player enters a garage |
| 33 | RequestVisitGarage | client -> host | `PlayerId owner`; a client answers any request it receives |
| 34 | VisitGarageResponse | owner -> visitor via host | 32-byte prefix, garage body, 240-byte garage scene, `PlayerId owner`, 8 zero bytes; 712 bytes, chunked. Does not name the visitor |
| 35 | (garage visit broadcast) | host -> others | `PlayerId visitor`, `PlayerId owner` |
| 41 | StopGarageVisit | client -> host | no body; visitor left for the hub |
| 42 | StopGarageVisitBroadcast | host -> others | `PlayerId visitor` |

Join sequence as a real host does it:

1. Client sends Greeting; host echoes it.
2. Client sends ClientInfo. Host answers with ServerInfo (joined clients first,
   itself last, applicant = the joiner's new id), then one GarageStateCommit
   per listed player, and sends PlayerJoined to everyone already in the lobby.
3. Client sends GarageState. Host commits it to the rest of the lobby; the
   client is now in.
4. Leaving: client sends Disconnect, host sends PlayerLeft to everyone.

Race sequence as a real host does it (`bccap` captures with the host confirming
last):

1. Host presses Start Race: ReadyBroadcast(host), StartRace, then one SpawnCar
   per participant, host first, all ordered.
2. Every client streams CarState from the moment its car exists; the host
   rebroadcasts each as CarStateBroadcast to the other clients.
3. At the grid each client presses a button and sends Ready(true) again; the
   host relays it as ReadyBroadcast. Clients show "Waiting for other players"
   until the host itself confirms, which sends ReadyBroadcast(host) on the
   ordered stream followed by RaceGo on the unordered one. `beatermp` does that
   as soon as the last client confirms.
4. A car crossing the finish sends CrossedFinish; the host relays it to the
   other clients as CarCrossedFinish (its own finish included). Clients show
   "Waiting for other players to finish" until then.
5. When the host player leaves the finish overlay it sends RaceEnd and a fresh
   GarageStateCommit of its garage, both ordered; every client answers with its
   worn GarageState. `beatermp` sends RaceEnd 5 s after the last finish.

Garage visit, as a real host does it: the visitor sends RequestVisitGarage;
the host answers with a VisitGarageResponse for its own garage (or forwards
the request to the owner and relays the owner's response back), then tells
everyone else with event 35 and an UpdateLocationBroadcast placing the visitor
in that garage. While visiting, the client streams UpdateAvatarState, which
the host re-tags as UpdateAvatarStateBroadcast for the others. Leaving for the
hub sends UpdateLocation(0) and StopGarageVisit, both re-tagged with the
sender's `PlayerId`.

`docs/notes/bodies.md` has the byte-level walk of the GarageState body;
`docs/notes/parity.md` lists what still differs from a real host and how to
close it.

Full protocol coverage, including the parts this server does not implement
(master-server rendezvous on `209.250.240.105:4321`, NAT punch-through, Steam
NetworkingSockets/Matchmaking, Workshop/UGC, Steam Input and Cloud), is in
`docs/protocols.md`. Every file format the game reads or writes -- mods and
Workshop items, scenes, saves, personal bests, replays, settings and
localization -- is in `docs/packs.md`.

## Tools

- `tools/udpsniff/udpsniff.c`: `LD_PRELOAD` shim logging every `sendto`/
  `recvfrom` as `<ns> <S|R> <fd> <len> <peer> <hex>` to `$BC_SNIFF_LOG`.
  Build with `gcc -O2 -shared -fPIC -o udpsniff.so udpsniff.c -ldl`.
- `tools/udpsniff/capture_session.sh <outdir>`: real host on `:99` plus a
  joining client on `:98`, both captured.
- `tools/udpsniff/join_client.sh <outdir> [host:port]`: one real client under
  Xvfb (`BC_DISPLAY`) that connects by IP; used to test `beatermp`.
- `tools/re/frames.py <capture> [--fd N] [--no-hb] [--kinds]`: annotated frame
  listing of a capture.
- `tools/re/extract_fields.py <binary>`: recovers serde field-name tables.
- `tools/re/network_events.py <binary>`: recovers every `NetworkEvent` variant
  from the binary -- discriminant, body shape and tuple-variant name (see
  `docs/notes/parity.md`).
- `tools/re/export_decomp.py`: decompiles chosen functions out of a Ghidra
  project via PyGhidra (the Java post-script route does not work in this build);
  usage and the one-time venv setup are in the file header.
- `tools/re/spawns.py [game_dir] > crates/server/maps.txt`: regenerates the
  baked per-variant grid-pose table.

Development: `cargo test -p beatermp-codec`, `cargo clippy --release --all-targets`.
