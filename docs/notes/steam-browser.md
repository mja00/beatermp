# In-game Server list: how it works and what a beatermp lobby needs

Phase 0 of the Steam-lobby plan: recover the client's Server-list path, the
lobby contract, and the Steam transport framing. All addresses are file virtual
addresses in the shipped `beaterCore`; decompile with
`tools/re/export_decomp.py`.

## 1. The Server list is a Steam Matchmaking lobby list

`Hub::ui_render` (`0x6a6a50`) switches on the popup that was opened. The
Server-list case (`case 9`) opens an `SUIContext` and calls

```
Matchmaking::request_lobby_list::<Hub::ui_render::{closure#2}>   (0x741fd0)
```

which is a thin wrapper around `SteamAPI_ISteamMatchmaking_RequestLobbyList`
(`0x741feb`). The result arrives through the `LobbyMatchList_t` callback
(`register_call_result<...>::call_once` shim at `0x74bcc0`), which:

- reads the lobby count from the callback,
- for `i in 0..count` calls `GetLobbyByIndex(i)` (`0x74bd46`),
- collects the `LobbyId`s into a `Vec<LobbyId>`,
- sends them to the UI over an `mpsc::Sender<Vec<LobbyId>>` (`0x3828` in the
  decompile) for rendering.

There is **no custom-master server-list query** in the binary. The master at
`209.250.240.105:4321` is referenced by exactly two things: `Server::register_room`
(`0x3d5f20`, the host's `0x00` register byte) and `Client::nat_punch_connect`
(`0x3d5290`, the room-code `0x02 <u32>` lookup). The UI string
`connection_error_master_server_not_responding` reads "Server list is not
responding", but that `ConnectionError` variant is just reused for a failed
connect; it is not a separate list protocol.

## 2. The lobby contract is minimal

- **No list filters.** The only imported matchmaking list APIs are
  `RequestLobbyList` and `GetLobbyByIndex`; there is no `AddRequestLobbyList*`
  import at all. The list is therefore every lobby for AppID 3711050.
- **No lobby data.** `GetLobbyData` is called only inside
  `Matchmaking::lobby_data` (`0x825330`), and `SetLobbyData` only inside
  `Matchmaking::set_lobby_data` (`0x8254b0`). Neither function has a caller
  (no direct call, no function-pointer reference), so this build never sets or
  reads lobby data. A lobby needs **no keys** to appear or be joinable.

Host side (`SteamNetworkingServer::new`, `0x78afc0`):

```
CreateListenSocketP2P(sockets, 0, 0, 8)
create_lobby(lobby_type = 2 - friends_only, max_members = 6)
```

so `Public` = 2 and `FriendsOnly` = 1, six seats.

Join side (`connect_by_steamid`, `0x78b290`): the client `JoinLobby(id)`s, and
on `LobbyEnter_t` builds a `NetworkingIdentity` from the lobby owner's
`SteamId` and calls `ConnectP2P(sockets, identity, 0, 0, 8)`.

## 3. Steam transport framing: raw events, not the UDP envelope

The UDP transport wraps events in the kind-0/1/2/3 `Frame`/`Packet` envelope
(seq, acks, retransmits, chunks). The Steam transport does not:

- `SteamNetworkingClient::send_reliable` (`0x7a50b0`) and
  `SteamNetworkingServer::send_reliable` (`0x7b0670`) are
  `NetConnection::send_message(data, len, flags = 8)`; `send` (`0x7a53a0`) is
  the same with `flags = 0`. Steam's own reliability/ordering replaces the
  envelope.
- `SteamNetworkingClient::run` (`0x7a50e0`) and `SteamNetworkingServer::run`
  (`0x7b07b0`) call `NetConnection::receive_messages` and hand back the raw
  message bytes; there is no `PacketFactory::process_packet` on this path.

So a Steam message payload is a bare bincode `NetworkEvent`
(`[u32 discriminant][body]`), with Steam's reliable bit distinguishing what UDP
would send as kind 0 from kind 1.

## 4. What this means for beatermp

1. Creating a lobby is necessary but not sufficient: a player who clicks it is
   sent to `ConnectP2P(lobby owner)`. beatermp must be the lobby owner **and**
   the P2P endpoint, speaking raw `NetworkEvent`s.
2. Because the client never reads lobby data, there are no keys to match and no
   metadata to advertise server-side. Discovery is all a lobby gives us.
3. Presence and transport are one feature, not two.
4. The bridge must translate `Frame` <-> raw `NetworkEvent` for the existing
   UDP server: strip/rebuild the UDP envelope, acknowledge beatermp's kind-0
   frames on the bridge's behalf, and reassemble beatermp's chunked payloads
   (SpawnCar 472 B, VisitGarageResponse 712 B) into single Steam messages.

## 5. Bridge implementation

`crates/steam` (binary `beatermp-steam`) implements a bridge against this
contract:

- `translate::Peer` is the seam: it wraps client Steam messages (raw events) in
  kind-0/kind-1 `Frame`s for beatermp, and turns beatermp's frames back into
  single Steam messages, acking beatermp's kind-0 frames and reassembling its
  chunked payloads (`SpawnCar`, `VisitGarageResponse`) before they reach Steam.
- The binary creates the lobby, a P2P listen socket, and one local UDP socket
  per Steam peer pointed at `127.0.0.1:6237`; `--data-file` records lobby,
  connect/disconnect and `ClientInfo` (name + enabled-mod count) events as
  JSONL.

Build and run (needs Steam running, game owned):

```sh
./target/release/beatermp &
cargo run -p beatermp-steam --features steam -- --port 6237 --data-file players.jsonl
```

## 6. Verification (blocked on a Steam session)

The probe that would prove this is: run a Steamworks process as AppID 3711050,
`create_lobby(Public, 6)`, keep it alive, and open BeaterCore's Server list. It
needs, on one machine: the Steam client running and logged in, an account that
owns BeaterCore (installed here:
`/mnt/data-drive/SteamLibrary/steamapps/appmanifest_3711050.acf`), and the game
client to view the list. None of that is available in this environment - the
Steam client is not running - so the end-to-end check must be run by hand.

## 6. Evidence index

| Claim | Address / symbol |
|---|---|
| Server-list popup -> Steam lobby list | `Hub::ui_render` `0x6a6a50` case 9; `request_lobby_list` `0x741fd0`; `RequestLobbyList` call `0x741feb` |
| Result callback collects `LobbyId`s | `LobbyMatchList_t` shim `0x74bcc0`; `GetLobbyByIndex` `0x74bd46` |
| No custom-master list query | master string refs: `register_room` `0x3d5f20`, `nat_punch_connect` `0x3d5290` only |
| No lobby-list filters | no `AddRequestLobbyList*` imports |
| No lobby data used | `lobby_data` `0x825330`, `set_lobby_data` `0x8254b0`, both uncalled |
| Lobby create type/max | `SteamNetworkingServer::new` `0x78afc0` (type `2 - friends_only`, max 6) |
| Join by SteamID | `connect_by_steamid` `0x78b290` |
| Steam send flags 8/0 | `SteamNetworkingClient::{send_reliable,send}` `0x7a50b0`/`0x7a53a0` |
| Steam receive is raw | `SteamNetworkingClient::run` `0x7a50e0` |
