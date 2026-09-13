# beatermp

Hosting a BeaterCore (Steam AppID 3711050) multiplayer lobby without a player
sitting at the host machine. Two approaches, both worked out from the shipped
Linux binary and live packet captures:

1. **Headless real client** (works today, including races): run the game under
   a virtual X display, click through Host Multiplayer once, leave it running.
2. **`beatermp`**, a standalone UDP server in this repo that speaks the game's
   protocol (lobby only: join, garage sync, ready toggles, leave).

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
- Forward **UDP 6237** to the machine. To change the port, patch the literal
  `"0.0.0.0:6237"` at file offset `0x9bab2` in `beaterCore` in place (same
  length, e.g. `"0.0.0.0:6238"`).
- The game ignores SIGTERM; stop it with `kill -9`.

The host is a normal player: it appears in the lobby, must tick Ready, and
someone must press Start Race on it. Automating that is more `xdotool`.

## Approach 2: `beatermp`

```sh
cargo build --release
./target/release/beatermp [--port 6237] [--name beatermp] [--map forest_long]...
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
finish, or when the lobby empties. Races can be run back to back.

Maps: `--map` picks any scene the game can race (repeat it for a rotation that
advances after each race). Grid poses come from `crates/server/maps.txt`, which
`tools/re/spawns.py` extracts from the game's `scene.ron` files, so the server
needs no game install. Only StartRace carries the map name, so after a rotation
the lobby keeps showing the previous minimap until the next race loads.

Limits: the host car is a parked phantom on grid slot 0 (a client only accepts
a race with a host car present) that "finishes" with the last real finisher's
time so the results table has no empty row; race settings and variant are the
captured defaults (one lap).

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
| 1 | StartRace | host -> all | map name, `u32 0 1 0 0 1` race settings (ordered) |
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
| 15 | ClientInfo | client -> host | name, `u64 0` |
| 16 | GarageState | client -> host | 420-byte car description |
| 17 | GarageStateCommit | host -> client | `PlayerId owner`, GarageState body |
| 21 | Ping | both | `f32` clock, unreliable, 1 Hz |
| 22 | Pong | both | `f32` echo |
| 23 | Disconnect | client -> host | `u32 0` |
| 24 | PlayerLeft | host -> all | `PlayerId` |

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

`docs/notes/bodies.md` has the byte-level walk of the GarageState body.

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
- `tools/re/spawns.py [game_dir] > crates/server/maps.txt`: regenerates the
  baked grid-pose table.

Development: `cargo test -p beatermp-codec`, `cargo clippy --release --all-targets`.
