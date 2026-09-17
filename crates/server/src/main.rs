//! A headless multiplayer server for BeaterCore.
//!
//! # Why this exists
//!
//! BeaterCore ships no dedicated server binary: the only way to host is to run
//! the full game, which needs an X display (winit panics without one), a GPU,
//! and about 1.3 cores plus 600 MB of RAM to sit on a menu. This binary speaks
//! the same UDP protocol without any of that.
//!
//! # What it implements
//!
//! The join handshake, the reliability layer (acks and retransmits), the lobby
//! Ready toggle, and relaying of everything else between joined clients. The
//! observed exchange it reproduces (see `beatermp-codec` for the wire format):
//!
//! ```text
//! client  -> Greeting                       server -> Greeting (echo)
//! client  -> Reliable ClientInfo            server -> Ack, Reliable ServerInfo, Reliable GarageStateCommit
//! client  -> Ack, Ack, Reliable GarageState server -> Ack
//! both    -> Unreliable Ping at 1 Hz, answered with a Pong echoing the clock
//! client  -> Reliable Ready(bool)           server -> Ack, ReadyBroadcast to the other clients
//! all ready                                 server -> ReadyBroadcast(host), StartRace, SpawnCar per car
//! client  -> Reliable Ready(true) at grid   server -> ReadyBroadcast to the others; once everyone
//!                                                     confirmed: ReadyBroadcast(host), RaceGo
//! client  -> Unreliable CarState at 20 Hz   server -> CarStateBroadcast to the other clients
//! client  -> Reliable CrossedFinish          server -> CarCrossedFinish to the other clients; once
//!                                                     everyone finished: CarCrossedFinish(host),
//!                                                     then RaceEnd and GarageStateCommit(host)
//! client  -> Reliable GarageState (worn)     server -> GarageStateCommit to the other clients
//! ```
//!
//! It does not simulate anything: the host car is a parked phantom that
//! "finishes" just behind the last real finisher so the results table is
//! complete, and a race also ends when a grace period after the first finish
//! runs out or the lobby empties.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use beatermp_codec::{
    broadcast_twin, broadcast_verbatim, clock_of, decode_chat, decode_crossed_finish,
    decode_disconnect, decode_ready, decode_visit_request, encode, encode_car_crossed_finish,
    encode_chat, encode_clock, encode_garage_commit, encode_garage_visit_broadcast,
    encode_lobby_change_map, encode_location_in_garage, encode_player_left, encode_race_end,
    encode_race_go, encode_ready_broadcast, encode_spawn_car, encode_start_race,
    encode_visit_garage_response, encode_with_sender, encode_with_sender_disc, event_discriminant,
    event_kind, parse, CarState, Chunk, ClientInfo, Event, Finish, Frame, Packet, PlayerId,
    PlayerInfo, Pose, RaceSettings, ServerInfo, CHUNK_SIZE, GREETING_ID,
};

/// The port the game hardcodes. It is a string literal in the binary, so a
/// client can only reach a server on this port unless the binary is patched.
const DEFAULT_PORT: u16 = 6237;

/// Ping cadence and retransmit interval measured from captures: 1 Hz.
const TICK: Duration = Duration::from_millis(1000);

/// Drop a client after this long without a datagram. Generous relative to the
/// 1 Hz ping so a brief stall does not evict anyone.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);

/// Give up on a reliable packet after this many sends; the peer is gone.
const MAX_RESENDS: u8 = 15;

/// The map every captured lobby advertised; used when none is given.
const DEFAULT_MAP: &str = "forest_long";

/// Grid poses per raceable scene, extracted from the game's `scene.ron`
/// files by `tools/re/spawns.py` so the server needs no game install.
const MAPS_TABLE: &str = include_str!("../maps.txt");

/// Off-grid parking poses for the phantom host, one row per grid slot, from
/// `tools/re/parking.py`. A map/variant missing here falls back to the grid.
const PARKING_TABLE: &str = include_str!("../parking.txt");

/// A car at rest sits this far above its spawn point's y on flat ground
/// (captured host: 0.754 over a spawn at 0.0). Spawning there instead of in
/// the terrain avoids a physics pop at the grid.
const SPAWN_LIFT: f32 = 0.75;

/// `AvatarState` as every real host and client has sent it; opaque here.
const AVATAR: [u8; 5] = [0; 5];

/// A stock car's `GarageState` payload, captured verbatim.
///
/// A joining client expects a garage commit for the host during the handshake;
/// committing this stock body keeps the handshake complete without modelling
/// vehicle state. Encoded as hex because it is opaque to this server.
const HOST_GARAGE_HEX: &str = concat!(
    "10000000",
    "01010000000025a9633f000000000b0000000000000072696d5f64656661756c74",
    "010100000000d6e64c3f000000000b0000000000000072696d5f64656661756c74",
    "0101000000006b974b3f000000000b0000000000000072696d5f64656661756c74",
    "010100000000082a4b3f000000000b0000000000000072696d5f64656661756c74",
    "03000000000000005a61707488502049b0e33f970768826f61e23f5c480f321c53",
    "e23fcb2b835c694ce13fdc2a6f6e2d90e13fcd90ca034d17e73f2a56d427234ce7",
    "3f1e4565572bb3e73f2f568258a1a2e73f01000000010000000100000001000000",
    "056bfdef7c02e73fd30aa93508d8ea3f76840b7acb91eb3f0fc7c33eea0ded3f05",
    "00000000000000ac2d0dff58feef3f9e792a1ec9caef3f0d9a7e95d454ee3f5ece",
    "0d78280eed3f7f7fc352b5efee3f0000000000000000000300000052b85e3f52b8",
    "1e3f0000803e0100000001000000000092ce6a3f4d29693fcabb423f22e43f3f04",
    "00000000000000010a3d20d21a7c7a40010a3d20d21a7c7a40010a3d20d21a7c7a",
    "40010a3d20d21a7c7a400044e3273e000000000000000000",
);

/// Entity the real host gave its own car; clients only echo it.
const HOST_ENTITY: u32 = 0x540;

/// The host car's physics snapshot at the grid, captured verbatim
/// (`CarStateBroadcast` for `HOST_ENTITY`, sitting still). Streamed at 20 Hz
/// during a race so clients see a parked host car; its pose is patched to the
/// current map's host grid slot in `start_race`.
const HOST_CAR_STATE_HEX: &str = concat!(
    "0c000000ffffffff010000004005000001000000",
    "0000000092fc7fbf0000000021aa273ca00f8e42bb3f3f3f00007843000000005c58b6be",
    "000000000000000000000000000000000000000017798e3e0000803f0000000000000000",
    "90880045010000000000000001000000000011c58e43",
);

/// Size of the car state inside `CarState`/`CarStateBroadcast`.
const CAR_STATE_LEN: usize = 94;

/// Byte offsets of the two fields `live_host_state` patches, from the layout
/// recovered out of the binary's `NetworkCarState` deserializer (`0x6ca030`)
/// and cross-checked against the captured state (offset 84 is `1` while held
/// at the grid; offset 90 is an f32 clock, `285.54` in the capture). Field
/// names were stripped from the binary, so these are positional:
///
/// ```text
/// 0..28   pose, `Isometry<f32, UnitQuaternion<f32>, 3>` (rotation xyzw, position xyz)
/// 28..40  [f32; 3]        40..52  [f32; 3]
/// 52..64  u32 x3          64..76  f32 x3
/// 76..84  usize
/// 84      bool            held at the grid until 3 s after RaceGo
/// 85      bool
/// 86..90  f32
/// 90..94  f32             sender clock
/// ```
const CAR_STATE_GRID_HOLD: usize = 84;
const CAR_STATE_CLOCK: usize = 90;

/// Cadence of `CarState` traffic in the capture.
const CAR_STATE_INTERVAL: Duration = Duration::from_millis(50);

/// A real host relayed ~100 Hz avatar updates from a garage visitor at
/// ~20 Hz, reliably; going faster only grows the retransmit queues.
const AVATAR_RELAY_INTERVAL: Duration = Duration::from_millis(50);

/// Pause between the last finish and `RaceEnd`, so finishers see the final
/// times before the results notepad replaces them. A real host waits for the
/// host player to press a button here.
const RACE_END_DELAY: Duration = Duration::from_secs(5);

/// End the race this long after the first finish even if someone is still
/// out on the track (stuck, crashed, or idling); otherwise one player could
/// hold the lobby forever.
const FINISH_GRACE: Duration = Duration::from_secs(180);

/// Variant names in the game's `VariantName` enum order; the wire value is
/// the 1-based position (Default=1 and Reverse=2 observed on a real host).
const VARIANTS: [&str; 5] = [
    "Default",
    "Reverse",
    "Alternative",
    "TimeAttack",
    "TimeAttackReverse",
];

/// Chat prefix a real client uses for its own system lines
/// (`SharedData::display_chat_system`, 0x5ecde0): an inline colour tag the
/// chat renderer understands, closed by `#{RES}`.
const CHAT_SYSTEM_COLOR: &str = "#{100100230}";
const CHAT_RESET: &str = "#{RES}";

/// Chat lines starting with this are commands for the server, not banter.
const COMMAND_PREFIX: char = '!';

/// A raceable scene variant, where its cars start, and how long its race is.
struct Map {
    name: String,
    /// 1-based index into [`VARIANTS`].
    variant: u32,
    spawns: Vec<Pose>,
    /// Laps for this entry of the rotation; `None` takes the server default.
    laps: Option<u32>,
}

impl Map {
    /// Look `spec` (`name[:variant][@laps]`, variant case-insensitive) up in
    /// the baked table. Slots are handed out in the scene's own spawn-point
    /// order, which is also what a real host appeared to do.
    fn load(spec: &str) -> Option<Map> {
        let (spec, laps) = match spec.split_once('@') {
            Some((spec, laps)) => (spec, Some(laps.parse().ok().filter(|n| *n > 0)?)),
            None => (spec, None),
        };
        let (name, variant_name) = spec.split_once(':').unwrap_or((spec, "Default"));
        let variant = VARIANTS
            .iter()
            .position(|v| v.eq_ignore_ascii_case(variant_name))? as u32
            + 1;
        let mut lines = MAPS_TABLE.lines();
        let count: usize = loop {
            let header = lines.next()?;
            let mut words = header.split(' ');
            if words.next() == Some(name) && words.next() == Some(VARIANTS[variant as usize - 1]) {
                break words.next()?.parse().ok()?;
            }
        };
        let spawns = lines
            .take(count)
            .map(|line| {
                let mut pose: Pose = [0.0; 7];
                for (slot, word) in pose.iter_mut().zip(line.split(' ')) {
                    *slot = word.parse().expect("maps.txt is generated");
                }
                pose[5] += SPAWN_LIFT;
                pose
            })
            .collect();
        Some(Map {
            name: name.to_string(),
            variant,
            spawns,
            laps,
        })
    }

    /// Every `name:variant` the table knows, as accepted by `--map`.
    fn names() -> impl Iterator<Item = String> {
        MAPS_TABLE
            .lines()
            .filter(|l| l.starts_with(|c: char| c.is_ascii_alphabetic()))
            .filter_map(|l| {
                let mut words = l.split(' ');
                Some(format!(
                    "{}:{}",
                    words.next()?,
                    words.next()?.to_ascii_lowercase()
                ))
            })
    }

    /// `name | Variant`, the way the game's chat line names a track.
    fn label(&self) -> String {
        format!("{} | {}", self.name, VARIANTS[self.variant as usize - 1])
    }

    /// A lobby larger than the grid wraps onto the first points; the game
    /// itself caps lobbies per map.
    fn grid_pose(&self, slot: usize) -> Pose {
        self.spawns[slot % self.spawns.len()]
    }

    /// Where the phantom host parks when `slot` cars race: an off-road
    /// shoulder pose at normal height from [`PARKING_TABLE`], or the next
    /// grid slot when the scene has no computed shoulder. Parking far under
    /// the terrain corrupted client physics (NaN poses, instant finishes).
    fn parking_pose(&self, slot: usize) -> Option<Pose> {
        let variant = VARIANTS[self.variant as usize - 1];
        let mut lines = PARKING_TABLE.lines();
        let count: usize = loop {
            let header = lines.next()?;
            let mut words = header.split(' ');
            if words.next() == Some(self.name.as_str()) && words.next() == Some(variant) {
                break words.next()?.parse().ok()?;
            }
        };
        let line = lines.nth(slot % count)?;
        let mut pose: Pose = [0.0; 7];
        for (word, slot) in line.split(' ').zip(pose.iter_mut()) {
            *slot = word.parse().expect("parking.txt is generated");
        }
        pose[5] += SPAWN_LIFT;
        Some(pose)
    }
}

/// A reliable packet waiting for its ack.
struct Pending {
    packet: Packet,
    last_sent: Instant,
}

/// What this server knows about a connected client.
struct Client {
    /// Identity as the client announced it; `None` until its `ClientInfo`.
    info: Option<ClientInfo>,
    /// Assigned slot. `u32::MAX` is the host in the game's own captures, so
    /// assigned client ids start at 1; this server never splits screens.
    id: PlayerId,
    last_seen: Instant,
    /// The client's `GarageState` payload, kept so it can be committed to
    /// every other lobby member. `Some` once the join handshake is complete
    /// and the client is in the lobby.
    garage: Option<Vec<u8>>,
    ready: bool,
    /// Spawned into the current race. A client that joins mid-race sits in
    /// the lobby and must not hold up the countdown or the race end.
    racing: bool,
    /// Crossed the finish in the current race.
    finished: bool,
    /// Sequence for the next reliable packet sent to this client. A real host
    /// numbers them from 1 per session, and a client that sees an unexpected
    /// number drops the packet, so this is per client.
    next_seq: u32,
    /// Position in the ordered stream of lobby events sent to this client.
    next_ordered: u32,
    /// Id for the next chunked message to this client.
    next_chunk: u32,
    /// Reliable packets sent but not yet acked.
    pending: Vec<Pending>,
    /// Sequences already processed from this client, so a retransmit is
    /// re-acked but not re-applied.
    processed: Vec<u32>,
    /// Chunked payloads from this client still being reassembled, by chunk id.
    inbound: HashMap<u32, Reassembly>,
    /// Clients waiting for this one's `VisitGarageResponse`; the response
    /// does not name the visitor, so the host has to remember who asked.
    visitors: Vec<SocketAddr>,
    /// Last avatar-state relay; a real host forwards ~100 Hz input at ~20 Hz.
    last_avatar: Instant,
    /// Rotation index this player wants raced next, from `!vote`. Dies with
    /// the client, so a leaver's vote never counts.
    vote: Option<usize>,
}

impl Client {
    fn new(client_id: u32) -> Self {
        Client {
            info: None,
            id: PlayerId {
                client_id,
                player_index: 1,
            },
            last_seen: Instant::now(),
            garage: None,
            ready: false,
            racing: false,
            finished: false,
            next_seq: 1,
            next_ordered: 0,
            next_chunk: 1,
            pending: Vec::new(),
            processed: Vec::new(),
            inbound: HashMap::new(),
            visitors: Vec::new(),
            last_avatar: Instant::now(),
            vote: None,
        }
    }

    fn name(&self) -> &str {
        self.info.as_ref().map_or("", |i| i.name.as_str())
    }

    /// Entity index for this client's car. Any value distinct from the host's
    /// works: it is a label clients echo back in `CarState`.
    fn entity(&self) -> u32 {
        0x1000 + self.id.client_id
    }
}

/// One chunked payload being collected from a client.
struct Reassembly {
    buf: Vec<u8>,
    /// Bytes received so far; complete once it reaches `buf.len()`.
    have: usize,
}

/// Decode a compile-time hex literal into bytes.
fn decode_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex literal"))
        .collect()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `m:ss.mmm`, the way the game's own results table shows a race time.
fn format_race_time(seconds: f64) -> String {
    let minutes = (seconds / 60.0).floor();
    format!("{minutes}:{:06.3}", seconds - minutes * 60.0)
}

struct Server {
    socket: UdpSocket,
    /// Name shown over the host's car in the lobby.
    name: String,
    /// Maps to race, in order; `map_index` is the one the lobby is on.
    maps: Vec<Map>,
    map_index: usize,
    /// Night, rain and the default lap count; a rotation entry may override
    /// the laps.
    settings: RaceSettings,
    clients: HashMap<SocketAddr, Client>,
    next_client_id: u32,
    /// Clock reported in this server's own pings.
    started: Instant,
    /// Set by `start_race`; cleared by `end_race` once everyone finished (or
    /// the grace ran out) and when the lobby empties.
    racing: bool,
    /// When the first car finished; starts the grace clock.
    first_finish: Option<Instant>,
    /// Race time of the latest finisher; the phantom host "finishes" with it.
    last_finish_time: f64,
    /// When to send `RaceEnd`, once finishing is settled.
    race_end_at: Option<Instant>,
    /// `CarStateBroadcast` payload for the phantom's grid slot on the current
    /// map; its clock and grid flag are patched per send.
    host_state: Vec<u8>,
    /// When `RaceGo` went out; the grid flag clears 3 s later like a real car.
    race_go_at: Option<Instant>,
}

impl Server {
    fn new(socket: UdpSocket, name: String, maps: Vec<Map>, settings: RaceSettings) -> Self {
        Server {
            socket,
            name,
            maps,
            map_index: 0,
            settings,
            clients: HashMap::new(),
            next_client_id: 1,
            started: Instant::now(),
            racing: false,
            first_finish: None,
            last_finish_time: 0.0,
            race_end_at: None,
            host_state: Vec::new(),
            race_go_at: None,
        }
    }

    fn map(&self) -> &Map {
        &self.maps[self.map_index]
    }

    /// Settings for a race on rotation entry `index`.
    fn settings_for(&self, index: usize) -> RaceSettings {
        RaceSettings {
            laps: self.maps[index].laps.unwrap_or(self.settings.laps),
            ..self.settings
        }
    }

    /// `name | Variant (N laps)`, for chat and logs.
    fn describe(&self, index: usize) -> String {
        let laps = self.settings_for(index).laps;
        let plural = if laps == 1 { "" } else { "s" };
        format!("{} ({laps} lap{plural})", self.maps[index].label())
    }

    /// The rotation entry the next race will run on: the lobby's map, or
    /// during a race whatever `end_race` will switch to.
    fn upcoming(&self) -> usize {
        if !self.racing {
            return self.map_index;
        }
        self.vote_leader()
            .map_or((self.map_index + 1) % self.maps.len(), |(index, _)| index)
    }

    /// Votes per rotation entry from everyone in the lobby.
    fn tally(&self) -> Vec<usize> {
        let mut counts = vec![0; self.maps.len()];
        for c in self.clients.values().filter(|c| c.garage.is_some()) {
            if let Some(index) = c.vote {
                counts[index] += 1;
            }
        }
        counts
    }

    /// The rotation entry with the most votes and its count, if anyone
    /// voted; a tie goes to the earlier entry.
    fn vote_leader(&self) -> Option<(usize, usize)> {
        let counts = self.tally();
        let (index, &n) = counts
            .iter()
            .enumerate()
            .max_by_key(|(i, n)| (**n, std::cmp::Reverse(*i)))?;
        (n > 0).then_some((index, n))
    }

    /// A rotation entry named by 1-based number or by a unique
    /// case-insensitive fragment of its label; `Err` carries the reason.
    fn find_map(&self, query: &str) -> Result<usize, String> {
        if let Ok(n) = query.parse::<usize>() {
            return (1..=self.maps.len())
                .contains(&n)
                .then_some(n - 1)
                .ok_or_else(|| format!("No map number {n}; see !maps"));
        }
        let query = query.to_ascii_lowercase();
        let matches: Vec<usize> = (0..self.maps.len())
            .filter(|&i| self.maps[i].label().to_ascii_lowercase().contains(&query))
            .collect();
        match matches.as_slice() {
            [index] => Ok(*index),
            [] => Err(format!("No map matches {query:?}; see !maps")),
            _ => Err(format!(
                "{} maps match {query:?}; vote by number, see !maps",
                matches.len()
            )),
        }
    }

    /// One chat line from the server to `addr`, coloured like a client's own
    /// system messages.
    fn say(&mut self, addr: SocketAddr, text: &str) {
        let line = encode_chat(&format!("{CHAT_SYSTEM_COLOR}{text}{CHAT_RESET}"));
        self.send_reliable(addr, line, true);
    }

    /// One chat line from the server to the whole lobby.
    fn say_all(&mut self, text: &str) {
        let line = encode_chat(&format!("{CHAT_SYSTEM_COLOR}{text}{CHAT_RESET}"));
        for addr in self.lobby_peers(None) {
            self.send_reliable(addr, line.clone(), true);
        }
        println!("chat: {text}");
    }

    /// Move the lobby to rotation entry `index`: LobbyChangeMap so minimaps
    /// and the client's "Host changed track" line follow, then the lap count,
    /// which the game never shows. Votes were for this switch, so they reset.
    fn set_map(&mut self, index: usize) {
        self.map_index = index;
        let change = encode_lobby_change_map(&self.map().name, self.map().variant);
        for addr in self.lobby_peers(None) {
            self.send_reliable(addr, change.clone(), true);
        }
        for c in self.clients.values_mut() {
            c.vote = None;
        }
        let text = format!(
            "Next race: {}. {COMMAND_PREFIX}vote picks another.",
            self.describe(index)
        );
        self.say_all(&text);
    }

    /// A chat line from a lobby member: relay it verbatim like a real host,
    /// then act on it if it is a `!command`. The client already prefixed
    /// its own name, so that is stripped before looking for the prefix.
    fn chat(&mut self, from: SocketAddr, payload: Vec<u8>) -> Option<String> {
        let line = match decode_chat(&payload) {
            Ok(line) => line,
            Err(e) => return Some(format!("bad chat from {from}: {e}")),
        };
        let c = self.clients.get(&from).filter(|c| c.garage.is_some())?;
        let text = line
            .strip_prefix(c.name())
            .and_then(|rest| rest.strip_prefix(": "))
            .unwrap_or(&line)
            .trim();
        let command = text.strip_prefix(COMMAND_PREFIX).map(str::to_string);
        let n = self.broadcast_reliable(from, payload);
        let Some(command) = command else {
            return Some(format!("chat relayed to {n} peer(s)"));
        };
        self.command(from, &command);
        Some(format!("chat command {command:?} handled"))
    }

    fn command(&mut self, from: SocketAddr, command: &str) {
        let (verb, arg) = command.split_once(' ').unwrap_or((command, ""));
        match verb.to_ascii_lowercase().as_str() {
            "help" => {
                // One chat line does not wrap; the full list ran off screen.
                self.say(from, "!maps shows the rotation, !next the upcoming race");
                self.say(
                    from,
                    "!vote N or !vote <map> picks the next map, !vote shows the tally",
                );
            }
            "maps" | "rotation" => {
                for index in 0..self.maps.len() {
                    let mark = if index == self.upcoming() { ">" } else { " " };
                    let line = format!("{mark}{}. {}", index + 1, self.describe(index));
                    self.say(from, &line);
                }
            }
            "next" => {
                let text = if self.racing {
                    format!(
                        "Racing {} now; next: {}",
                        self.map().label(),
                        self.describe(self.upcoming())
                    )
                } else {
                    format!("Next race: {}", self.describe(self.upcoming()))
                };
                self.say(from, &text);
            }
            "vote" => self.vote(from, arg.trim()),
            _ => {
                let text =
                    format!("Unknown command {COMMAND_PREFIX}{verb}; try {COMMAND_PREFIX}help");
                self.say(from, &text);
            }
        }
    }

    /// `!vote` alone shows the tally; with a map it records the vote and
    /// tells everyone. A majority of the lobby switches the map at once when
    /// nobody is racing; otherwise the leader wins when the race ends.
    fn vote(&mut self, from: SocketAddr, target: &str) {
        if target.is_empty() {
            let listed: Vec<String> = self
                .tally()
                .iter()
                .enumerate()
                .filter(|(_, n)| **n > 0)
                .map(|(i, n)| format!("{} {n}", self.maps[i].label()))
                .collect();
            let text = if listed.is_empty() {
                format!("No votes yet. {COMMAND_PREFIX}vote N or {COMMAND_PREFIX}vote <map> picks the next map; see {COMMAND_PREFIX}maps")
            } else {
                format!("Votes: {}", listed.join(", "))
            };
            self.say(from, &text);
            return;
        }
        let index = match self.find_map(target) {
            Ok(index) => index,
            Err(why) => return self.say(from, &why),
        };
        if !self.racing && index == self.map_index {
            let text = format!("{} is already next", self.maps[index].label());
            return self.say(from, &text);
        }
        let Some(c) = self.clients.get_mut(&from) else {
            return;
        };
        c.vote = Some(index);
        let name = c.name().to_string();
        let votes = self.tally()[index];
        let needed = self.lobby_peers(None).len() / 2 + 1;
        let text = format!(
            "{name} voted for {} ({votes}/{needed} needed)",
            self.maps[index].label()
        );
        self.say_all(&text);
        if !self.racing && votes >= needed {
            self.say_all("Vote passed.");
            self.set_map(index);
        }
    }

    /// Do what a real host does on Start Race: mark the host ready, announce
    /// the race, then spawn one car per participant (host first) into every
    /// client's world, all on the ordered stream.
    ///
    /// The host's ReadyBroadcast mirrors the capture (lobby_race_host.txt:860);
    /// the grid gate is satisfied separately by the host's grid confirm.
    fn start_race(&mut self) {
        let host_ready = encode_ready_broadcast(PlayerId::HOST, true);
        let settings = self.settings_for(self.map_index);
        let start = encode_start_race(&self.map().name, self.map().variant, &settings);
        let host_garage = decode_hex(HOST_GARAGE_HEX);
        // Park the phantom on a computed off-road shoulder when the scene has
        // one (falling back to the next grid slot); burying its chassis left
        // detached wheels and NaN'd the client's physics state.
        let client_count = self.clients.values().filter(|c| c.garage.is_some()).count();
        let host_pose = self
            .map()
            .parking_pose(client_count)
            .unwrap_or_else(|| self.map().grid_pose(client_count));
        let mut spawns = vec![encode_spawn_car(
            PlayerId::HOST,
            HOST_ENTITY,
            &host_garage,
            &host_pose,
        )];
        // Spawn and streamed state must agree on the host's grid pose.
        let mut host_state = decode_hex(HOST_CAR_STATE_HEX);
        let pose_at = host_state.len() - CAR_STATE_LEN;
        for (i, f) in host_pose.iter().enumerate() {
            host_state[pose_at + 4 * i..pose_at + 4 * i + 4].copy_from_slice(&f.to_le_bytes());
        }
        self.host_state = host_state;
        self.race_go_at = None;
        let mut participants: Vec<&Client> = self
            .clients
            .values()
            .filter(|c| c.garage.is_some())
            .collect();
        participants.sort_by_key(|c| c.id.client_id);
        for (slot, c) in participants.iter().enumerate() {
            let garage = c.garage.as_ref().expect("filtered");
            spawns.push(encode_spawn_car(
                c.id,
                c.entity(),
                garage,
                &self.map().grid_pose(slot),
            ));
        }
        let targets = self.lobby_peers(None);
        for addr in targets {
            self.send_reliable(addr, host_ready.clone(), true);
            self.send_reliable(addr, start.clone(), true);
            for spawn in &spawns {
                self.send_reliable(addr, spawn.clone(), true);
            }
        }
        for c in self.clients.values_mut() {
            c.racing = c.garage.is_some();
            c.ready = false;
            c.finished = false;
        }
        self.racing = true;
        self.first_finish = None;
        self.race_end_at = None;
        println!(
            "race started on {} with {} car(s)",
            self.map().label(),
            spawns.len()
        );
    }

    /// The phantom's state for this instant. A real car's state carries its
    /// sender clock and a "held at grid" flag that clears 3 s after RaceGo;
    /// a frozen copy left clients drawing the host as loose wheels.
    fn live_host_state(&self) -> Vec<u8> {
        let mut payload = self.host_state.clone();
        let state_at = payload.len() - CAR_STATE_LEN;
        let held = self
            .race_go_at
            .is_none_or(|t| t.elapsed() < Duration::from_secs(3));
        // Offset 84 is a bool; patching four bytes there would also overwrite
        // the low half of the f32 at 86.
        payload[state_at + CAR_STATE_GRID_HOLD] = held as u8;
        payload[state_at + CAR_STATE_CLOCK..state_at + CAR_STATE_CLOCK + 4]
            .copy_from_slice(&self.started.elapsed().as_secs_f32().to_le_bytes());
        payload
    }

    /// A client crossed the finish: tell the others as the host does, and
    /// schedule the end once nobody is left racing.
    fn car_finished(&mut self, from: SocketAddr, finish: Finish) {
        let Some(c) = self.clients.get_mut(&from) else {
            return;
        };
        c.finished = true;
        let id = c.id;
        let name = c.name().to_string();
        println!("{name} finished in {:.3} s", finish.time);
        let relay = encode_car_crossed_finish(id, &finish);
        for addr in self.lobby_peers(Some(from)) {
            self.send_reliable(addr, relay.clone(), false);
        }
        self.first_finish.get_or_insert_with(Instant::now);
        self.last_finish_time = finish.time;
        let place = self.clients.values().filter(|c| c.finished).count();
        let text = format!(
            "{name} finished P{place} in {}",
            format_race_time(finish.time)
        );
        self.say_all(&text);
        self.settle_finish();
    }

    /// Once every remaining racer has finished, the phantom host "finishes"
    /// too (so the results table shows no 0.000 row) and `RaceEnd` is
    /// scheduled. Called after a finish and after a racer leaves.
    fn settle_finish(&mut self) {
        if !self.racing || self.race_end_at.is_some() {
            return;
        }
        let all_finished = self
            .clients
            .values()
            .filter(|c| c.racing)
            .all(|c| c.finished);
        if !all_finished {
            return;
        }
        // A hair behind the last real finisher, so the results table lists
        // the phantom last instead of tied with a human.
        let host_finish = encode_car_crossed_finish(
            PlayerId::HOST,
            &Finish {
                entity: HOST_ENTITY,
                generation: 1,
                time: self.last_finish_time + 0.001,
            },
        );
        for addr in self.lobby_peers(None) {
            self.send_reliable(addr, host_finish.clone(), false);
        }
        self.race_end_at = Some(Instant::now() + RACE_END_DELAY);
    }

    /// Send everyone back to the lobby the way a real host does after the
    /// finish overlay: `RaceEnd`, then a fresh commit of the host garage.
    /// Clients answer with their own worn `GarageState`.
    fn end_race(&mut self, why: &str) {
        let race_end = encode_race_end();
        let host_garage = encode_garage_commit(PlayerId::HOST, &decode_hex(HOST_GARAGE_HEX));
        for addr in self.lobby_peers(None) {
            self.send_reliable(addr, race_end.clone(), true);
            self.send_reliable(addr, host_garage.clone(), true);
        }
        for c in self.clients.values_mut() {
            c.racing = false;
            c.ready = false;
            c.finished = false;
        }
        self.racing = false;
        self.first_finish = None;
        self.race_end_at = None;
        // Rotate for the next race: the vote leader if anyone voted, else the
        // next entry. `set_map` tells the lobby so minimaps and the "Host
        // changed track" chat line match what StartRace will load.
        let next = match self.vote_leader() {
            Some((index, votes)) => {
                let text = format!(
                    "Vote won by {} with {votes} vote(s)",
                    self.maps[index].label()
                );
                self.say_all(&text);
                index
            }
            None => (self.map_index + 1) % self.maps.len(),
        };
        self.set_map(next);
        println!("race over: {why}; next map {}", self.map().label());
    }

    fn send(&self, addr: SocketAddr, bytes: &[u8]) {
        // A failed send to one peer must not take down the server; the client
        // will be reaped by the timeout if it is truly gone.
        if let Err(e) = self.socket.send_to(bytes, addr) {
            eprintln!("send to {addr} failed: {e}");
        }
    }

    fn send_unreliable(&self, addr: SocketAddr, payload: Vec<u8>) {
        self.send(addr, &encode(&Frame::Unreliable(payload)));
    }

    /// Queue a reliable message: sent now and again every tick until acked.
    /// Payloads over `CHUNK_SIZE` go out as several packets that share one
    /// ordered index and a chunk id, each acked on its own.
    fn send_reliable(&mut self, addr: SocketAddr, payload: Vec<u8>, ordered: bool) {
        let Some(c) = self.clients.get_mut(&addr) else {
            return;
        };
        let ordered_index = if ordered {
            let i = c.next_ordered;
            c.next_ordered += 1;
            Some(i)
        } else {
            None
        };
        let chunked = payload.len() > CHUNK_SIZE;
        let chunk_id = c.next_chunk;
        if chunked {
            c.next_chunk += 1;
        }
        let total_size = payload.len() as u32;
        let count = payload.len().div_ceil(CHUNK_SIZE) as u16;
        let mut out = Vec::with_capacity(count as usize);
        for (i, piece) in payload.chunks(CHUNK_SIZE).enumerate() {
            let packet = Packet {
                payload: piece.to_vec(),
                seq: c.next_seq,
                resend: 0,
                sent_at: unix_seconds(),
                ordered_index,
                chunk: chunked.then_some(Chunk {
                    id: chunk_id,
                    offset: (i * CHUNK_SIZE) as u32,
                    total_size,
                    count,
                }),
            };
            c.next_seq += 1;
            out.push(encode(&Frame::Reliable(packet.clone())));
            c.pending.push(Pending {
                packet,
                last_sent: Instant::now(),
            });
        }
        for bytes in out {
            self.send(addr, &bytes);
        }
    }

    /// Lobby description for a joining client, in a real host's order:
    /// everyone already joined, then the host itself (a client stalls at
    /// "Connecting" without the host entry).
    fn server_info(&self, joiner: &Client) -> ServerInfo {
        let mut players: Vec<PlayerInfo> = self
            .clients
            .values()
            .filter(|c| c.id != joiner.id && c.garage.is_some())
            .map(|c| PlayerInfo {
                id: c.id,
                name: c.name().to_string(),
                avatar: AVATAR,
            })
            .collect();
        players.push(PlayerInfo {
            id: PlayerId::HOST,
            name: self.name.clone(),
            avatar: AVATAR,
        });
        ServerInfo {
            players,
            applicant: joiner.id,
            host: PlayerId::HOST,
            map: self.map().name.clone(),
            variant: self.map().variant,
            enabled_mods: Vec::new(),
        }
    }

    /// Handle one datagram. Returns a description of what happened, for logging.
    fn handle(&mut self, from: SocketAddr, buf: &[u8]) -> Option<String> {
        let frame = match parse(buf) {
            Ok(f) => f,
            Err(e) => {
                // Stray traffic on a public port is normal; log and ignore.
                return Some(format!("ignored {} bytes from {from}: {e}", buf.len()));
            }
        };

        // Any valid datagram proves the peer is alive and (re)creates its entry.
        if !self.clients.contains_key(&from) {
            let id = self.next_client_id;
            self.next_client_id += 1;
            self.clients.insert(from, Client::new(id));
            println!("client connected: {from} (client id {id})");
        }
        let client = self.clients.get_mut(&from).expect("inserted above");
        client.last_seen = Instant::now();

        match frame {
            // The greeting is a bare connectivity check: echo it unchanged.
            Frame::Greeting(id) => {
                self.send(from, &encode(&Frame::Greeting(id)));
                if id != GREETING_ID {
                    return Some(format!("greeting with unexpected id {id} echoed"));
                }
                Some("greeting echoed".to_string())
            }

            Frame::Ack(seq) => {
                let before = client.pending.len();
                client.pending.retain(|p| p.packet.seq != seq);
                if client.pending.len() == before {
                    return Some(format!("ack {seq} for nothing pending"));
                }
                None
            }

            Frame::Unreliable(payload) => self.handle_unreliable(from, payload),

            Frame::Reliable(p) => {
                // Ack first, unconditionally: a lost ack is why the peer
                // retransmits, and a retransmit must not be applied twice.
                self.send(from, &encode(&Frame::Ack(p.seq)));
                let client = self.clients.get_mut(&from).expect("present");
                if client.processed.contains(&p.seq) {
                    return Some(format!("re-acked seq {} (resend {})", p.seq, p.resend));
                }
                client.processed.push(p.seq);
                if client.processed.len() > 256 {
                    client.processed.remove(0);
                }
                let payload = match p.chunk {
                    None => p.payload,
                    Some(chunk) => {
                        // Pieces are acked individually and deduplicated by
                        // seq above, so byte counting is enough to know when
                        // the whole event is in.
                        let total = chunk.total_size as usize;
                        let at = chunk.offset as usize;
                        if at + p.payload.len() > total {
                            return Some(format!(
                                "bad chunk {} from {from}: piece past end",
                                chunk.id
                            ));
                        }
                        let r = client
                            .inbound
                            .entry(chunk.id)
                            .or_insert_with(|| Reassembly {
                                buf: vec![0; total],
                                have: 0,
                            });
                        r.buf[at..at + p.payload.len()].copy_from_slice(&p.payload);
                        r.have += p.payload.len();
                        if r.have < total {
                            return None;
                        }
                        client.inbound.remove(&chunk.id).expect("present").buf
                    }
                };
                self.handle_event(from, payload)
            }
        }
    }

    fn handle_unreliable(&mut self, from: SocketAddr, payload: Vec<u8>) -> Option<String> {
        match event_kind(&payload) {
            Ok(Event::Ping) => {
                // Echo the clock we were just given, not our own.
                let clock = clock_of(&payload).ok()?;
                self.send_unreliable(from, encode_clock(Event::Pong, clock));
                None
            }
            Ok(Event::Pong) => None,
            Ok(Event::CarState) => {
                let c = self.clients.get(&from).filter(|c| c.garage.is_some())?;
                let broadcast = match CarState::decode(&payload) {
                    Ok(state) => state.encode_broadcast(c.id),
                    Err(e) => return Some(format!("bad CarState from {from}: {e}")),
                };
                self.relay_unreliable(from, &broadcast);
                None
            }
            Ok(Event::UpdateAvatarState) => {
                let c = self.clients.get_mut(&from).filter(|c| c.garage.is_some())?;
                if c.last_avatar.elapsed() < AVATAR_RELAY_INTERVAL {
                    return None;
                }
                c.last_avatar = Instant::now();
                let id = c.id;
                self.broadcast_reliable(
                    from,
                    encode_with_sender(Event::UpdateAvatarStateBroadcast, id, &payload),
                );
                None
            }
            kind => {
                if let Ok(disc) = event_discriminant(&payload) {
                    if let Some((twin, n)) = self.relay_twin(from, disc, &payload) {
                        return Some(format!(
                            "unreliable event {disc} -> broadcast {twin} to {n} peer(s)"
                        ));
                    }
                    if !broadcast_verbatim(disc) {
                        return Some(format!("consumed unreliable event {disc} (no twin)"));
                    }
                }
                let n = self.relay_unreliable(from, &payload);
                match kind {
                    Ok(k) => Some(format!("relayed unreliable {k:?} to {n} peer(s)")),
                    Err(_) => Some(format!("relayed unknown unreliable event to {n} peer(s)")),
                }
            }
        }
    }

    fn handle_event(&mut self, from: SocketAddr, payload: Vec<u8>) -> Option<String> {
        let kind = event_kind(&payload);
        match kind {
            Ok(Event::ClientInfo) => {
                let info = match ClientInfo::decode(&payload) {
                    Ok(i) => i,
                    Err(e) => return Some(format!("bad ClientInfo from {from}: {e}")),
                };
                let client = self.clients.get_mut(&from).expect("present");
                let id = client.id;
                println!(
                    "{from} identifies as {:?} (client id {})",
                    info.name, id.client_id
                );
                let joined = info.encode_joined(id);
                client.info = Some(info);

                // Mirror a real host: describe the lobby, commit the host
                // garage, then one commit per player already in it, all
                // retransmitted until acked. Existing members are told about
                // the newcomer now; its garage follows once it arrives.
                let server_info = self.server_info(&self.clients[&from]).encode();
                self.send_reliable(from, server_info, false);
                let mut commits = vec![encode_garage_commit(
                    PlayerId::HOST,
                    &decode_hex(HOST_GARAGE_HEX),
                )];
                commits.extend(
                    self.clients
                        .values()
                        .filter(|c| c.id != id)
                        .filter_map(|c| {
                            let garage = c.garage.as_ref()?;
                            Some(encode_garage_commit(c.id, garage))
                        }),
                );
                let n = commits.len();
                for commit in commits {
                    self.send_reliable(from, commit, false);
                }
                for addr in self.lobby_peers(Some(from)) {
                    self.send_reliable(addr, joined.clone(), false);
                }
                Some(format!(
                    "client info -> lobby described, {n} garage commit(s)"
                ))
            }

            Ok(Event::GarageState) => {
                // The client's garage completes the join, and comes again after
                // each race with worn conditions. Its body is opaque here; it
                // is kept so it can be committed to everyone else, which is
                // how they see the newcomer's (or refreshed) car.
                let client = self.clients.get_mut(&from).expect("present");
                let joining = client.garage.is_none();
                client.garage = Some(payload.clone());
                let id = client.id;
                if joining {
                    println!(
                        "client joined lobby: {from} ({}, client id {})",
                        client.name(),
                        id.client_id
                    );
                }
                let n = self.broadcast_reliable(from, encode_garage_commit(id, &payload));
                if joining {
                    let text = format!(
                        "Next race: {}. Chat {COMMAND_PREFIX}help for commands.",
                        self.describe(self.upcoming())
                    );
                    self.say(from, &text);
                }
                Some(format!("garage state -> committed to {n} peer(s)"))
            }

            Ok(Event::Chat) => self.chat(from, payload),

            Ok(Event::CrossedFinish) => {
                let finish = match decode_crossed_finish(&payload) {
                    Ok(f) => f,
                    Err(e) => return Some(format!("bad CrossedFinish from {from}: {e}")),
                };
                if !self.racing {
                    return Some("crossed finish outside a race (ignored)".to_string());
                }
                self.car_finished(from, finish);
                Some(format!("crossed finish at {:.3} s", finish.time))
            }

            Ok(Event::Ready) => {
                let ready = match decode_ready(&payload) {
                    Ok(r) => r,
                    Err(e) => return Some(format!("bad Ready from {from}: {e}")),
                };
                let client = self.clients.get_mut(&from).expect("present");
                let changed = client.ready != ready;
                client.ready = ready;
                let broadcast = encode_ready_broadcast(client.id, ready);
                let n = self.broadcast_reliable(from, broadcast);
                if self.racing {
                    // The grid "press any button" confirm reuses Ready. A real
                    // host confirms last and then sends its own ReadyBroadcast
                    // plus RaceGo (bccap5 host lines 7799-7800); the phantom
                    // host confirms the moment the last client does. A late
                    // joiner's lobby Ready lands here too and just waits.
                    let racer = self.clients.get(&from).is_some_and(|c| c.racing);
                    let all_confirmed = self.clients.values().filter(|c| c.racing).all(|c| c.ready);
                    if !racer {
                        return Some(format!(
                            "lobby ready={ready} during a race -> broadcast to {n} peer(s)"
                        ));
                    }
                    if changed && all_confirmed {
                        let host_ready = encode_ready_broadcast(PlayerId::HOST, true);
                        let go = encode_race_go();
                        self.race_go_at = Some(Instant::now());
                        for addr in self.lobby_peers(None) {
                            self.send_reliable(addr, host_ready.clone(), true);
                            self.send_reliable(addr, go.clone(), false);
                        }
                        return Some(format!(
                            "race confirm ready={ready} -> everyone confirmed, go"
                        ));
                    }
                    return Some(format!(
                        "race confirm ready={ready} -> broadcast to {n} peer(s)"
                    ));
                }
                let joined: Vec<&Client> = self
                    .clients
                    .values()
                    .filter(|c| c.garage.is_some())
                    .collect();
                if !joined.is_empty() && joined.iter().all(|c| c.ready) {
                    self.start_race();
                    return Some(format!("ready={ready} -> everyone ready, race started"));
                }
                Some(format!("ready={ready} -> broadcast to {n} peer(s)"))
            }

            Ok(Event::Disconnect) => {
                // A real host also keeps retransmitting PlayerLeft to the
                // leaver, which never acks; dropping it here is equivalent.
                // The body is a DisconnectReason (0 none, 1 kick, 2 timed_out,
                // 3 host_left, 4 ban, 5 player_limit); every capture is 0.
                let reason = decode_disconnect(&payload).unwrap_or(0);
                self.remove_client(from, "disconnected");
                Some(format!("disconnect (reason {reason}) -> player left"))
            }

            Ok(Event::RequestVisitGarage) => {
                let owner = match decode_visit_request(&payload) {
                    Ok(id) => id,
                    Err(e) => return Some(format!("bad RequestVisitGarage from {from}: {e}")),
                };
                let visitor = self.clients.get(&from)?.id;
                if owner == PlayerId::HOST {
                    let response =
                        encode_visit_garage_response(PlayerId::HOST, &decode_hex(HOST_GARAGE_HEX));
                    self.send_reliable(from, response, true);
                } else {
                    // Clients answer any request they receive, so only the
                    // owner may see it; the reply comes back without a
                    // visitor id, hence the note of who is waiting.
                    let Some((&owner_addr, owner_client)) =
                        self.clients.iter_mut().find(|(_, c)| c.id == owner)
                    else {
                        return Some(format!("visit request for unknown player {owner:?}"));
                    };
                    owner_client.visitors.push(from);
                    self.send_reliable(owner_addr, payload, true);
                }
                let entered = encode_garage_visit_broadcast(visitor, owner);
                let location = encode_location_in_garage(visitor, owner);
                for addr in self.lobby_peers(Some(from)) {
                    self.send_reliable(addr, entered.clone(), true);
                    self.send_reliable(addr, location.clone(), true);
                }
                Some(format!("visiting garage of {owner:?}"))
            }

            Ok(Event::VisitGarageResponse) => {
                let visitors = std::mem::take(&mut self.clients.get_mut(&from)?.visitors);
                for addr in &visitors {
                    self.send_reliable(*addr, payload.clone(), true);
                }
                Some(format!(
                    "garage response forwarded to {} visitor(s)",
                    visitors.len()
                ))
            }

            // Client events whose broadcast twin is the same body tagged
            // with the sender.
            Ok(Event::UpdateLocation) | Ok(Event::StopGarageVisit) => {
                let id = self.clients.get(&from).filter(|c| c.garage.is_some())?.id;
                let twin = if kind == Ok(Event::UpdateLocation) {
                    Event::UpdateLocationBroadcast
                } else {
                    Event::StopGarageVisitBroadcast
                };
                let n = self.broadcast_reliable(from, encode_with_sender(twin, id, &payload));
                Some(format!("{:?} -> {twin:?} to {n} peer(s)", kind.ok()?))
            }

            // Everything else, including events this server does not model,
            // is handled the way a real host does: a client event with a
            // `*Broadcast` twin is re-tagged with the sender's `PlayerId`
            // (`encode_with_sender_disc`), and the one verbatim event (the
            // `String`, variant 0) is forwarded as-is. Any other client event
            // is consumed, never broadcast -- forwarding the client-role event
            // would put something no peer expects on the wire.
            _ => {
                if let Ok(disc) = event_discriminant(&payload) {
                    if let Some((twin, n)) = self.relay_twin(from, disc, &payload) {
                        return Some(format!("event {disc} -> broadcast {twin} to {n} peer(s)"));
                    }
                    if !broadcast_verbatim(disc) {
                        return Some(format!("consumed event {disc} (no broadcast twin)"));
                    }
                }
                let n = self.broadcast_reliable(from, payload);
                match kind {
                    Ok(k) => Some(format!("relayed {k:?} to {n} peer(s)")),
                    Err(e) => Some(format!("relayed unknown event to {n} peer(s) ({e})")),
                }
            }
        }
    }

    /// Everyone in the lobby, optionally minus one address.
    fn lobby_peers(&self, except: Option<SocketAddr>) -> Vec<SocketAddr> {
        self.clients
            .iter()
            .filter(|(addr, c)| Some(**addr) != except && c.garage.is_some())
            .map(|(addr, _)| *addr)
            .collect()
    }

    /// Forget a client and tell the lobby its slot is gone, if it ever had one.
    fn remove_client(&mut self, addr: SocketAddr, why: &str) {
        let Some(c) = self.clients.remove(&addr) else {
            return;
        };
        println!(
            "client {why}: {addr} ({}, client id {})",
            c.name(),
            c.id.client_id
        );
        if c.garage.is_some() {
            let left = encode_player_left(c.id);
            for peer in self.lobby_peers(None) {
                self.send_reliable(peer, left.clone(), false);
            }
        }
        if self.racing && self.lobby_peers(None).is_empty() {
            self.end_race("lobby empty");
        } else if self.racing {
            // The leaver may have been the last one still driving.
            self.settle_finish();
        }
    }

    fn broadcast_reliable(&mut self, from: SocketAddr, payload: Vec<u8>) -> usize {
        let targets = self.lobby_peers(Some(from));
        for addr in &targets {
            self.send_reliable(*addr, payload.clone(), true);
        }
        targets.len()
    }

    /// If `disc` is a client event with a `*Broadcast` twin, re-tag it with the
    /// sender's `PlayerId` and send it to the other lobby members. Returns the
    /// twin and how many peers it went to, or `None` when the event has no twin
    /// (the host would not broadcast it) or the sender is not in the lobby.
    ///
    /// Both the reliable and the unreliable receive paths use this: the output
    /// is what the host's `broadcast_equivalent` would produce, regardless of
    /// how the client frame arrived.
    fn relay_twin(&mut self, from: SocketAddr, disc: u32, payload: &[u8]) -> Option<(u32, usize)> {
        let twin = broadcast_twin(disc)?;
        let id = self.clients.get(&from).filter(|c| c.garage.is_some())?.id;
        let n = self.broadcast_reliable(from, encode_with_sender_disc(twin, id, payload));
        Some((twin, n))
    }

    fn relay_unreliable(&self, from: SocketAddr, payload: &[u8]) -> usize {
        let targets = self.lobby_peers(Some(from));
        let bytes = encode(&Frame::Unreliable(payload.to_vec()));
        for addr in &targets {
            self.send(*addr, &bytes);
        }
        targets.len()
    }

    /// Once a second: evict silent clients, retransmit unacked packets, ping.
    fn tick(&mut self) {
        let now = Instant::now();
        let stale: Vec<SocketAddr> = self
            .clients
            .iter()
            .filter(|(_, c)| now.duration_since(c.last_seen) > CLIENT_TIMEOUT)
            .map(|(a, _)| *a)
            .collect();
        for addr in stale {
            self.remove_client(addr, "timed out");
        }

        let mut resends: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        for (addr, c) in self.clients.iter_mut() {
            c.pending.retain(|p| p.packet.resend < MAX_RESENDS);
            for p in c.pending.iter_mut() {
                if now.duration_since(p.last_sent) < TICK {
                    continue;
                }
                p.packet.resend += 1;
                p.last_sent = now;
                resends.push((*addr, encode(&Frame::Reliable(p.packet.clone()))));
            }
        }
        for (addr, bytes) in resends {
            self.send(addr, &bytes);
        }

        let ping = encode(&Frame::Unreliable(encode_clock(
            Event::Ping,
            self.started.elapsed().as_secs_f32(),
        )));
        for addr in self.clients.keys() {
            self.send(*addr, &ping);
        }
    }

    fn run(&mut self) -> std::io::Result<()> {
        // Short read timeout so timers stay on schedule without a second
        // thread; the loop wakes, fires what is due, and goes back to waiting.
        self.socket
            .set_read_timeout(Some(Duration::from_millis(10)))?;

        let mut buf = [0u8; 65536];
        let mut next_tick = Instant::now() + TICK;
        let mut next_car_state = Instant::now();

        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let Some(what) = self.handle(from, &buf[..n]) {
                        println!("{from} ({n}B): {what}");
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }

            let now = Instant::now();
            if now >= next_tick {
                self.tick();
                next_tick = now + TICK;
            }
            if self.racing && now >= next_car_state {
                let frame = encode(&Frame::Unreliable(self.live_host_state()));
                for addr in self.lobby_peers(None) {
                    self.send(addr, &frame);
                }
                next_car_state = now + CAR_STATE_INTERVAL;
            }
            if self.racing {
                let grace_out = self
                    .first_finish
                    .is_some_and(|t| now.duration_since(t) >= FINISH_GRACE);
                if self.race_end_at.is_some_and(|t| now >= t) {
                    self.end_race("everyone finished");
                } else if grace_out {
                    self.end_race("finish grace expired");
                }
            }
        }
    }
}

const USAGE: &str = "usage: beatermp [--port N] [--name NAME] [--map MAP[:VARIANT][@LAPS]]... [--laps N] [--night] [--rain] [--list-maps]
  --port N     UDP port (default 6237, the only one unpatched clients reach)
  --name NAME  host name shown in the lobby (default beatermp)
  --map MAP    map to race, optionally with a variant and lap count such as
               forest_long:reverse@3; repeat to rotate through several in that
               order (default forest_long). Players pick the next one with !vote
  --laps N     laps for maps without their own count (default 1)
  --night      race at night
  --rain       race in the rain
  --list-maps  print the maps and variants this build knows and exit";

fn main() {
    let mut port = DEFAULT_PORT;
    let mut name = "beatermp".to_string();
    let mut maps: Vec<Map> = Vec::new();
    let mut settings = RaceSettings::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next().unwrap_or_else(|| {
                eprintln!("{arg} needs a value\n{USAGE}");
                std::process::exit(2);
            })
        };
        match arg.as_str() {
            "--port" => {
                port = value().parse().unwrap_or_else(|_| {
                    eprintln!("port must be a number\n{USAGE}");
                    std::process::exit(2);
                })
            }
            "--name" => name = value(),
            "--map" => {
                let map = value();
                maps.push(Map::load(&map).unwrap_or_else(|| {
                    eprintln!("unknown map {map:?}; see --list-maps");
                    std::process::exit(2);
                }));
            }
            "--laps" => {
                settings.laps = value().parse().ok().filter(|n| *n > 0).unwrap_or_else(|| {
                    eprintln!("laps must be a positive number\n{USAGE}");
                    std::process::exit(2);
                })
            }
            "--night" => settings.night = true,
            "--rain" => settings.rain = true,
            "--list-maps" => {
                for name in Map::names() {
                    println!("{name}");
                }
                return;
            }
            _ => {
                eprintln!("unexpected argument {arg:?}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    if maps.is_empty() {
        maps.push(Map::load(DEFAULT_MAP).expect("default map is in the table"));
    }

    let socket = UdpSocket::bind(("0.0.0.0", port)).unwrap_or_else(|e| {
        eprintln!("failed to bind 0.0.0.0:{port}: {e}");
        std::process::exit(1);
    });

    let rotation: Vec<String> = maps
        .iter()
        .map(|m| format!("{} x{}", m.label(), m.laps.unwrap_or(settings.laps)))
        .collect();
    println!(
        "beatermp listening on 0.0.0.0:{port} as {name:?}, rotation {}{}{}",
        rotation.join(", "),
        if settings.night { ", night" } else { "" },
        if settings.rain { ", rain" } else { "" },
    );

    let mut server = Server::new(socket, name, maps, settings);
    if let Err(e) = server.run() {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The baked parking table must line up with the grid table it was
    /// generated from: same slot count per block, finite poses, and every
    /// pose actually moved off its grid slot. A scene without a block falls
    /// back to the grid.
    #[test]
    fn parking_table_matches_grid_table() {
        let mut parked = 0;
        for spec in Map::names() {
            let map = Map::load(&spec).unwrap();
            let Some(first) = map.parking_pose(0) else {
                continue;
            };
            parked += 1;
            for slot in 0..map.spawns.len() {
                let pose = map.parking_pose(slot).unwrap();
                let grid = map.grid_pose(slot);
                assert!(pose.iter().all(|v| v.is_finite()), "{spec} slot {slot}");
                // The generator prints 6 significant digits.
                for i in [0, 1, 2, 3, 5] {
                    assert!(
                        (pose[i] - grid[i]).abs() < 1e-3,
                        "{spec} slot {slot} keeps grid rotation and height"
                    );
                }
                let off = ((pose[4] - grid[4]).powi(2) + (pose[6] - grid[6]).powi(2)).sqrt();
                assert!(off >= 4.9, "{spec} slot {slot} only {off} m off the grid");
            }
            // Blocks wrap like the grid does.
            assert_eq!(map.parking_pose(map.spawns.len()), Some(first));
        }
        assert!(parked > 0);
        assert!(Map::load("oval").unwrap().parking_pose(0).is_none());
    }
}
