# BeaterCore protocols

Every wire protocol BeaterCore speaks, with the evidence each claim rests on.
The game is the Linux `beaterCore` binary (Steam AppID 3711050); addresses are
file virtual addresses in that binary, so they resolve directly with
`objdump -d`, `nm -C`, or the Ghidra project in `tools/re/export_decomp.py`.

Companion documents: `docs/packs.md` (file formats), `README.md` (the
`beatermp` server), `docs/notes/parity.md` and `docs/notes/bodies.md` (the
byte-level history of the UDP game protocol).

## 1. Transports at a glance

| Transport | Where | Purpose |
|---|---|---|
| UDP game protocol | host, `0.0.0.0:6237` | lobby, garage visits, racing (all `NetworkEvent`s) |
| Master server rendezvous | `209.250.240.105:4321` UDP | host registration/keepalive, room-code lookup, NAT punch-through |
| Steam NetworkingSockets (P2P) | Steam | same `NetworkEvent` payloads over Steam's relay/NAT |
| Steam Matchmaking | Steam | lobby create/join/list, lobby data, friend invites |
| Steam Workshop / UGC | Steam | mod discovery, subscribe, download, publish |
| Steam Input | local Steam client | gamepad bindings and action sets |
| Steam Remote Storage (Cloud) | Steam | save-file cloud sync |

The UDP game protocol is the only one `beatermp` implements. Everything else is
documented here so the picture is complete and so a future server can decide
what (not) to reimplement.

## 2. UDP game protocol (port 6237)

Full wire codec: `crates/codec/src/lib.rs`; round-trip-tested against captures
in `crates/codec/tests/fixtures/`.

### Envelope

Every datagram starts with one kind byte:

```text
kind 0 Reliable   [0x00][Packet]        acked by kind 2, retransmitted until acked
kind 1 Unreliable [0x01][NetworkEvent]  fire-and-forget (ping/pong, car state)
kind 2 Ack        [0x02][u32 seq]       acknowledges the Reliable packet with that seq
kind 3 Greeting   [0x03][u32 257]       connectivity check, echoed by the host
```

A `Packet` is

```text
[u64 payload_len][payload][u32 seq][u8 resend][u64 sent_at_unix_s]
[Option<u32> ordered_index][Option<Chunk> chunk]

Chunk = { u32 id, u32 offset, u32 total_size, u16 count }
```

bincode `DefaultOptions` + `FixintEncoding` + `AllowTrailing` + `Infinite`:
little-endian fixed-width ints, `u64` length prefixes, one-byte `Option` tag,
`u32` enum discriminant. Each side numbers Reliable packets from 1, acks every
one it receives, retransmits at ~1 Hz bumping `resend`, and chunks payloads over
450 bytes.

### Identities

- `PlayerId = (u32 client_id, u32 player_index)`. Host is `(0xffffffff, 1)`;
  joiners count from `(1, 1)`.
- `Entity = (u32 index, u32 generation)` (a slotmap key); `generation` is `1` in
  every capture.
- `ClientId` is the same slotmap pair (or a bare `u32` at some call sites).

### NetworkEvent variants (all 49)

`tools/re/network_events.py` recovers these from the bincode deserialiser's
`visit_u32` jump table; discriminant == declaration index `0..=48`. The `Sent`
column is the send mode observed in captures: **ord** = reliable + ordered
index, **rel** = reliable unordered, **unrel** = kind 1. Twins are from
`NetworkEvent::broadcast_equivalent` (`0x4d67a0`): a host re-tags a client event
by inserting the sender's `PlayerId` after the discriminant and carrying the
body verbatim (`codec::broadcast_twin`).

| id | serde name (recovered) | body | direction | Sent |
|---|---|---|---|---|
| 0 | (String) | chat string, relayed verbatim | client -> all | rel |
| 1 | (RacePreset) | StartRace: map, `u32 0`, laps, night, rain, variant | host -> all | ord |
| 2 | (unit) | RaceGo | host -> all | unrel |
| 3 | (bool) | RaceEnd, only `1` seen | host -> all | ord |
| 4 | CrossedFinish | `(Entity, f64 time)` | client -> host | rel |
| 5 | CarCrossedFinish | `(PlayerId, Entity, f64 time)` | host -> others | rel |
| 6 | (bool) | Ready checkbox / grid confirm | client -> host | rel |
| 7 | PlayerSetStatusBroadcast | `(PlayerId, bool)` | host -> all | ord |
| 8 | (unit) | unnamed client request | client -> host | rel |
| 9 | (ClientId) | twin of 8 | host -> all | rel |
| 10 | SpawnCarBroadcast | `(CarOwner, PlayerId, Entity, car body, Pose)` 472 B | host -> all | ord |
| 11 | SyncCarState | `(NetworkCarState 94 B, Entity)` | client -> host | unrel |
| 12 | SyncCarStateBroadcast | `(PlayerId, Entity, NetworkCarState)` | host -> others | unrel |
| 13 | ClientConnected | `(PlayerId, ClientInfo)` | host -> lobby | rel |
| 14 | (LocalServerInfo) | ServerInfo (6 fields) | host -> joiner | rel |
| 15 | (ClientInfo) | name + mod map | client -> host | rel |
| 16 | CarDeriative | 420-byte car body (codec: GarageState) | client -> host | rel |
| 17 | LobbyChangeCarBroadcast | `(PlayerId, car body)` | host -> all | rel |
| 18 | LobbyChangeMap | `(map, u32 variant)` | host -> lobby | ord |
| 19 | (unit) | "no car selected" | client -> host | rel |
| 20 | (ClientId) | twin of 19 | host -> all | rel |
| 21 | (unit) | Ping | both | unrel |
| 22 | (unit) | Pong | both | unrel |
| 23 | Disconnect | `u32 DisconnectReason` | client -> host | rel |
| 24 | PlayerLeft | `(ClientId)` | host -> all | rel |
| 25 | (unit) | unnamed client request | client -> host | rel |
| 26 | (SerKey) | twin of 25 | host -> all | rel |
| 27 | (unit) | unnamed client request | client -> host | rel |
| 28 | (ClientId) | twin of 27 | host -> all | rel |
| 29 | UpdateAvatarState | AvatarState (48 B) | client -> host | unrel |
| 30 | UpdateAvatarStateBroadcast | `(PlayerId, AvatarState)` | host -> others | ord |
| 31 | (u32) | UpdateLocation tag | client -> host | rel |
| 32 | (PlayerId, Location) | UpdateLocationBroadcast | host -> others | ord |
| 33 | (SerKey) | RequestVisitGarage (owner) | client -> host | rel |
| 34 | VisitGarageResponse | `(prefix, garage, scene, owner)` 712 B | owner -> visitor | ord |
| 35 | (PlayerId, PlayerId) | garage visit broadcast | host -> others | ord |
| 36 | (GarageStateCommit 5 fields) | client car commit, consumed locally | client -> host | rel |
| 37 | (ClientId) | client request; host unicasts 38 | client -> host | rel |
| 38 | (ClientId) | targeted reply to 37 | host -> one | rel |
| 39 | CarEvent | `(CarEvent, ...)` horn/lights | client -> host | ord* |
| 40 | CarEventBroadcast | `(PlayerId, ...)` | host -> others | ord* |
| 41 | (unit) | StopGarageVisit | client -> host | rel |
| 42 | (PlayerId) | StopGarageVisitBroadcast | host -> others | ord |
| 43 | PushCartStarted | `(Entity)` | client -> host | ord* |
| 44 | PushCartMoved | `(Entity, ...)` | client -> host | ord* |
| 45 | PushCartEnd | `(Entity)` | client -> host | ord* |
| 46 | PushCartStartedBroadcast | `(PlayerId, Entity)` | host -> others | ord* |
| 47 | PushCartMovedBroadcast | `(PlayerId, Entity, ...)` | host -> others | ord* |
| 48 | PushCartEndBroadcast | `(PlayerId, Entity)` | host -> others | ord* |

`*` = never observed in a capture; `docs/notes/parity.md` keeps these twins
reliable + ordered, unlike every other row here which is pinned by a fixture.

Notes:

- **ClientInfo (15)** carries the player name and a mod map. `connect_client`
  (`0x6377f0`) builds a `HashMap<ModName, u64>` of the local mods and serialises
  a `NetworkEvent` whose discriminant is `0xf = 15`. In every capture the map is
  empty, which is the previously-unexplained trailing `u64 0` in the codec's
  `ClientInfo` (`crates/codec/src/lib.rs`). `ModName` is a bincode enum of two
  variants: `0` = newtype `String` (a local mod name), `1` = `(String, u64)` (a
  workshop item, name + published id); the map value is the mod hash.
- **ServerInfo (14)** is `LocalServerInfo` (6 fields: `client_list
  applicant_client_id host_client_id map variant enabled_mods`). The host lists
  joined clients first and itself last; `applicant_client_id` is how a joiner
  learns its own id. `enabled_mods` is the same `ModName` sequence/map, empty in
  all captures.
- **NetworkCarState** is 94 bytes / 14 fields; serde field names were stripped.
  See `docs/notes/parity.md` for the recovered offset layout.
- `Name inferred, not recovered` applies to 3, 8/9, 19/20, 25/26, 27/28, 35:
  their bytes are known, their serde names are not.

### Sequences

Join (a real host):

1. Client `Greeting`; host echoes it.
2. Client `ClientInfo`. Host answers `ServerInfo`, one `GarageStateCommit(17)`
   per listed player, and `ClientConnected(13)` to everyone already in.
3. Client `CarDeriative(16)`. Host relays it as `LobbyChangeCarBroadcast(17)`;
   the client is in.
4. Leave: client `Disconnect(23)`, host `PlayerLeft(24)` to all. A 15 s silence
   also times a client out.

Race: host `PlayerSetStatusBroadcast(host)` + `StartRace(1)` + one
`SpawnCarBroadcast(10)` per participant (host first), all ordered; clients
stream `SyncCarState(11)` at ~20 Hz and the host rebroadcasts each as
`SyncCarStateBroadcast(12)`; at the grid every client sends `Ready(6)` again and
the host relays it; the host's own confirm is `PlayerSetStatusBroadcast(host)`
then `RaceGo(2)`; a finish is `CrossedFinish(4)` / `CarCrossedFinish(5)`;
leaving the results overlay is `RaceEnd(3)` plus a fresh `GarageStateCommit(17)`
of the host's garage, and every client answers with its own worn `GarageState`.

Garage visit: visitor `RequestVisitGarage(33)`; host answers a
`VisitGarageResponse(34)` (or forwards the request to the owner and relays the
reply), then `GarageVisitBroadcast(35)` and an `UpdateLocationBroadcast(32)`
placing the visitor in that garage. While visiting the client streams
`UpdateAvatarState(29)` (~100 Hz), relayed as 30. Leaving sends
`UpdateLocation(31)`/`StopGarageVisit(41)`, relayed as 32/42.

## 3. Master server rendezvous (`209.250.240.105:4321`)

Two rodata copies of the address exist (`0x9ba58`, `0xabf54`). It is the
server-list master named by the `connection_error_master_server_not_responding`
message; it is dead on the public internet, so this section is recovered from
the binary, not from captures.

All messages are raw UDP, no envelope. Opcodes are the first byte.

### Host: register / keepalive

`Server::register_room` (`0x3d5f20`), called once in `Hub::ui_render`
(`0x6adc00`) immediately after `Server::new`, sends **one byte `0x00`** from the
game socket (the `0.0.0.0:6237` socket opened in `Server::new`, `0x3d5fb0`) to
the stored master address. The payload is a 1-byte empty-string constant shared
with many other call sites (`0x812fa`), so it is a bare "register/hello" byte;
`register_room` has exactly one caller (the host button path), so this is the
host's registration/hole-punch hello.

### Joiner: room-code lookup + punch

`Client::nat_punch_connect` (`0x3d5290`) is the whole client side:

1. Parse `209.250.240.105:4321`, bind a fresh UDP socket to `0.0.0.0:0`, set it
   non-blocking.
2. Up to 10 times, 500 ms apart: `recv_from` on that socket, then send the
   **5-byte** request `[0x02][u32 room_code LE]` to the master. The `u32` is
   assembled big-endian from the four bytes a player types into the room-code
   field (`Hub::ui_render`, `0x6ac763`..`0x6ac78b`): `b0<<24 | b1<<16 | b2<<8 | b3`.
3. On a reply:
   - **`0xFF` + UTF-8 `"ip:port"`** -> the master is naming the host; the client
     sends the 4-byte punch `[0xFE]Hi!` (`0x7acb8`) to that address, then keeps
     listening.
   - **`0xFE`** -> proceed: build the `Client` from the punched socket
     (`Client::from_socket`) and start the normal game handshake (section 2).
4. After 10 rounds it gives up (`ConnectionError`).

`Server::run` (`0x3ea980`) references the same `[0xFE]Hi!` constant
(`0x3eb7a9`), i.e. the host echoes the punch to open its own NAT mapping.

### Rooms / server list

- `room_code` is a `u64` read straight out of the server object (`+0x8c`,
  `<Server as NetworkServer>::room_code`, `0x6a54f0`); the Steam variant returns
  a constant `0` (`0x6bacb0`). `Server::new` only clears its low byte, so the
  value is assigned later.
- **Unresolved:** where a host's room code is generated/stored. No code path in
  the host flow writes a non-zero `+0x8c`, the master is dead, and no capture of
  master traffic exists. The client-side lookup format (`02 <u32>`) is proven;
  the host-side advertisement that must bind a code to the host endpoint is not.
- The "Server list" entry resolves through Steam Matchmaking when Steam is
  available (section 4); the custom master's exact list query was not located.

### ConnectionError

From `<ConnectionError>::display`: `unknown`, `connection_failed`,
`version_mismatch`, `master_server_not_responding`, `host_not_responding`,
`failed_to_set_nonblocking` (plus a `DEMO`-prefixed variant).

### Direct by IP

`NetworkManager::client_direct_connect` (`0x538b40`) skips the master entirely:
bind `0.0.0.0:0`, `Client::from_socket`, then the same handshake. This is what
`beatermp` clients use.

## 4. Steam transport and matchmaking

When Steam is present the game offers a second, parallel transport. It carries
the same `NetworkEvent` payloads, framed by the same `PacketFactory`; only the
datagram layer differs. Falls back to Steam-disabled operation when
`SteamAPI_Init` fails.

### Steam NetworkingSockets (P2P)

- `SteamNetworkingServer::new` (`0x78afc0`) calls
  `SteamAPI_ISteamNetworkingSockets_CreateListenSocketP2P(sockets, 0, 0, 8)` —
  a P2P listen socket with 8 virtual ports.
- `SteamNetworkingClient::connect_by_steamid` (`0x78b290`) builds a
  `NetworkingIdentity` from the host's `SteamId` and calls
  `SteamAPI_ISteamNetworkingSockets_ConnectP2P(sockets, identity, 0, 0, 8)`.
- `NetworkingUtils::init_relay_network_access` is called so connections can fall
  back to Steam's relay network when NAT traversal fails.
- Messages are `steamworks::networking_types::NetworkingMessage`; the server
  polls `NetPollGroup::receive_messages` / `NetConnection::receive_messages`
  (`SteamNetworkingServer::run`, `0x7b07b0`).
- Steam's sockets prepend an ASCII banner (`s...`) to datagrams; the game's own
  UDP protocol never starts with `s`, which is how `codec::looks_like_game_traffic`
  tells them apart.

### Steam Matchmaking (lobby / server list)

`steamworks::matchmaking::Matchmaking`:

- `SteamNetworkingServer::new` calls `create_lobby(lobby_type = 2 - friends_only,
  max_members = 6, callback)`. `Public` = 2, `FriendsOnly` = 1 (matching the UI
  options); `Private` is not selectable. A `LobbyCreated_t` callback yields the
  `LobbyId` (an `u64`; the log shows `LobbyId(109775241215232251)`).
- Lobby data (`set_lobby_data` / `get_lobby_data`) carries the host's
  advertised fields; `RequestLobbyList` -> `LobbyMatchList_t` provides the
  server list (`hub_button_server_list`), `join_lobby` -> `LobbyEnter_t` joins
  (`Game::update` closure and `Hub::ui_render` closures), `leave_lobby` leaves.
- `client_steam_connect` (`0x538700`) maps a chosen lobby/SteamId to
  `connect_by_steamid`.

Steam interface getters imported by the binary (i.e. the full set of Steam
subsystems the game touches): `ISteamNetworkingSockets`,
`ISteamNetworkingUtils`, `ISteamMatchmaking`, `ISteamUGC`, `ISteamRemoteStorage`,
`ISteamInput`, `ISteamFriends`, `ISteamUtils`. There is no `ISteamHTTP`,
`ISteamGameServer`, telemetry or crash-reporting import, and no URL other than
the Discord invite and the store page.

## 5. Steam Workshop / UGC (mods)

`steamworks::ugc::UGC`, in `Game::Hub` and `Game`:

- Discovery: `query_items` (+ `QueryHandle::fetch`, `QueryResults::get`) fills
  the in-game Workshop browser (`Hub::start_workshop_display`, `0x601550`
  region); `GetLobbyByIndex`-style paging is not used here.
- Install: `subscribe_item` -> `RemoteStorageSubscribePublishedFileResult_t`;
  `item_install_info(id)` gives the installed folder; if absent,
  `Game::add_workshop_item_mod` (`0x525220`) calls `download_item(id, true)`.
- Updates: the `ItemInstalled_t` and `UserSubscribedItemsListChanged_t`
  callbacks drive `Game::updates` and `Hub::update_workshop_item` (`0x789630`);
  `Hub::check_for_uninstalled` drops mods whose files vanished.
- An installed Workshop item is just a mod folder (section 6.1 of
  `docs/packs.md`): `<install_dir>/description.json5` is parsed as a
  `ModDescription`, its `mod_hash` computed, and a `ModInfo` inserted into the
  game's VFS keyed by the published-file-id string.
- Publishing: `tools/workshop_uploader/workshop_uploader` (iced GUI,
  `steamworks-rs`) uses `ISteamUGC_SetItemContent(content_path)` and
  `SetItemPreview(.../thumbnail.png)` with title/description from the mod's
  `description.json5`; it understands the asset extensions `ron json5 ogg glb
  png txt md` and warns if `thumbnail.png` is missing.

## 6. Steam Input and Cloud

- **Steam Input**: `beaterCore::window` logs `Steam Input: Available gamepads`;
  the game enumerates controller bindings from Steam (the huge embedded
  controller-config blob) and maps them onto its `Binding`/`InputShape` action
  set (`beaterCore::input`). Local `settings.json5` stores only the user's
  `controls: {}` overrides.
- **Steam Cloud (Remote Storage)**: `beaterCore` logs `Steam cloud: ...` when
  syncing saves; save files live under `XDG_DATA_HOME/beaterCore/saves/`
  (section 5 of `docs/packs.md`).
- **Steam Friends**: lobby invites / rich presence (`ISteamFriends`); the
  `FriendsOnly` lobby type and the lobby-list filter are the multiplayer-visible
  parts.

## 7. Open questions

These are the only known gaps in the coverage above; each is bounded, not just
unexamined.

- **Who assigns a host's room code.** `Server::room_code` reads `+0x8c`, but the
  only writer found is `Server::new` clearing its low byte, and no host-path
  code sets it afterwards. The join side (`02 <u32>`) is fully recovered; the
  advertisement that binds a code to a host is not. The master is dead, so it
  cannot be observed live.
- **The custom master's server-list query.** The "Server list" UI resolves via
  Steam Matchmaking when Steam is available; a non-Steam list query against
  `209.250.240.105:4321` was not located beyond the register byte.
- **Serde field names stripped from the binary** for `NetworkCarState` (94
  bytes / 14 fields; offsets recovered in `docs/notes/parity.md`) and the
  unnamed `NetworkEvent` variants (8/9, 19/20, 25/26, 27/28, 35). Bytes and
  roles are known; names are not.
- **Master opcodes beyond 0x00/0x02/0xFF/0xFE** are not enumerated; only the
  four message forms the client/host actually exchange were recovered.

## 8. Evidence index

| Claim | Address / file |
|---|---|
| UDP envelope, Packet, reliability | `crates/codec/src/lib.rs`, README |
| NetworkEvent enum (49) | `tools/re/network_events.py`; parity.md |
| broadcast twins | `<NetworkEvent>::broadcast_equivalent` `0x4d67a0` |
| ClientInfo mod map | `connect_client` `0x6377f0`; ModName serialise `0x4ce330` |
| Master address | rodata `0x9ba58`, `0xabf54`; `nat_punch_connect` `0x3d5290` |
| Host register byte | `register_room` `0x3d5f20`; byte `0x812fa` |
| Rendezvous `02 <u32>` / `FEHi!` | `nat_punch_connect` (`0x3d537a`, `0x7acb8`) |
| Host punch echo | `Server::run` `0x3ea980` (`0x3eb7a9`) |
| Room code getter | `Server::room_code` `0x6a54f0` |
| Room code input assembly | `Hub::ui_render` `0x6ac763`..`0x6ac78b` |
| Steam P2P sockets | `SteamNetworkingServer::new` `0x78afc0`, `connect_by_steamid` `0x78b290` |
| Steam lobby create (type/max 6) | `SteamNetworkingServer::new` |
| Workshop install | `Game::add_workshop_item_mod` `0x525220` |
| Workshop callbacks/UI | `Hub::update_workshop_item` `0x789630`, `start_workshop_display` |
