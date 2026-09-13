//! Wire codec for BeaterCore's UDP multiplayer protocol.
//!
//! # Provenance
//!
//! Everything here was derived from the shipped binary
//! (`beaterCore`, Steam AppID 3711050) and from syscall-level packet captures
//! of real host/client sessions (`tests/fixtures`). The relevant evidence:
//!
//! * `bincode::internal::serialize::<Packet, _>` shows the codec is bincode's
//!   `DefaultOptions` with `FixintEncoding`, `AllowTrailing` and `Infinite`
//!   limit: little-endian fixed-width integers, `u64` length prefixes for
//!   sequences and strings, a one-byte `Option` tag, and a `u32` discriminant
//!   for Rust enums.
//! * The serde string blob in rodata lists `Packet`'s field names as
//!   `id generation sent_at ordered_index large_packet_chunk` (serde reports
//!   "struct Packet with 6 elements"; the sixth is the payload, which is
//!   serialised first).
//! * Captures show four envelope kinds, distinguished by a leading byte.
//!
//! # Envelope
//!
//! ```text
//! kind 0 Reliable   [0x00][Packet]        acked by kind 2, retransmitted until acked
//! kind 1 Unreliable [0x01][NetworkEvent]  fire and forget (ping/pong, car state)
//! kind 2 Ack        [0x02][u32 seq]       acknowledges the Reliable packet with that seq
//! kind 3 Greeting   [0x03][u32 257]       connectivity check, echoed by the server
//! ```
//!
//! A `Packet` is `[u64 len][payload][u32 seq][u8 resend][u64 sent_at]
//! [Option<u32> ordered_index][Option<Chunk> chunk]`. The payload, and the
//! whole body of an Unreliable frame, is a bincode-encoded `NetworkEvent`
//! whose `u32` discriminant selects the message; see [`Event`].
//!
//! # Reliability
//!
//! Each side numbers its Reliable packets from 1. The receiver answers every
//! one with `Ack(seq)`; the sender retransmits at ~1 Hz with `resend`
//! incremented until the ack arrives. Payloads over [`CHUNK_SIZE`] bytes are
//! split into consecutive packets that share a `chunk.id`.

use std::fmt;

/// One-byte envelope tag at the start of every datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Kind {
    Reliable = 0,
    Unreliable = 1,
    Ack = 2,
    Greeting = 3,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        match v {
            0 => Some(Kind::Reliable),
            1 => Some(Kind::Unreliable),
            2 => Some(Kind::Ack),
            3 => Some(Kind::Greeting),
            _ => None,
        }
    }
}

/// The value both sides send and echo in a Greeting: `03 01 01 00 00`.
pub const GREETING_ID: u32 = 0x0000_0101;

/// Largest Reliable payload sent in one datagram; larger events are chunked.
/// Measured from a 472-byte event that arrived as 450 + 22 bytes.
pub const CHUNK_SIZE: usize = 450;

/// `NetworkEvent` discriminants observed on the wire.
///
/// The enum has at least 49 variants (serde's `variant index 0 <= i < 49`
/// panic string). Only the ones seen in captures are named; the role column
/// is inferred from when each appeared and what the UI did in response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Event {
    /// Host -> clients when the host presses Start Race. Body: map name,
    /// variant and race settings.
    StartRace = 1,
    /// Host -> clients, unordered, when the host confirms at the grid after
    /// everyone else: starts the countdown. No body.
    RaceGo = 2,
    /// Host -> clients when the host leaves the finish overlay: `bool`, only
    /// `true` observed. Clients jump to the results notepad and then the
    /// lobby, and both sides re-exchange garage state (conditions wore).
    /// The variant name was not recovered; this one is descriptive.
    RaceEnd = 3,
    /// Client -> host: its car crossed the finish, `(Entity, f64 race time)`.
    CrossedFinish = 4,
    /// Host -> the other clients: `(PlayerId, Entity, f64 race time)` for a
    /// car that finished, including the host's own.
    CarCrossedFinish = 5,
    /// Client -> host: `bool`, the lobby Ready checkbox.
    Ready = 6,
    /// Host -> clients: `(u32 client_id, u32 player_index, bool)` mirror of a
    /// Ready toggle, drawn as a check mark over that player's car.
    ReadyBroadcast = 7,
    /// Host -> clients at race start, once per participant: the car to spawn
    /// (`client_id`, `player_index`, entity id, garage body). 472 bytes, so
    /// it is the one event seen chunked.
    SpawnCar = 10,
    /// Client -> host, Unreliable, ~20 Hz during a race: own car state.
    CarState = 11,
    /// Host -> clients, Unreliable, ~20 Hz: another participant's car state.
    CarStateBroadcast = 12,
    /// Host -> lobby members: a new client identified itself, see
    /// [`PlayerInfo`]. Its garage follows as a `GarageStateCommit`.
    PlayerJoined = 13,
    /// Host -> client: lobby description, see [`ServerInfo`].
    ServerInfo = 14,
    /// Client -> host: the joining player's identity, see [`ClientInfo`].
    ClientInfo = 15,
    /// Client -> host: the joining player's garage (car customisation).
    GarageState = 16,
    /// Host -> client: the host's committed garage state.
    GarageStateCommit = 17,
    /// Unreliable, 1 Hz both ways: `f32` sender clock.
    Ping = 21,
    /// Unreliable: `f32` echo of the peer's most recent Ping clock.
    Pong = 22,
    /// Client -> host: leaving the lobby; body is a `u32`, only `0` observed.
    /// The client stops acking afterwards.
    Disconnect = 23,
    /// Host -> lobby members: a player left, body is its [`PlayerId`].
    PlayerLeft = 24,
}

impl Event {
    pub fn from_u32(v: u32) -> Option<Event> {
        Some(match v {
            1 => Event::StartRace,
            2 => Event::RaceGo,
            3 => Event::RaceEnd,
            4 => Event::CrossedFinish,
            5 => Event::CarCrossedFinish,
            6 => Event::Ready,
            7 => Event::ReadyBroadcast,
            10 => Event::SpawnCar,
            11 => Event::CarState,
            12 => Event::CarStateBroadcast,
            13 => Event::PlayerJoined,
            14 => Event::ServerInfo,
            15 => Event::ClientInfo,
            16 => Event::GarageState,
            17 => Event::GarageStateCommit,
            21 => Event::Ping,
            22 => Event::Pong,
            23 => Event::Disconnect,
            24 => Event::PlayerLeft,
            _ => return None,
        })
    }
}

/// Errors produced while framing or parsing datagrams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Buffer ended before a field could be read.
    Truncated {
        wanted: usize,
        at: usize,
        len: usize,
    },
    /// Leading byte was not a known envelope kind.
    UnknownKind(u8),
    /// A variable-length field claimed a size that cannot fit.
    ImplausibleLength {
        field: &'static str,
        value: u64,
        at: usize,
    },
    /// A `u32` discriminant with no known meaning.
    UnknownDiscriminant { what: &'static str, value: u32 },
    /// An `Option` tag other than 0 or 1.
    BadOptionTag { at: usize, value: u8 },
    /// Text field was not valid UTF-8.
    InvalidUtf8 { at: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated { wanted, at, len } => {
                write!(
                    f,
                    "truncated: wanted {wanted} bytes at {at}, datagram is {len}"
                )
            }
            Error::UnknownKind(k) => write!(f, "unknown envelope kind {k}"),
            Error::ImplausibleLength { field, value, at } => {
                write!(f, "implausible length {value} for {field} at {at}")
            }
            Error::UnknownDiscriminant { what, value } => {
                write!(f, "unknown {what} discriminant {value}")
            }
            Error::BadOptionTag { at, value } => write!(f, "bad option tag {value} at {at}"),
            Error::InvalidUtf8 { at } => write!(f, "invalid UTF-8 at {at}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Fragment header carried by every piece of a payload larger than
/// [`CHUNK_SIZE`]. Field names are inferred from the values: two pieces of one
/// 472-byte event carried `(1, 0, 472, 2)` and `(1, 450, 472, 2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// Identifies the large event; all of its pieces share this id.
    pub id: u32,
    /// Byte offset of this piece within the reassembled payload.
    pub offset: u32,
    /// Length of the reassembled payload.
    pub total_size: u32,
    /// Number of pieces.
    pub count: u16,
}

/// A Reliable datagram: one `NetworkEvent` (or a chunk of one) plus the
/// bookkeeping that makes it reliable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub payload: Vec<u8>,
    /// Per-sender sequence number, counting from 1. Acked by `Ack(seq)`.
    pub seq: u32,
    /// How many times this packet has been sent before; 0 on first send.
    pub resend: u8,
    /// Sender's clock, whole seconds since the Unix epoch.
    pub sent_at: u64,
    /// Position in the sender's ordered stream, when the event is ordered.
    /// The host numbers its lobby/race events from 0; handshake events and
    /// everything the client sends carry `None`.
    pub ordered_index: Option<u32>,
    /// Present when this packet is one piece of a larger payload.
    pub chunk: Option<Chunk>,
}

/// A framed datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Reliable(Packet),
    /// A bare `NetworkEvent`.
    Unreliable(Vec<u8>),
    Ack(u32),
    Greeting(u32),
}

// ---------------------------------------------------------------------------
// Little-endian cursor helpers
// ---------------------------------------------------------------------------

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(Error::Truncated {
                wanted: n,
                at: self.pos,
                len: self.buf.len(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn option_tag(&mut self) -> Result<bool> {
        let at = self.pos;
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(Error::BadOptionTag { at, value }),
        }
    }

    /// A bincode byte sequence: `u64` length followed by that many bytes.
    fn bytes(&mut self, field: &'static str) -> Result<&'a [u8]> {
        let at = self.pos;
        let n = self.u64()?;
        if n as usize > self.remaining() {
            return Err(Error::ImplausibleLength {
                field,
                value: n,
                at,
            });
        }
        self.take(n as usize)
    }

    fn string(&mut self) -> Result<String> {
        let at = self.pos;
        let raw = self.bytes("string")?;
        String::from_utf8(raw.to_vec()).map_err(|_| Error::InvalidUtf8 { at })
    }
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Parse a datagram received on the game's server socket.
pub fn parse(buf: &[u8]) -> Result<Frame> {
    let mut c = Cursor::new(buf);
    let tag = c.u8()?;
    let kind = Kind::from_u8(tag).ok_or(Error::UnknownKind(tag))?;

    match kind {
        Kind::Unreliable => Ok(Frame::Unreliable(c.take(c.remaining())?.to_vec())),
        Kind::Ack => Ok(Frame::Ack(c.u32()?)),
        Kind::Greeting => Ok(Frame::Greeting(c.u32()?)),
        Kind::Reliable => {
            let payload = c.bytes("payload")?.to_vec();
            let seq = c.u32()?;
            let resend = c.u8()?;
            let sent_at = c.u64()?;
            let ordered_index = if c.option_tag()? {
                Some(c.u32()?)
            } else {
                None
            };
            let chunk = if c.option_tag()? {
                Some(Chunk {
                    id: c.u32()?,
                    offset: c.u32()?,
                    total_size: c.u32()?,
                    count: c.u16()?,
                })
            } else {
                None
            };
            Ok(Frame::Reliable(Packet {
                payload,
                seq,
                resend,
                sent_at,
                ordered_index,
                chunk,
            }))
        }
    }
}

/// Serialise a frame back to wire form.
pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    match frame {
        Frame::Unreliable(payload) => {
            out.push(Kind::Unreliable as u8);
            out.extend_from_slice(payload);
        }
        Frame::Ack(seq) => {
            out.push(Kind::Ack as u8);
            out.extend_from_slice(&seq.to_le_bytes());
        }
        Frame::Greeting(id) => {
            out.push(Kind::Greeting as u8);
            out.extend_from_slice(&id.to_le_bytes());
        }
        Frame::Reliable(p) => {
            out.push(Kind::Reliable as u8);
            out.extend_from_slice(&(p.payload.len() as u64).to_le_bytes());
            out.extend_from_slice(&p.payload);
            out.extend_from_slice(&p.seq.to_le_bytes());
            out.push(p.resend);
            out.extend_from_slice(&p.sent_at.to_le_bytes());
            match p.ordered_index {
                Some(i) => {
                    out.push(1);
                    out.extend_from_slice(&i.to_le_bytes());
                }
                None => out.push(0),
            }
            match p.chunk {
                Some(ch) => {
                    out.push(1);
                    out.extend_from_slice(&ch.id.to_le_bytes());
                    out.extend_from_slice(&ch.offset.to_le_bytes());
                    out.extend_from_slice(&ch.total_size.to_le_bytes());
                    out.extend_from_slice(&ch.count.to_le_bytes());
                }
                None => out.push(0),
            }
        }
    }
    out
}

/// Read the leading `u32` discriminant of a `NetworkEvent` payload.
pub fn event_kind(payload: &[u8]) -> Result<Event> {
    let v = Cursor::new(payload).u32()?;
    Event::from_u32(v).ok_or(Error::UnknownDiscriminant {
        what: "event",
        value: v,
    })
}

fn expect_event(c: &mut Cursor<'_>, want: Event, what: &'static str) -> Result<()> {
    let disc = c.u32()?;
    if Event::from_u32(disc) != Some(want) {
        return Err(Error::UnknownDiscriminant { what, value: disc });
    }
    Ok(())
}

/// Decode the `f32` clock carried by a Ping or Pong.
pub fn clock_of(payload: &[u8]) -> Result<f32> {
    let mut c = Cursor::new(payload);
    c.u32()?;
    c.f32()
}

/// Encode a Ping or Pong carrying `clock`.
pub fn encode_clock(event: Event, clock: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&(event as u32).to_le_bytes());
    out.extend_from_slice(&clock.to_bits().to_le_bytes());
    out
}

/// A player identity, sent by a joining client as [`Event::ClientInfo`].
///
/// The captured body is exactly `len-prefixed "mja00"` followed by `u64 0`.
/// The trailing `u64` has only ever been observed as `0`; its meaning is
/// unresolved (no recovered field names for `ClientInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub name: String,
    pub trailing: u64,
}

impl ClientInfo {
    pub fn decode(payload: &[u8]) -> Result<ClientInfo> {
        let mut c = Cursor::new(payload);
        expect_event(&mut c, Event::ClientInfo, "ClientInfo")?;
        ClientInfo::read(&mut c)
    }

    fn read(c: &mut Cursor<'_>) -> Result<ClientInfo> {
        let name = c.string()?;
        let trailing = c.u64()?;
        Ok(ClientInfo { name, trailing })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.name.len());
        out.extend_from_slice(&(Event::ClientInfo as u32).to_le_bytes());
        put_string(&mut out, &self.name);
        out.extend_from_slice(&self.trailing.to_le_bytes());
        out
    }

    /// Decode an [`Event::PlayerJoined`] broadcast into the newcomer's id and
    /// identity.
    pub fn decode_joined(payload: &[u8]) -> Result<(PlayerId, ClientInfo)> {
        let mut c = Cursor::new(payload);
        expect_event(&mut c, Event::PlayerJoined, "PlayerJoined")?;
        let id = PlayerId::read(&mut c)?;
        Ok((id, ClientInfo::read(&mut c)?))
    }

    /// Encode an [`Event::PlayerJoined`] broadcast, which a host sends to every
    /// lobby member when a new client identifies itself: this `ClientInfo`
    /// re-tagged with the id the host assigned it.
    pub fn encode_joined(&self, id: PlayerId) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.name.len());
        out.extend_from_slice(&(Event::PlayerJoined as u32).to_le_bytes());
        id.write(&mut out);
        put_string(&mut out, &self.name);
        out.extend_from_slice(&self.trailing.to_le_bytes());
        out
    }
}

/// One entry of [`ServerInfo::players`] (`BasicPlayerInfo`, 4 fields).
///
/// The host's own entry is `(0xffffffff, 1, name, avatar)`; the same
/// `(0xffffffff, 1)` pair prefixes the host's `ReadyBroadcast` and `SpawnCar`,
/// and joined clients count up from `(1, 1)`. `u32::MAX` therefore reads as
/// "the host" and the second word as a per-client player slot (the game
/// supports split screen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerInfo {
    pub id: PlayerId,
    pub name: String,
    /// `AvatarState`: observed only as `00 00000000`. Carried verbatim.
    pub avatar: [u8; 5],
}

impl PlayerInfo {
    fn read(c: &mut Cursor<'_>) -> Result<PlayerInfo> {
        let id = PlayerId::read(c)?;
        let name = c.string()?;
        let mut avatar = [0u8; 5];
        avatar.copy_from_slice(c.take(5)?);
        Ok(PlayerInfo { id, name, avatar })
    }

    fn write(&self, out: &mut Vec<u8>) {
        self.id.write(out);
        put_string(out, &self.name);
        out.extend_from_slice(&self.avatar);
    }
}

/// Client id the game uses for the hosting player.
pub const HOST_CLIENT_ID: u32 = u32::MAX;

/// `(client_id, player_index)` naming one player; the game's `PlayerId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerId {
    pub client_id: u32,
    pub player_index: u32,
}

impl PlayerId {
    pub const HOST: PlayerId = PlayerId {
        client_id: HOST_CLIENT_ID,
        player_index: 1,
    };

    fn read(c: &mut Cursor<'_>) -> Result<PlayerId> {
        Ok(PlayerId {
            client_id: c.u32()?,
            player_index: c.u32()?,
        })
    }

    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.client_id.to_le_bytes());
        out.extend_from_slice(&self.player_index.to_le_bytes());
    }
}

/// Lobby description sent to a joining client as [`Event::ServerInfo`]
/// (`LocalServerInfo`, 6 fields: `client_list applicant_client_id
/// host_client_id map variant enabled_mods` per the recovered names).
///
/// A real host lists already-joined clients first and itself last. The
/// applicant id is how a joiner learns its own `(client_id, player_index)`.
/// A client sent an empty `players` list stalls at "Connecting", so the host's
/// own entry is mandatory, and the client also waits for one
/// `GarageStateCommit` per listed player before it enters the lobby.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub players: Vec<PlayerInfo>,
    /// The player slot assigned to the client this description is addressed to.
    pub applicant: PlayerId,
    pub host: PlayerId,
    pub map: String,
    /// Map variant selector (`VariantName`); `1` in every capture.
    pub variant: u32,
    pub enabled_mods: Vec<String>,
}

impl ServerInfo {
    pub fn decode(payload: &[u8]) -> Result<ServerInfo> {
        let mut c = Cursor::new(payload);
        expect_event(&mut c, Event::ServerInfo, "ServerInfo")?;
        let n = c.u64()?;
        if n > 64 {
            return Err(Error::ImplausibleLength {
                field: "players",
                value: n,
                at: 4,
            });
        }
        let mut players = Vec::with_capacity(n as usize);
        for _ in 0..n {
            players.push(PlayerInfo::read(&mut c)?);
        }
        let applicant = PlayerId::read(&mut c)?;
        let host = PlayerId::read(&mut c)?;
        let map = c.string()?;
        let variant = c.u32()?;
        let m = c.u64()?;
        if m > 1024 {
            return Err(Error::ImplausibleLength {
                field: "enabled_mods",
                value: m,
                at: c.pos - 8,
            });
        }
        let mut enabled_mods = Vec::with_capacity(m as usize);
        for _ in 0..m {
            enabled_mods.push(c.string()?);
        }
        Ok(ServerInfo {
            players,
            applicant,
            host,
            map,
            variant,
            enabled_mods,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(96);
        out.extend_from_slice(&(Event::ServerInfo as u32).to_le_bytes());
        out.extend_from_slice(&(self.players.len() as u64).to_le_bytes());
        for p in &self.players {
            p.write(&mut out);
        }
        self.applicant.write(&mut out);
        self.host.write(&mut out);
        put_string(&mut out, &self.map);
        out.extend_from_slice(&self.variant.to_le_bytes());
        out.extend_from_slice(&(self.enabled_mods.len() as u64).to_le_bytes());
        for m in &self.enabled_mods {
            put_string(&mut out, m);
        }
        out
    }
}

/// Decode a client's [`Event::Ready`] toggle.
pub fn decode_ready(payload: &[u8]) -> Result<bool> {
    let mut c = Cursor::new(payload);
    expect_event(&mut c, Event::Ready, "Ready")?;
    Ok(c.u8()? != 0)
}

/// Encode a [`Event::ReadyBroadcast`] for the given player.
pub fn encode_ready_broadcast(id: PlayerId, ready: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(13);
    out.extend_from_slice(&(Event::ReadyBroadcast as u32).to_le_bytes());
    id.write(&mut out);
    out.push(ready as u8);
    out
}

/// Build a [`Event::GarageStateCommit`] announcing `id`'s garage.
///
/// A commit is the player's `GarageState` body re-tagged with who owns it:
/// captured commit bytes from offset 12 equal captured `GarageState` bytes
/// from offset 4, and the inserted pair is the owner's `PlayerId`. The host
/// commits its own garage during the handshake, and a joining client stalls
/// at "Connecting" until it holds a commit for every player listed in
/// `ServerInfo`.
pub fn encode_garage_commit(id: PlayerId, garage_state: &[u8]) -> Vec<u8> {
    let body = garage_state.get(4..).unwrap_or(&[]);
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(&(Event::GarageStateCommit as u32).to_le_bytes());
    id.write(&mut out);
    out.extend_from_slice(body);
    out
}

/// Encode an [`Event::PlayerLeft`] broadcast for `id`.
pub fn encode_player_left(id: PlayerId) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&(Event::PlayerLeft as u32).to_le_bytes());
    id.write(&mut out);
    out
}

/// Decode an [`Event::PlayerLeft`] broadcast.
pub fn decode_player_left(payload: &[u8]) -> Result<PlayerId> {
    let mut c = Cursor::new(payload);
    expect_event(&mut c, Event::PlayerLeft, "PlayerLeft")?;
    PlayerId::read(&mut c)
}

/// Encode an [`Event::StartRace`] for `map`.
///
/// The five words after the map name were `0 1 0 0 1` in the only captured
/// start (`forest_long`, default variant); `RaceSettings` field names were not
/// recovered, so they are replayed verbatim.
pub fn encode_start_race(map: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + map.len());
    out.extend_from_slice(&(Event::StartRace as u32).to_le_bytes());
    put_string(&mut out, map);
    for word in [0u32, 1, 0, 0, 1] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

/// Encode an [`Event::RaceGo`]; it has no body.
pub fn encode_race_go() -> Vec<u8> {
    (Event::RaceGo as u32).to_le_bytes().to_vec()
}

/// A car that crossed the finish, as reported in [`Event::CrossedFinish`]
/// (client -> host) and relayed in [`Event::CarCrossedFinish`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Finish {
    /// `Entity` index from the car's `SpawnCar`.
    pub entity: u32,
    /// `Entity` generation; `1` in every capture.
    pub generation: u32,
    /// Race time in seconds, as the results table shows it.
    pub time: f64,
}

impl Finish {
    fn read(c: &mut Cursor<'_>) -> Result<Finish> {
        Ok(Finish {
            entity: c.u32()?,
            generation: c.u32()?,
            time: f64::from_bits(c.u64()?),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.entity.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.time.to_bits().to_le_bytes());
    }
}

/// Decode a client's [`Event::CrossedFinish`].
pub fn decode_crossed_finish(payload: &[u8]) -> Result<Finish> {
    let mut c = Cursor::new(payload);
    expect_event(&mut c, Event::CrossedFinish, "CrossedFinish")?;
    Finish::read(&mut c)
}

/// Encode a [`Event::CarCrossedFinish`] telling the other clients that
/// `id`'s car finished.
pub fn encode_car_crossed_finish(id: PlayerId, finish: &Finish) -> Vec<u8> {
    let mut out = Vec::with_capacity(28);
    out.extend_from_slice(&(Event::CarCrossedFinish as u32).to_le_bytes());
    id.write(&mut out);
    finish.write(&mut out);
    out
}

/// Decode a host's [`Event::CarCrossedFinish`] into the owner and its finish.
pub fn decode_car_crossed_finish(payload: &[u8]) -> Result<(PlayerId, Finish)> {
    let mut c = Cursor::new(payload);
    expect_event(&mut c, Event::CarCrossedFinish, "CarCrossedFinish")?;
    Ok((PlayerId::read(&mut c)?, Finish::read(&mut c)?))
}

/// Encode an [`Event::RaceEnd`]: body is the one `bool` a real host sent.
pub fn encode_race_end() -> Vec<u8> {
    let mut out = (Event::RaceEnd as u32).to_le_bytes().to_vec();
    out.push(1);
    out
}

/// A car's placement: rotation quaternion `(x, y, z, w)` then position.
pub type Pose = [f32; 7];

/// Encode an [`Event::SpawnCar`]: the host's instruction to create `id`'s car
/// in every client's world.
///
/// Layout from the capture: `u32 0`, owner `PlayerId`, the `Entity` the host
/// gave the car as `(u32 index, u32 generation)` (clients echo it in their
/// `CarState`), the player's `GarageState` body, then the start `Pose`. At
/// 472 bytes it always goes out chunked.
pub fn encode_spawn_car(id: PlayerId, entity: u32, garage_state: &[u8], pose: &Pose) -> Vec<u8> {
    let body = garage_state.get(4..).unwrap_or(&[]);
    let mut out = Vec::with_capacity(52 + body.len());
    out.extend_from_slice(&(Event::SpawnCar as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    id.write(&mut out);
    out.extend_from_slice(&entity.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(body);
    for f in pose {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// One car's physics snapshot as carried by [`Event::CarState`] (client ->
/// host, `state` then entity) and [`Event::CarStateBroadcast`] (host ->
/// clients, owner and entity then `state`). The 94-byte `state` is opaque to
/// a relay; it opens with the same pose as `SpawnCar` ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarState<'a> {
    pub state: &'a [u8],
    /// `Entity` index from the car's `SpawnCar`.
    pub entity: u32,
    /// `Entity` generation; `1` in every capture.
    pub generation: u32,
}

impl<'a> CarState<'a> {
    pub fn decode(payload: &'a [u8]) -> Result<CarState<'a>> {
        let mut c = Cursor::new(payload);
        expect_event(&mut c, Event::CarState, "CarState")?;
        let n = c.remaining().checked_sub(8).ok_or(Error::Truncated {
            wanted: 8,
            at: c.pos,
            len: payload.len(),
        })?;
        let state = c.take(n)?;
        let entity = c.u32()?;
        let generation = c.u32()?;
        Ok(CarState {
            state,
            entity,
            generation,
        })
    }

    /// Decode a broadcast into the owning player and its state.
    pub fn decode_broadcast(payload: &'a [u8]) -> Result<(PlayerId, CarState<'a>)> {
        let mut c = Cursor::new(payload);
        expect_event(&mut c, Event::CarStateBroadcast, "CarStateBroadcast")?;
        let id = PlayerId::read(&mut c)?;
        let entity = c.u32()?;
        let generation = c.u32()?;
        let state = c.take(c.remaining())?;
        Ok((
            id,
            CarState {
                state,
                entity,
                generation,
            },
        ))
    }

    /// Encode as the [`Event::CarStateBroadcast`] a host relays to the other
    /// participants.
    pub fn encode_broadcast(&self, id: PlayerId) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + self.state.len());
        out.extend_from_slice(&(Event::CarStateBroadcast as u32).to_le_bytes());
        id.write(&mut out);
        out.extend_from_slice(&self.entity.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(self.state);
        out
    }
}

/// Classify a raw datagram as game traffic rather than a coincidental packet.
///
/// Steam's networking sockets tag their datagrams with an ASCII banner; the
/// game's own protocol never produces a leading `s`.
pub fn looks_like_game_traffic(buf: &[u8]) -> bool {
    matches!(buf.first(), Some(&tag) if Kind::from_u8(tag).is_some())
}
