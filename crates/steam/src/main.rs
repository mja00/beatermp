//! `beatermp-steam`: advertise a `beatermp` host in the in-game Server list.
//!
//! Creates a public (or friends-only) Steam Matchmaking lobby as AppID 3711050
//! and a P2P listen socket, then relays each Steam peer to a local `beatermp`
//! UDP server. The game's Steam path carries raw `NetworkEvent` messages while
//! `beatermp` speaks the UDP `Frame` envelope, so [`beatermp_steam::translate`]
//! converts both ways and terminates `beatermp`'s reliability.
//!
//! Requires the Steam client running and logged in with an account that owns
//! BeaterCore. Run `beatermp` first (default `127.0.0.1:6237`), then this.
//! See `docs/notes/steam-browser.md`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::net::UdpSocket;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use beatermp_codec::{event_kind, ClientInfo, Event};
use beatermp_steam::translate::{Peer, ToSteam};
use steamworks::networking_sockets::NetConnection;
use steamworks::networking_types::{ListenSocketEvent, NetworkingConfigEntry, SendFlags};
use steamworks::{Client, LobbyType};

/// BeaterCore's Steam AppID.
const APP_ID: u32 = 3711050;

/// The port `beatermp` listens on unless told otherwise.
const DEFAULT_PORT: u16 = 6237;

/// Shown in the client's Server list; the game's host writes its own
/// "Server name:" field into the lobby's `name` key.
const DEFAULT_NAME: &str = "beatermp";

const USAGE: &str =
    "usage: beatermp-steam [--port N] [--name NAME] [--friends-only] [--data-file PATH]
  --port N         local beatermp UDP port to relay to (default 6237)
  --name NAME      lobby name shown in the in-game Server list (default beatermp)
  --friends-only   create a friends-only lobby instead of a public one
  --data-file PATH append one JSON line per lobby/identity event to PATH

Requires the Steam client running with an account that owns BeaterCore.";

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Optional JSONL sink for the lobby + identity data the bridge observes
/// (joins, leaves, and each client's `ClientInfo` name and enabled-mod count).
struct DataLog(Option<File>);

impl DataLog {
    fn open(path: Option<String>) -> Self {
        let file = path.map(|p| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .unwrap_or_else(|e| {
                    eprintln!("could not open data file {p}: {e}");
                    std::process::exit(1);
                })
        });
        DataLog(file)
    }

    fn line(&mut self, keys: &[(&str, String)]) {
        let Some(file) = self.0.as_mut() else {
            return;
        };
        let mut out = String::from("{");
        for (i, (k, v)) in keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(k);
            out.push_str("\":\"");
            out.push_str(&json_escape(v));
            out.push('"');
        }
        out.push_str("}\n");
        let _ = file.write_all(out.as_bytes());
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The remote SteamID of a connection, when Steam reports one.
fn steam_id_of(conn: &NetConnection) -> Option<u64> {
    conn.info()
        .ok()
        .and_then(|info| info.identity_remote())
        .and_then(|identity| identity.steam_id())
        .map(|id| id.raw())
}

/// One Steam connection and its private UDP socket to the local beatermp.
struct RuntimePeer {
    conn: steamworks::networking_sockets::NetConnection,
    udp: UdpSocket,
    peer: Peer,
}

fn main() {
    let mut port = DEFAULT_PORT;
    let mut friends_only = false;
    let mut name = DEFAULT_NAME.to_string();
    let mut data_file: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                    eprintln!("--port needs a number\n{USAGE}");
                    std::process::exit(2);
                })
            }
            "--name" => {
                name = args.next().unwrap_or_else(|| {
                    eprintln!("--name needs a value\n{USAGE}");
                    std::process::exit(2);
                })
            }
            "--friends-only" => friends_only = true,
            "--data-file" => {
                data_file = Some(args.next().unwrap_or_else(|| {
                    eprintln!("--data-file needs a path\n{USAGE}");
                    std::process::exit(2);
                }))
            }
            _ => {
                eprintln!("unexpected argument {arg:?}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let mut log = DataLog::open(data_file);

    let client = Client::init_app(APP_ID).unwrap_or_else(|e| {
        eprintln!(
            "failed to init Steamworks for AppID {APP_ID}: {e:?}\n\
             Is the Steam client running and logged in, and does it own BeaterCore?"
        );
        std::process::exit(1);
    });
    let me = client.user().steam_id();

    // Create the lobby and wait for Steam to answer.
    let lobby_type = if friends_only {
        LobbyType::FriendsOnly
    } else {
        LobbyType::Public
    };
    let (tx, rx) = mpsc::channel();
    client
        .matchmaking()
        .create_lobby(lobby_type, 6, move |res| {
            let _ = tx.send(res);
        });
    let lobby = wait_for(&client, &rx, Duration::from_secs(10)).unwrap_or_else(|| {
        eprintln!("Steam did not create a lobby in time");
        std::process::exit(1);
    });
    // The Server list renders GetLobbyData(lobby, "name") and falls back to
    // "Unnamed server"; a real host writes its "Server name:" field here.
    if !client.matchmaking().set_lobby_data(lobby, "name", &name) {
        eprintln!(
            "warning: Steam rejected the lobby name; the Server list will say \"Unnamed server\""
        );
    }
    println!(
        "beatermp-steam: lobby {} is {} and listed for AppID {APP_ID} as {name:?} owned by {}; relaying to 127.0.0.1:{port}",
        lobby.raw(),
        if friends_only { "friends-only" } else { "public" },
        me.raw(),
    );
    println!(
        "beatermp-steam: clients ConnectP2P to {}, so the joining game must run on a different Steam account",
        me.raw(),
    );
    log.line(&[
        ("ts", unix_seconds().to_string()),
        ("event", "lobby_created".into()),
        ("lobby", lobby.raw().to_string()),
        ("host_steam_id", me.raw().to_string()),
        ("name", name.clone()),
    ]);

    // Relay access is what carries P2P connections between peers that cannot
    // reach each other directly; ask for it before anyone dials in.
    client.networking_utils().init_relay_network_access();

    let sockets = client.networking_sockets();
    let listen = sockets
        .create_listen_socket_p2p(0, Vec::<NetworkingConfigEntry>::new())
        .unwrap_or_else(|_| {
            eprintln!("failed to create a P2P listen socket");
            std::process::exit(1);
        });

    let mut peers: HashMap<i64, RuntimePeer> = HashMap::new();
    let mut next_peer_id: i64 = 1;
    let mut buf = [0u8; 65536];

    loop {
        client.run_callbacks();

        while let Some(event) = listen.try_receive_event() {
            match event {
                ListenSocketEvent::Connecting(req) => {
                    if let Err(e) = req.accept() {
                        eprintln!("accept failed: {e:?}");
                    }
                }
                ListenSocketEvent::Connected(ev) => {
                    let id = next_peer_id;
                    next_peer_id += 1;
                    let conn = ev.take_connection();
                    let _ = conn.set_connection_user_data(id);
                    let steam_id = steam_id_of(&conn);
                    match UdpSocket::bind(("127.0.0.1", 0)) {
                        Ok(udp) => {
                            let _ = udp.set_read_timeout(Some(Duration::from_millis(2)));
                            println!("peer {id} connected");
                            log.line(&[
                                ("ts", unix_seconds().to_string()),
                                ("event", "connect".into()),
                                ("peer", id.to_string()),
                                (
                                    "steam_id",
                                    steam_id.map(|s| s.to_string()).unwrap_or_default(),
                                ),
                            ]);
                            peers.insert(
                                id,
                                RuntimePeer {
                                    conn,
                                    udp,
                                    peer: Peer::new(),
                                },
                            );
                        }
                        Err(e) => eprintln!("peer {id}: could not open a relay socket: {e}"),
                    }
                }
                ListenSocketEvent::Disconnected(ev) => {
                    let id = ev.user_data();
                    if peers.remove(&id).is_some() {
                        println!("peer {id} disconnected");
                        log.line(&[
                            ("ts", unix_seconds().to_string()),
                            ("event", "disconnect".into()),
                            ("peer", id.to_string()),
                        ]);
                    }
                }
            }
        }

        for (id, p) in peers.iter_mut() {
            // Steam -> beatermp.
            for msg in p.conn.receive_messages(16).unwrap_or_default() {
                // The join handshake carries the player's name and its
                // enabled-mod map; record both as they pass through.
                if let Ok(Event::ClientInfo) = event_kind(msg.data()) {
                    if let Ok(info) = ClientInfo::decode(msg.data()) {
                        log.line(&[
                            ("ts", unix_seconds().to_string()),
                            ("event", "client_info".into()),
                            ("peer", id.to_string()),
                            ("name", info.name),
                            ("mods", info.trailing.to_string()),
                        ]);
                    }
                }
                let datagram = p.peer.from_steam(msg.data(), unix_seconds());
                if let Err(e) = p.udp.send_to(&datagram, ("127.0.0.1", port)) {
                    eprintln!("peer {id}: relay to beatermp failed: {e}");
                }
            }
            // beatermp -> Steam.
            loop {
                match p.udp.recv_from(&mut buf) {
                    Ok((n, _)) => {
                        let (ack, messages) = p.peer.from_beatermp(&buf[..n]);
                        if let Some(ack) = ack {
                            let _ = p.udp.send_to(&ack, ("127.0.0.1", port));
                        }
                        for message in messages {
                            let (data, flags) = match &message {
                                ToSteam::Reliable(d) => (d, SendFlags::RELIABLE),
                                ToSteam::Unreliable(d) => (d, SendFlags::UNRELIABLE),
                            };
                            if let Err(e) = p.conn.send_message(data, flags) {
                                eprintln!("peer {id}: send to Steam failed: {e:?}");
                            }
                        }
                    }
                    Err(e)
                        if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(e) => {
                        eprintln!("peer {id}: relay from beatermp failed: {e}");
                        break;
                    }
                }
            }
        }

        // Keep Steam's callbacks flowing without spinning a core.
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Pump Steam callbacks until the create-lobby result arrives.
fn wait_for<T>(
    client: &Client,
    rx: &mpsc::Receiver<Result<T, steamworks::SteamError>>,
    timeout: Duration,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        client.run_callbacks();
        match rx.try_recv() {
            Ok(Ok(v)) => return Some(v),
            Ok(Err(e)) => {
                eprintln!("Steam returned an error: {e:?}");
                return None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
