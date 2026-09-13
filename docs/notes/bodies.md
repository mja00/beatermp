# BeaterCore UDP payload body decode (variants 14, 15, 16, 17)

> **Corrections (later captures, authoritative; `crates/codec/src/lib.rs` and
> `README.md` implement these).** The sections below were written from a
> single-client capture and some of their field mappings turned out wrong:
>
> - Envelope: the `u32` after the payload is a per-peer **sequence number**
>   (from 1), the `u8` is the **retransmission count**, and the tail continues
>   with `Option<u32> ordered_index` and `Option<Chunk>`. Kind 2 frames are
>   **acks** of that sequence, not "Control" messages.
> - `BasicPlayerInfo` is `(client_id: u32, player_index: u32, name, avatar: 5 bytes)`.
>   The host is `(0xffffffff, 1)`; joined clients count from `(1, 1)`. The
>   `AvatarState` is the 5 zero bytes only.
> - `LocalServerInfo` after `client_list` is `applicant: (client_id, player_index)`
>   then `host: (client_id, player_index)`, i.e. `01000000 01000000 ffffffff 01000000`
>   for a first joiner and `02000000 01000000 ffffffff 01000000` for the second.
>   Joined clients are listed first, the host last. The applicant pair is how a
>   joiner learns its own id.
> - `GarageStateCommit` = `[17][owner PlayerId][GarageState body]`; the host
>   sends one per listed player during the handshake and one to the lobby when
>   a newcomer's `GarageState` arrives.
> - New variants: `13 PlayerJoined = [13][PlayerId][ClientInfo body]`,
>   `23 Disconnect = [23][u32 0]` (client -> host), `24 PlayerLeft = [24][PlayerId]`.
> - Race variants (`lobby_race_*.txt`, `grid_confirm_host.txt`): event ids are
>   `u32`, so `1 StartRace = [1][map][u32 0 1 0 0 1]`, `10 SpawnCar =
>   [10][u32 0][PlayerId][u32 entity][u32 generation][GarageState body][f32; 7
>   pose]` (472 bytes, chunked 450 + 22), `11 CarState = [11][94-byte
>   state][u32 entity][u32 generation]` (unreliable), `12 CarStateBroadcast =
>   [12][PlayerId][u32 entity][u32 generation][94-byte state]`. The 94-byte
>   state opens with the SpawnCar pose; no ready/go flag lives in it. The grid
>   confirm is a second `6 Ready(true)`; the host's own confirm is
>   `ReadyBroadcast(host)` (ordered) followed by `2 RaceGo = [2]` (unordered),
>   and clients wait on exactly that.

Scope: this document covers only the **payload bodies** (`[u32 discriminant][body...]`) that
sit inside the envelope's kind-0 `Reliable` variant. The envelope itself lives in
`crates/codec/src/lib.rs`.

All captures below come from `crates/codec/tests/fixtures/host_fd87.txt` (the host/server
side of the capture). Line numbers are 1-based as read from that file.

Confidence key:
- **PROVEN** -- round-trips byte-exactly *and* the type/count matches the serde field-name /
  field-count evidence recovered from the binary.
- **LIKELY** -- byte-exact and internally consistent, but the field-name mapping is inferred
  from context (value plausibility, adjacent evidence) rather than a directly-recovered name.
- **UNRESOLVED** -- byte range and rough type are known, but neither the field name nor exact
  sub-structure could be pinned down from available evidence.

---

## Variant 15 -- `ClientInfo` (client -> server)

Source: line 4, 49-byte envelope, payload = 25 bytes.

### Field table

| Offset | Type | Value (capture) | Field | Confidence |
|---|---|---|---|---|
| 0 | u32 | 15 | enum discriminant | PROVEN |
| 4 | u64 | 5 | string length prefix | PROVEN |
| 12 | 5 bytes (utf8) | "mja00" | player name | LIKELY (matches BasicPlayerInfo/host name seen elsewhere; ClientInfo has no recovered field-name list, only the type name) |
| 17 | u64 | 0 | trailing field, always observed as 0 in this capture | UNRESOLVED (no second occurrence with a non-zero value to confirm semantics; plausibly a generation/version counter) |

Total: 4 + (8+5) + 8 = 25 bytes. Matches payload length exactly.

### Parse script (verified to run clean)

```python
import struct

def read_u32(b, o): return struct.unpack_from('<I', b, o)[0], o + 4
def read_u64(b, o): return struct.unpack_from('<Q', b, o)[0], o + 8
def read_str(b, o):
    n, o = read_u64(b, o)
    return b[o:o+n].decode('utf-8'), o + n

clientinfo = bytes.fromhex(
    '0f00000005000000000000006d6a6130300000000000000000'
)
assert len(clientinfo) == 25
o = 0
disc, o = read_u32(clientinfo, o); assert disc == 15
name, o = read_str(clientinfo, o); assert name == 'mja00'
trailing, o = read_u64(clientinfo, o); assert trailing == 0
assert o == len(clientinfo)
print('ClientInfo OK, consumed', o, 'of', len(clientinfo))
```

### Hex dump

```
0f00000005000000000000006d6a6130300000000000000000
```

---

## Variant 14 -- `ServerInfo` (server -> client, wraps `LocalServerInfo`)

Source: line 6, 109-byte envelope, payload = 85 bytes.

`LocalServerInfo` field-name blob (declaration order, confirmed from `/tmp/bc_str3.txt`
offset ~0x9b681): `client_list, applicant_client_id, host_client_id, map, variant,
enabled_mods` -- 6 fields, matching the recovered field count "ServerInfo 6".

`BasicPlayerInfo` recovered field count = 4 (no individual names recovered from the string
blob for this struct; `AvatarState` recovered field count = 4, only `animation` name
recovered).

### Field table

| Offset | Type | Value (capture) | Field | Confidence |
|---|---|---|---|---|
| 0 | u32 | 14 | enum discriminant | PROVEN |
| 4 | u64 | 1 | `client_list` length (Vec count) | PROVEN (matches LocalServerInfo.client_list, and this capture has exactly one connected client) |
| -- client_list[0] : BasicPlayerInfo (4 fields) -- ||||
| 12 | u32 | 0xffffffff | BasicPlayerInfo field 1 | UNRESOLVED name; LIKELY a per-client generation/session id that starts at the sentinel -1 |
| 16 | u32 | 1 | BasicPlayerInfo field 2 | LIKELY client_id (equals host_client_id below, consistent with "the only client is the host") |
| 20 | u64 | 5 | string length prefix | PROVEN |
| 28 | 5 bytes (utf8) | "mja00" | BasicPlayerInfo field 3 = player name | LIKELY |
| 33 | u8 | 0 | AvatarState field 1 (animation) -- Option tag, None | LIKELY (name confirmed by blob; tag semantics inferred) |
| 34 | u32 | 0 | AvatarState field 2 | UNRESOLVED |
| 38 | u32 | 1 | AvatarState field 3 | UNRESOLVED |
| 42 | u32 | 1 | AvatarState field 4 | UNRESOLVED |
| -- back to LocalServerInfo -- ||||
| 46 | u32 | 0xffffffff | applicant_client_id | PROVEN (name from blob; value -1 = "no applicant pending", consistent semantically) |
| 50 | u32 | 1 | host_client_id | PROVEN (name from blob; value matches client_list[0]'s client id) |
| 54 | u64 | 11 | string length prefix | PROVEN |
| 62 | 11 bytes (utf8) | "forest_long" | map | PROVEN (name from blob) |
| 73 | u32 | 1 | variant | PROVEN (name from blob; value is the map-variant selector, e.g. Default/Reverse/... per the VariantName enum seen in the string blob) |
| 77 | u64 | 0 | enabled_mods length (Vec count) | PROVEN (name from blob; 0 = no enabled mods) |

Total: 4+8 + (4+4+8+5+1+4+4+4) + 4+4+8+11+4+8 = 85 bytes. Matches payload length exactly.

### Parse script (verified to run clean)

```python
import struct

def read_u32(b, o): return struct.unpack_from('<I', b, o)[0], o + 4
def read_u64(b, o): return struct.unpack_from('<Q', b, o)[0], o + 8
def read_str(b, o):
    n, o = read_u64(b, o)
    return b[o:o+n].decode('utf-8'), o + n

serverinfo = bytes.fromhex(
    '0e0000000100000000000000ffffffff0100000005000000000000006d6a61'
    '303000000000000100000001000000ffffffff010000000b00000000000000'
    '666f726573745f6c6f6e67010000000000000000000000'
)
assert len(serverinfo) == 85
o = 0
disc, o = read_u32(serverinfo, o); assert disc == 14
client_count, o = read_u64(serverinfo, o); assert client_count == 1

# BasicPlayerInfo (4 fields)
f1, o = read_u32(serverinfo, o)          # sentinel-like u32, 0xffffffff
client_id, o = read_u32(serverinfo, o)   # likely client_id
name, o = read_str(serverinfo, o)        # player name
avatar_tag = serverinfo[o]; o += 1       # AvatarState.animation Option tag (None)
av2, o = read_u32(serverinfo, o)
av3, o = read_u32(serverinfo, o)
av4, o = read_u32(serverinfo, o)

applicant_client_id, o = read_u32(serverinfo, o)
host_client_id, o = read_u32(serverinfo, o)
mapname, o = read_str(serverinfo, o)
variant, o = read_u32(serverinfo, o)
enabled_mods_count, o = read_u64(serverinfo, o)

assert o == len(serverinfo)
print('ServerInfo OK, consumed', o, 'of', len(serverinfo))
assert name == 'mja00' and mapname == 'forest_long'
assert applicant_client_id == 0xffffffff and host_client_id == 1
assert enabled_mods_count == 0
```

### Hex dump

```
0e0000000100000000000000ffffffff0100000005000000000000006d6a613030000000
00000100000001000000ffffffff010000000b00000000000000666f726573745f6c6f6e
67010000000000000000000000
```

### Zero-client answer

`client_list` is a plain bincode `Vec<BasicPlayerInfo>`: an 8-byte little-endian u64 count
at **offset 4**, followed by that many BasicPlayerInfo entries back-to-back (no
per-entry framing beyond the fields themselves).

For a lobby with **zero clients**, a server would emit the count as u64::to_le_bytes(0) at
offset 4 and *omit all following BasicPlayerInfo bytes entirely* -- i.e. bytes 4..12 become
`00 00 00 00 00 00 00 00` and the payload shrinks by exactly one BasicPlayerInfo's worth of
bytes (34 bytes, per this capture's entry size: 4+4+13+13). Concretely, the zero-client
ServerInfo payload would be this capture with:

- offset 4..12 changed from `01 00 00 00 00 00 00 00` to `00 00 00 00 00 00 00 00`
- offsets 12..46 (the one BasicPlayerInfo entry, 34 bytes) **removed**
- everything from the old offset 46 onward (applicant_client_id, host_client_id, map,
  variant, enabled_mods) shifts left by 34 bytes, unchanged in content

Resulting payload length: 85 minus 34 = 51 bytes.

This has **not** been directly observed in a capture (all observed ServerInfo frames in
both fixtures have exactly one connected client -- the host itself never leaves the
client_list). The derivation above follows directly from the proven Vec<T> bincode
encoding (u64 count + N unframed elements) and is not itself independently confirmed by a
second capture. **To settle this with certainty**: capture a ServerInfo frame sent before
the host's own avatar/session is registered (e.g. the very first frame after
BeginSession/SessionReady control messages, before any ClientInfo has been processed
server-side), or instrument the server to log LocalServerInfo before serialization.

---

## Variant 16 -- `GarageState` (client -> server)

Source: line 12, 448-byte envelope, payload = 424 bytes. Recovered field count: 3.

## Variant 17 -- `GarageStateCommit` (server -> client)

Source: line 7, 456-byte envelope, payload = 432 bytes. Recovered field count: 5.
Recovered field names (from blob, order): `items, projectiles, furniture, car_updates` (4 of
the 5 names recovered; the blob extraction is missing one name, see below).

### Structural relationship (PROVEN by byte diff)

Diffing the two payloads directly (not guessing): GarageStateCommit's bytes from offset 12
onward are **byte-for-byte identical** to GarageState's bytes from offset 4 onward (420
bytes, exact match, confirmed with bytes.find()). This means:

```
GarageState        = [u32 disc=16][ B ]                          (B = 420 bytes)
GarageStateCommit  = [u32 disc=17][u32 0xffffffff][u32 1][ B ]   (same B, byte-identical)
```

4 + 420 = 424, and 4 + 4 + 4 + 420 = 432. This is **PROVEN** (direct byte-exact
substring match across two independent captures of the same session).

The 2 extra u32 fields in GarageStateCommit (0xffffffff and 1) account for the
5-vs-3 recovered-field-count difference (5 = 2 extra + 3 shared). Their names could not be
pinned from the recovered blob (items, projectiles, furniture, car_updates -- 4 names for
what must be a 5-field struct; the field-name blob is evidently missing one name, likely
because it collided with an already-interned string elsewhere in the binary and the grep
window did not capture the duplicate). **LIKELY**: given 0xffffffff mirrors the
applicant_client_id/host_client_id sentinel pattern seen in ServerInfo, and this is a
server -> client acknowledgement of a client-submitted GarageState, offset 4 is plausibly
applicant_client_id or a similar per-client identifier, and offset 8 (=1) is plausibly an
accepted/generation flag. Both are **UNRESOLVED** for exact name.

### Shared body B (420 bytes) -- field table

B opens with a **fixed-size array of 4 identically-shaped 33-byte elements** at relative
offsets 0, 33, 66, 99 (i.e. absolute payload offsets 4, 37, 70, 103 in GarageState, matching
the hint already on file). Each element parses cleanly as:

| Rel. offset in element | Type | Value (element 0) | Field | Confidence |
|---|---|---|---|---|
| 0 | u8 | 1 | Option tag / bool (outer) | LIKELY -- Option<WheelCombo> "Some" tag, or WheelCombo.starter-adjacent bool |
| 1 | u8 | 1 | Option tag / bool (inner) | LIKELY -- nested Option<WheelState> "Some" tag |
| 2 | u32 | 0 | unresolved u32 | UNRESOLVED |
| 6 | f32 | 0.8893 | WheelState.condition | LIKELY (value is in the plausible [0,1] "condition" range; field name confirmed to exist on WheelState by the blob, offset assignment is inferred) |
| 10 | u32 | 0 | WheelState.tire_type | LIKELY (small enum discriminant 0 = Street, first of Street/Offroad/Winter/Racing) |
| 14 | u64 | 11 | string length prefix | PROVEN |
| 22 | 11 bytes (utf8) | "rim_default" | WheelState.rim | LIKELY (name confirmed by blob; "rim_default" is a literal seen elsewhere as a rim/wheel-part id string) |

33 bytes/element x 4 = 132 bytes. This 4-element fixed array is consistent with a per-car
4-wheel array ([Option<WheelCombo>; 4], one per wheel position); WheelCombo{state,
starter} and WheelState{condition, tire_type, rim} are both confirmed struct names from
the blob, and the byte layout is consistent with those 3 declared WheelState fields plus 2
extra leading option-tag/flag bytes for the outer WheelCombo wrapper. The exact byte-to-field
assignment for the two leading option/flag bytes (offset 0, 1) and the two UNRESOLVED u32s
is **not proven** -- only the outer shape (33 bytes, repeats x4, contains a valid
length-prefixed "rim_default" string at a fixed relative offset) is proven by direct byte
inspection.

After the 4 wheel elements (132 bytes), 288 bytes remain in B. This tail is a mix of
plausible u64-length-prefixed float sequences (candidate count markers found at relative
offsets 0 [=3], 131 [=5], 230 [=4] within the tail) interleaved with raw IEEE-754
float/double data whose values are broadly consistent with car-suspension telemetry (the
string blob has adjacent field names cornering_speed, corner_threshold, travel,
stiffness, damping, frame_point for wheel/suspension structs), but **no reliable,
byte-exact field-by-field decomposition could be established from a single capture** -- there
is no second capture with a different item/wheel count to disambiguate which apparent "count"
bytes are real Vec length prefixes vs. coincidental small integers embedded in fixed-size
numeric fields (e.g. gear counts). This entire 288-byte tail is reported as **UNRESOLVED**
internal structure; its *total length* (288 bytes) is PROVEN by exact byte accounting.

### Parse script (verified to run clean; proves exact-length consumption)

```python
import struct

def read_u32(b, o): return struct.unpack_from('<I', b, o)[0], o + 4
def read_u64(b, o): return struct.unpack_from('<Q', b, o)[0], o + 8
def read_str(b, o):
    n, o = read_u64(b, o)
    return b[o:o+n].decode('utf-8'), o + n

def parse_wheel_elem(b, o):
    opt1 = b[o]; o += 1
    opt2 = b[o]; o += 1
    u1, o = read_u32(b, o)
    condition = struct.unpack_from('<f', b, o)[0]; o += 4
    u2, o = read_u32(b, o)
    rim, o = read_str(b, o)
    return dict(opt1=opt1, opt2=opt2, u1=u1, condition=condition, u2=u2, rim=rim), o

def parse_garage_body(b, expect_disc):
    o = 0
    disc, o = read_u32(b, o); assert disc == expect_disc
    extra = None
    if expect_disc == 17:  # GarageStateCommit has 2 extra leading u32 fields
        extra1, o = read_u32(b, o)
        extra2, o = read_u32(b, o)
        extra = (extra1, extra2)
    wheels = []
    for _ in range(4):
        w, o = parse_wheel_elem(b, o)
        wheels.append(w)
    tail = b[o:]                 # UNRESOLVED opaque physics/suspension blob
    o += len(tail)
    assert o == len(b)
    return dict(disc=disc, extra=extra, wheels=wheels, tail_len=len(tail))

def payload_of(line):
    parts = line.split(' ')
    b = bytes.fromhex(parts[-1])
    plen = struct.unpack('<Q', b[1:9])[0]
    return b[9:9+plen]

with open('crates/codec/tests/fixtures/host_fd87.txt') as f:
    lines = f.read().splitlines()

garage = payload_of(lines[11])   # line 12 (0-indexed 11): GarageState, 424 bytes
commit = payload_of(lines[6])    # line 7  (0-indexed 6):  GarageStateCommit, 432 bytes

g = parse_garage_body(garage, 16)
c = parse_garage_body(commit, 17)

assert len(garage) == 424 and len(commit) == 432
assert g['tail_len'] == 288 and c['tail_len'] == 288
assert c['extra'] == (0xffffffff, 1)
print('GarageState / GarageStateCommit OK -- both consume exactly len(payload)')
```

### Hex dumps

GarageState (424 bytes):
```
1000000001010000000025a9633f000000000b0000000000000072696d5f64656661756c74
010100000000d6e64c3f000000000b0000000000000072696d5f64656661756c7401010000
00006b974b3f000000000b0000000000000072696d5f64656661756c74010100000000082a
4b3f000000000b0000000000000072696d5f64656661756c7403000000000000005a617074
88502049b0e33f970768826f61e23f5c480f321c53e23fcb2b835c694ce13fdc2a6f6e2d90e
13fcd90ca034d17e73f2a56d427234ce73f1e4565572bb3e73f2f568258a1a2e73f01000000
010000000100000001000000056bfdef7c02e73fd30aa93508d8ea3f76840b7acb91eb3f0f
c7c33eea0ded3f0500000000000000ac2d0dff58feef3f9e792a1ec9caef3f0d9a7e95d454e
e3f5ece0d78280eed3f7f7fc352b5efee3f0000000000000000000300000052b85e3f52b81e
3f0000803e0100000001000000000092ce6a3f4d29693fcabb423f22e43f3f040000000000
0000010a3d20d21a7c7a40010a3d20d21a7c7a40010a3d20d21a7c7a40010a3d20d21a7c7a4
00044e3273e000000000000000000
```

GarageStateCommit (432 bytes):
```
11000000ffffffff0100000001010000000025a9633f000000000b0000000000000072696d
5f64656661756c74010100000000d6e64c3f000000000b0000000000000072696d5f646566
61756c740101000000006b974b3f000000000b0000000000000072696d5f64656661756c74
010100000000082a4b3f000000000b0000000000000072696d5f64656661756c7403000000
000000005a61707488502049b0e33f970768826f61e23f5c480f321c53e23fcb2b835c694c
e13fdc2a6f6e2d90e13fcd90ca034d17e73f2a56d427234ce73f1e4565572bb3e73f2f56825
8a1a2e73f01000000010000000100000001000000056bfdef7c02e73fd30aa93508d8ea3f7
6840b7acb91eb3f0fc7c33eea0ded3f0500000000000000ac2d0dff58feef3f9e792a1ec9ca
ef3f0d9a7e95d454ee3f5ece0d78280eed3f7f7fc352b5efee3f0000000000000000000300
000052b85e3f52b81e3f0000803e0100000001000000000092ce6a3f4d29693fcabb423f22
e43f3f0400000000000000010a3d20d21a7c7a40010a3d20d21a7c7a40010a3d20d21a7c7a
40010a3d20d21a7c7a400044e3273e000000000000000000
```

---

## NetworkEvent variant discriminants pinned from the binary

Observed directly in captures (both fixtures, kind0 Data payloads):

| Name | Discriminant (u32) | Confidence |
|---|---|---|
| ServerInfo | 14 | PROVEN |
| ClientInfo | 15 | PROVEN |
| GarageState | 16 | PROVEN |
| GarageStateCommit | 17 | PROVEN |
| CrossedFinish | 4 | PROVEN (`finish_host.txt`; binary says "2 elements": `Entity`, `f64`) |
| CarCrossedFinish | 5 | PROVEN (`finish_host.txt`; "3 elements": `PlayerId`, `Entity`, `f64`) |
| (RaceEnd) | 3 | body `u8 1`, host -> clients when the host leaves the finish overlay; real variant name not recovered |
| UpdateAvatarState / UpdateAvatarStateBroadcast | 29 / 30 | PROVEN (`garage_visit_host.txt`; 48-byte avatar pose, broadcast prepends `PlayerId`) |
| UpdateLocation / UpdateLocationBroadcast | 31 / 32 | PROVEN (`u32` location tag; host emits `2` + owner `PlayerId` for "in a garage") |
| RequestVisitGarage | 33 | PROVEN (body: owner `PlayerId`) |
| VisitGarageResponse | 34 | PROVEN ("4 elements": 32-byte prefix, garage body, 240-byte scene, owner `PlayerId`, then 8 zero bytes; no visitor id) |
| (garage visit broadcast) | 35 | body `PlayerId visitor, PlayerId owner`; real variant name not recovered |
| StopGarageVisit / StopGarageVisitBroadcast | 41 / 42 | PROVEN (unit body; broadcast carries the visitor `PlayerId`) |

`Entity` is bevy's `(u32 index, u32 generation)`, the same pair SpawnCar and
CarState carry. Race time is the `f64` shown in the results table (9.224 and
41.271 in the capture). A host with one client did not echo that client's own
crossing back to it; the relay is for the other participants.

Additional NetworkEvent-adjacent type names recovered from bincode::ser::Compound::
serialize_field monomorphizations in /tmp/bc_str3.txt (demangled symbols), appearing as
sibling payload types serialized the same way as GarageState/GarageStateCommit/
ClientInfo (strongly suggesting they are also NetworkEvent variants), but with
**no numeric discriminant evidence available** (never observed in either capture, and no
jump/match table was located in the disassembly within the time available for this pass):

- NetworkCarState (recovered field count: 14) -- UNRESOLVED discriminant
- CarEvent -- UNRESOLVED discriminant
- CarOwner -- UNRESOLVED discriminant

Separately, a ClientNetworkEvent enum (distinct from NetworkEvent) was found in the same
string blob with 6 variant names in this order (declaration order, per serde derive's
interned VARIANTS array convention): CarResetBroadcast, LobbyMakeSpectatorBroadcast,
ClientDisconnected, EnterCarBroadcast, RequestVisitGarage, StopGarageVisitBroadcast. Whether
this is nested inside one NetworkEvent variant (e.g. NetworkEvent::ClientEvent
(ClientNetworkEvent)) or is a top-level sibling enum could not be determined, and no
numeric discriminants for it were pinned. **To settle this**: capture traffic during an
in-garage session (car reset, spectator toggle, disconnect, enter car) to observe these
variants on the wire, or locate the deserialize_enum/visit_enum match arms for
NetworkEvent in the disassembly (the demangled symbols reference visit_enums0_ through
visit_enumsh_ scope-local labels for NetworkEvent's visitor, which correspond to LLVM
basic-block names, not enum discriminants, and were not further resolved in this pass).
