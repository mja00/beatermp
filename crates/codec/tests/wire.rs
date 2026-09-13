//! Codec tests driven by real captured datagrams.
//!
//! The fixtures are verbatim slices of `tools/udpsniff` output taken from live
//! host/client sessions, restricted to the game's server socket. Each line is
//! `<ts> <S|R> <fd> <len> <peer> <hex>`; only the direction and hex are used.
//!
//! * `host_fd87` / `client_fd87`: join handshake plus ~40 s of idle lobby.
//! * `lobby_race_*`: join, both players toggle Ready, host starts the race,
//!   ~30 s of racing. Exercises acks, retransmits, ordered indices, chunking
//!   and the Unreliable car-state stream.
//! * `finish_host`: real host, forest_long trimmed to its finish gate; both
//!   cars cross, the host presses on, everyone returns to the lobby.
//! * `garage_visit_host`: real host with two clients; the first visits the
//!   host's garage from the lobby, walks around, returns to the hub.
//!
//! These tests fail if the framing is wrong in any way a hand-written encoder
//! could get wrong: field widths, field order, option tags, and the trailing
//! `Packet` bookkeeping block.

use beatermp_codec::{
    broadcast_twin, broadcast_verbatim, clock_of, decode_car_crossed_finish, decode_crossed_finish,
    decode_disconnect, decode_player_left, decode_ready, decode_visit_request, encode,
    encode_car_crossed_finish, encode_garage_commit, encode_garage_visit_broadcast,
    encode_lobby_change_map, encode_location_in_garage, encode_player_left, encode_race_end,
    encode_race_go, encode_ready_broadcast, encode_spawn_car, encode_start_race,
    encode_visit_garage_response, encode_with_sender, encode_with_sender_disc, event_kind, parse,
    CarState, ClientInfo, Event, Frame, Packet, PlayerId, Pose, RaceSettings, ServerInfo,
    CHUNK_SIZE, GREETING_ID,
};

const FIXTURES: [&str; 9] = [
    "host_fd87.txt",
    "client_fd87.txt",
    "lobby_race_host.txt",
    "lobby_race_client.txt",
    "second_join_host.txt",
    "leave_host.txt",
    "finish_host.txt",
    "garage_visit_host.txt",
    "race_settings_host.txt",
];

/// `(sent_by_this_side, bytes)` for every datagram in a fixture.
fn fixture(name: &str) -> Vec<(bool, Vec<u8>)> {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let sent = it.nth(1)? == "S";
            let hex = it.nth(3)?;
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            Some((sent, bytes))
        })
        .collect()
}

fn reliable(raw: &[u8]) -> Option<Packet> {
    match parse(raw) {
        Ok(Frame::Reliable(p)) => Some(p),
        _ => None,
    }
}

/// Every captured datagram must parse, and re-encoding a parsed frame must
/// reproduce the original bytes exactly.
#[test]
fn captured_datagrams_round_trip() {
    for name in FIXTURES {
        let datagrams = fixture(name);
        assert!(!datagrams.is_empty(), "{name} fixture is empty");
        for (_, raw) in datagrams {
            let frame = parse(&raw).unwrap_or_else(|e| panic!("{name}: parse {raw:02x?}: {e}"));
            let reencoded = encode(&frame);
            assert_eq!(
                reencoded, raw,
                "{name}: round trip mismatch\n  in : {raw:02x?}\n  out: {reencoded:02x?}"
            );
        }
    }
}

/// Every Reliable packet a side receives is answered with `Ack(seq)`, and
/// every Reliable packet a side sends is eventually acked by the peer. This
/// is the contract a server must honour or the client retransmits forever.
#[test]
fn every_reliable_packet_is_acked() {
    for name in FIXTURES {
        let mut sent_seqs = Vec::new();
        let mut received_seqs = Vec::new();
        let mut acks_sent = Vec::new();
        let mut acks_received = Vec::new();
        for (sent, raw) in fixture(name) {
            match parse(&raw).unwrap() {
                Frame::Reliable(p) if sent => sent_seqs.push(p.seq),
                Frame::Reliable(p) => received_seqs.push(p.seq),
                Frame::Ack(s) if sent => acks_sent.push(s),
                Frame::Ack(s) => acks_received.push(s),
                _ => {}
            }
        }
        assert!(!sent_seqs.is_empty(), "{name}: no reliable traffic");
        for s in &received_seqs {
            assert!(
                acks_sent.contains(s),
                "{name}: received seq {s} never acked"
            );
        }
        for s in &sent_seqs {
            assert!(
                acks_received.contains(s),
                "{name}: sent seq {s} never acked by peer"
            );
        }
    }
}

/// A retransmission is the same payload and seq with `resend` incremented.
/// The race fixture has both a host burst re-sent after 52 ms and a client
/// Ready re-sent after 2.5 ms, so both directions are covered.
#[test]
fn retransmits_repeat_payload_with_resend_incremented() {
    let mut seen_resend = false;
    for name in ["lobby_race_host.txt", "lobby_race_client.txt"] {
        let packets: Vec<(bool, Packet)> = fixture(name)
            .iter()
            .filter_map(|(sent, raw)| reliable(raw).map(|p| (*sent, p)))
            .collect();
        for (sent, p) in packets.iter().filter(|(_, p)| p.resend > 0) {
            seen_resend = true;
            let original = packets
                .iter()
                .find(|(s, o)| s == sent && o.seq == p.seq && o.resend == 0)
                .unwrap_or_else(|| panic!("{name}: resend of seq {} without original", p.seq));
            assert_eq!(
                original.1.payload, p.payload,
                "{name}: resend changed payload"
            );
            assert_eq!(original.1.ordered_index, p.ordered_index);
            assert_eq!(original.1.chunk, p.chunk);
        }
    }
    assert!(seen_resend, "race fixtures should contain retransmissions");
}

/// A payload over `CHUNK_SIZE` arrives as consecutive packets sharing a chunk
/// id whose offsets tile `total_size` exactly. Reassembly depends on this.
#[test]
fn chunked_payload_tiles_total_size() {
    let chunks: Vec<Packet> = fixture("lobby_race_host.txt")
        .iter()
        .filter(|(sent, _)| *sent)
        .filter_map(|(_, raw)| reliable(raw))
        .filter(|p| p.chunk.is_some() && p.resend == 0)
        .collect();
    assert!(!chunks.is_empty(), "race start should chunk SpawnCar");

    let mut ids: Vec<u32> = chunks.iter().map(|p| p.chunk.unwrap().id).collect();
    ids.dedup();
    for id in ids {
        let pieces: Vec<&Packet> = chunks
            .iter()
            .filter(|p| p.chunk.unwrap().id == id)
            .collect();
        let total = pieces[0].chunk.unwrap().total_size as usize;
        assert_eq!(pieces.len(), pieces[0].chunk.unwrap().count as usize);
        let mut offset = 0usize;
        for p in &pieces {
            assert_eq!(
                p.chunk.unwrap().offset as usize,
                offset,
                "chunk {id} not contiguous"
            );
            assert!(p.payload.len() <= CHUNK_SIZE);
            offset += p.payload.len();
        }
        assert_eq!(offset, total, "chunk {id} pieces do not sum to total_size");
        assert_eq!(event_kind(&pieces[0].payload), Ok(Event::SpawnCar));
    }
}

/// Reassemble every chunked reliable payload the given side sent, in order.
fn reassembled(name: &str, sent: bool) -> Vec<Vec<u8>> {
    let pieces: Vec<Packet> = fixture(name)
        .iter()
        .filter(|(s, _)| *s == sent)
        .filter_map(|(_, raw)| reliable(raw))
        .filter(|p| p.chunk.is_some() && p.resend == 0)
        .collect();
    let mut ids: Vec<u32> = pieces.iter().map(|p| p.chunk.unwrap().id).collect();
    ids.dedup();
    ids.iter()
        .map(|id| {
            let mut parts: Vec<&Packet> = pieces
                .iter()
                .filter(|p| p.chunk.unwrap().id == *id)
                .collect();
            parts.sort_by_key(|p| p.chunk.unwrap().offset);
            parts
                .iter()
                .flat_map(|p| p.payload.iter().copied())
                .collect()
        })
        .collect()
}

/// The race start is fully synthesisable: StartRace is a fixed shape around
/// the map name, and each SpawnCar is that player's GarageState body wrapped
/// with its id, an entity id and a grid pose. Both are checked byte-exact
/// against the real host's chunks.
#[test]
fn start_race_and_spawn_cars_match_capture() {
    let host = fixture("lobby_race_host.txt");
    let payloads = |sent: bool, e: Event| {
        host.iter()
            .filter(move |(s, _)| *s == sent)
            .filter_map(|(_, raw)| reliable(raw))
            .filter(move |p| event_kind(&p.payload) == Ok(e))
    };
    let start = payloads(true, Event::StartRace).next().expect("StartRace");
    assert_eq!(
        start.payload,
        encode_start_race("forest_long", 1, &RaceSettings::default())
    );
    assert_eq!(
        start.ordered_index,
        Some(1),
        "follows the host's ReadyBroadcast"
    );

    let spawns = reassembled("lobby_race_host.txt", true);
    assert_eq!(spawns.len(), 2);

    // The host's garage arrives as a commit; a GarageState payload is the same
    // body behind a different discriminant and no owner.
    let commit = payloads(true, Event::GarageStateCommit)
        .next()
        .unwrap()
        .payload;
    let mut host_garage = (Event::GarageState as u32).to_le_bytes().to_vec();
    host_garage.extend_from_slice(&commit[12..]);
    let client_garage = payloads(false, Event::GarageState).next().unwrap().payload;

    let rot = [
        0.0,
        f32::from_bits(0xbf7ffc91),
        0.0,
        f32::from_bits(0x3c27aa20),
    ];
    let pose = |x: u32| -> Pose {
        [
            rot[0],
            rot[1],
            rot[2],
            rot[3],
            f32::from_bits(x),
            0.754,
            248.0,
        ]
    };
    assert_eq!(
        spawns[0],
        encode_spawn_car(PlayerId::HOST, 0x540, &host_garage, &pose(0x428e0fa0))
    );
    let client = PlayerId {
        client_id: 1,
        player_index: 1,
    };
    assert_eq!(
        spawns[1],
        encode_spawn_car(client, 0xe4f, &client_garage, &pose(0x428544d1))
    );
}

/// The grid "Waiting for other players" gate opens only when the host itself
/// confirms: after the client's grid Ready the real host answered with its own
/// ReadyBroadcast on the ordered stream and a bodiless RaceGo on the unordered
/// one (bccap5, host at "Press any button" until it pressed).
#[test]
fn host_grid_confirm_sends_ready_then_race_go() {
    let host = fixture("grid_confirm_host.txt");
    let client_confirm = host
        .iter()
        .filter(|(sent, _)| !sent)
        .filter_map(|(_, raw)| reliable(raw))
        .next()
        .expect("client grid Ready");
    assert_eq!(decode_ready(&client_confirm.payload), Ok(true));

    let sent: Vec<_> = host
        .iter()
        .filter(|(sent, _)| *sent)
        .filter_map(|(_, raw)| reliable(raw))
        .collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].payload,
        encode_ready_broadcast(PlayerId::HOST, true)
    );
    assert!(sent[0].ordered_index.is_some());
    assert_eq!(sent[1].payload, encode_race_go());
    assert_eq!(sent[1].ordered_index, None);
}

/// Race end as a real host ran it (`finish_host.txt`, forest_long cut to its
/// finish gate): the client reports its crossing, the host later reports its
/// own to the lobby, then sends RaceEnd and re-commits its garage. The
/// client's crossing was not echoed back to it (no other clients to tell).
#[test]
fn finish_crossings_and_race_end_match_capture() {
    let host = fixture("finish_host.txt");
    let client_finish = host
        .iter()
        .filter(|(sent, _)| !sent)
        .filter_map(|(_, raw)| reliable(raw))
        .find(|p| event_kind(&p.payload) == Ok(Event::CrossedFinish))
        .expect("client CrossedFinish");
    let finish = decode_crossed_finish(&client_finish.payload).unwrap();
    assert_eq!((finish.entity, finish.generation), (0xe4f, 1));
    assert!((finish.time - 9.224).abs() < 1e-3, "{}", finish.time);
    // A relay of that crossing to a third player is the same triple behind
    // the owner's id; pin the shape the server will produce.
    let relayed = encode_car_crossed_finish(
        PlayerId {
            client_id: 2,
            player_index: 1,
        },
        &finish,
    );
    assert_eq!(&relayed[12..], &client_finish.payload[4..]);

    let sent: Vec<Packet> = host
        .iter()
        .filter(|(sent, _)| *sent)
        .filter_map(|(_, raw)| reliable(raw))
        .filter(|p| p.resend == 0)
        .collect();
    let kinds: Vec<Event> = sent
        .iter()
        .map(|p| event_kind(&p.payload).unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            Event::CarCrossedFinish,
            Event::RaceEnd,
            Event::GarageStateCommit
        ]
    );
    let (id, host_finish) = decode_car_crossed_finish(&sent[0].payload).unwrap();
    assert_eq!(id, PlayerId::HOST);
    assert_eq!(host_finish.entity, 0x540);
    assert!(
        (host_finish.time - 41.271).abs() < 1e-3,
        "{}",
        host_finish.time
    );
    assert_eq!(
        sent[0].payload,
        encode_car_crossed_finish(PlayerId::HOST, &host_finish)
    );
    assert!(sent[0].ordered_index.is_none());
    assert_eq!(sent[1].payload, encode_race_end());
    assert!(sent[1].ordered_index.is_some());
    assert!(sent[2].ordered_index.is_some());
}

/// During the race the client reports its car under the entity id its
/// SpawnCar carried, and the host's broadcasts wrap the same state shape with
/// the owner in front.
#[test]
fn car_state_carries_the_spawned_entity() {
    let host = fixture("lobby_race_host.txt");
    let mut states = 0;
    let mut broadcasts = 0;
    for (sent, raw) in &host {
        let Frame::Unreliable(payload) = parse(raw).unwrap() else {
            continue;
        };
        match event_kind(&payload) {
            Ok(Event::CarState) => {
                assert!(!sent);
                let cs = CarState::decode(&payload).unwrap();
                assert_eq!((cs.entity, cs.generation, cs.state.len()), (0xe4f, 1, 94));
                states += 1;
            }
            Ok(Event::CarStateBroadcast) => {
                assert!(sent);
                let (id, cs) = CarState::decode_broadcast(&payload).unwrap();
                assert_eq!(
                    (id, cs.entity, cs.generation, cs.state.len()),
                    (PlayerId::HOST, 0x540, 1, 94)
                );
                assert_eq!(cs.encode_broadcast(id), payload);
                broadcasts += 1;
            }
            _ => {}
        }
    }
    assert!(
        states > 1000 && broadcasts > 1000,
        "{states} states, {broadcasts} broadcasts"
    );
}

/// Unreliable frames carry bare events. In the lobby that is the 1 Hz
/// ping/pong pair; each Pong echoes the exact clock of the peer's latest Ping,
/// tracked per direction because the fixture interleaves both streams.
#[test]
fn pongs_echo_the_peers_ping() {
    let mut last_ping_from_peer: Option<f32> = None;
    let mut last_ping_from_us: Option<f32> = None;
    let mut echoes = 0;
    for (sent, raw) in fixture("host_fd87.txt") {
        let Frame::Unreliable(payload) = parse(&raw).unwrap() else {
            continue;
        };
        match event_kind(&payload) {
            Ok(Event::Ping) if sent => last_ping_from_us = Some(clock_of(&payload).unwrap()),
            Ok(Event::Ping) => last_ping_from_peer = Some(clock_of(&payload).unwrap()),
            Ok(Event::Pong) => {
                let expected = if sent {
                    last_ping_from_peer
                } else {
                    last_ping_from_us
                };
                assert_eq!(
                    Some(clock_of(&payload).unwrap()),
                    expected,
                    "pong did not echo"
                );
                echoes += 1;
            }
            _ => {}
        }
    }
    assert!(echoes > 10, "expected a steady pong stream, saw {echoes}");
}

/// During a race the Unreliable channel carries car state at ~20 Hz: the
/// client sends `CarState`, the host sends `CarStateBroadcast`. A server that
/// dropped Unreliable frames as noise would freeze every other car.
#[test]
fn race_streams_car_state_unreliably() {
    let mut sent_state = 0;
    let mut got_broadcast = 0;
    for (sent, raw) in fixture("lobby_race_client.txt") {
        let Frame::Unreliable(payload) = parse(&raw).unwrap() else {
            continue;
        };
        match event_kind(&payload) {
            Ok(Event::CarState) if sent => sent_state += 1,
            Ok(Event::CarStateBroadcast) if !sent => got_broadcast += 1,
            _ => {}
        }
    }
    assert!(sent_state > 100, "client sent {sent_state} CarState frames");
    assert!(
        got_broadcast > 100,
        "client got {got_broadcast} CarStateBroadcast frames"
    );
}

/// The joining client's identity message must decode with the player name the
/// game actually sent, and re-encode to the original bytes.
#[test]
fn client_info_round_trips() {
    let payload = fixture("host_fd87.txt")
        .iter()
        .filter_map(|(_, raw)| reliable(raw))
        .map(|p| p.payload)
        .find(|p| event_kind(p) == Ok(Event::ClientInfo))
        .expect("fixture contains ClientInfo");
    let info = ClientInfo::decode(&payload).unwrap();
    assert_eq!(info.name, "mja00");
    assert_eq!(info.encode(), payload);
}

/// ServerInfo is the largest thing a headless server has to synthesise, so
/// its layout has to round-trip against the real capture and decode to the
/// lobby a real host advertised.
#[test]
fn server_info_round_trips() {
    let payload = fixture("host_fd87.txt")
        .iter()
        .filter_map(|(_, raw)| reliable(raw))
        .map(|p| p.payload)
        .find(|p| event_kind(p) == Ok(Event::ServerInfo))
        .expect("fixture contains ServerInfo");
    let info = ServerInfo::decode(&payload).unwrap();
    assert_eq!(info.players.len(), 1);
    assert_eq!(info.players[0].name, "mja00");
    assert_eq!(info.players[0].id, PlayerId::HOST);
    assert_eq!(
        info.applicant,
        PlayerId {
            client_id: 1,
            player_index: 1
        }
    );
    assert_eq!(info.host, PlayerId::HOST);
    assert_eq!(info.map, "forest_long");
    assert!(info.enabled_mods.is_empty());
    assert_eq!(info.encode(), payload);
}

/// A second joiner is where a single-entry list can hide a wrong layout: the
/// real host lists the first client, then itself, and assigns the applicant
/// id 2. Existing members learn of the newcomer through `PlayerJoined` and
/// then a `GarageStateCommit` that is the newcomer's `GarageState` re-tagged.
#[test]
fn second_join_lists_clients_before_host_and_announces_the_newcomer() {
    let host = fixture("second_join_host.txt");
    let payloads: Vec<(bool, Vec<u8>)> = host
        .iter()
        .filter_map(|(sent, raw)| reliable(raw).map(|p| (*sent, p.payload)))
        .collect();
    let find = |sent: bool, e: Event| {
        payloads
            .iter()
            .filter(move |(s, p)| *s == sent && event_kind(p) == Ok(e))
            .map(|(_, p)| p.clone())
    };

    let info = find(true, Event::ServerInfo)
        .find(|p| ServerInfo::decode(p).unwrap().players.len() == 2)
        .expect("second joiner's ServerInfo");
    let info = ServerInfo::decode(&info).unwrap();
    assert_eq!(
        info.players[0].id,
        PlayerId {
            client_id: 1,
            player_index: 1
        }
    );
    assert_eq!(info.players[1].id, PlayerId::HOST);
    assert_eq!(
        info.applicant,
        PlayerId {
            client_id: 2,
            player_index: 1
        }
    );
    assert_eq!(info.host, PlayerId::HOST);

    let joined_bytes = find(true, Event::PlayerJoined)
        .next()
        .expect("PlayerJoined");
    let (id, joined) = ClientInfo::decode_joined(&joined_bytes).unwrap();
    assert_eq!(
        id,
        PlayerId {
            client_id: 2,
            player_index: 1
        }
    );
    assert_eq!(joined.name, "second");
    assert_eq!(joined.encode_joined(id), joined_bytes);

    let garages: Vec<Vec<u8>> = find(false, Event::GarageState).collect();
    let commits: Vec<Vec<u8>> = find(true, Event::GarageStateCommit).collect();
    let second_garage = garages.last().expect("second client's GarageState");
    let expected = encode_garage_commit(id, second_garage);
    assert!(
        commits.contains(&expected),
        "commit for client 2 is its garage re-tagged"
    );
}

/// Leaving is client-initiated: a `Disconnect` from the leaver, then the host
/// tells everyone (the leaver included) which player slot went away.
#[test]
fn disconnect_is_answered_with_player_left() {
    let host = fixture("leave_host.txt");
    let disconnect = host
        .iter()
        .filter(|(sent, _)| !sent)
        .filter_map(|(_, raw)| reliable(raw))
        .find(|p| event_kind(&p.payload) == Ok(Event::Disconnect))
        .expect("client sent Disconnect");
    assert_eq!(disconnect.payload, [23, 0, 0, 0, 0, 0, 0, 0]);

    let left: Vec<Packet> = host
        .iter()
        .filter(|(sent, _)| *sent)
        .filter_map(|(_, raw)| reliable(raw))
        .filter(|p| event_kind(&p.payload) == Ok(Event::PlayerLeft))
        .collect();
    let id = PlayerId {
        client_id: 2,
        player_index: 1,
    };
    assert!(left
        .iter()
        .all(|p| decode_player_left(&p.payload) == Ok(id)));
    assert_eq!(left[0].payload, encode_player_left(id));
    assert!(left[0].ordered_index.is_none());
    let distinct_seqs: std::collections::HashSet<u32> = left.iter().map(|p| p.seq).collect();
    assert_eq!(
        distinct_seqs.len(),
        2,
        "one PlayerLeft stream per remaining peer and the leaver"
    );
}

/// Ready is the first lobby interaction after the handshake. The client's
/// toggle and the host's broadcast must both decode to the bytes captured
/// when both players ticked the box.
#[test]
fn ready_toggle_and_broadcast_match_capture() {
    let host = fixture("lobby_race_host.txt");
    let ready = host
        .iter()
        .filter(|(sent, _)| !sent)
        .filter_map(|(_, raw)| reliable(raw))
        .find(|p| event_kind(&p.payload) == Ok(Event::Ready))
        .expect("client toggled Ready");
    assert_eq!(decode_ready(&ready.payload), Ok(true));

    let broadcast = host
        .iter()
        .filter(|(sent, _)| *sent)
        .filter_map(|(_, raw)| reliable(raw))
        .find(|p| event_kind(&p.payload) == Ok(Event::ReadyBroadcast))
        .expect("host broadcast its own Ready");
    assert_eq!(
        broadcast.payload,
        encode_ready_broadcast(PlayerId::HOST, true)
    );
    assert_eq!(
        broadcast.ordered_index,
        Some(0),
        "first ordered event from the host"
    );
}

/// A host that picked `autumn_02 | Reverse` with 3 laps, night and rain
/// announced the track twice in the lobby and then started the race; each
/// of those payloads must be reproducible from the chosen settings.
#[test]
fn map_changes_and_race_settings_match_capture() {
    let host = fixture("race_settings_host.txt");
    let mut sent = host
        .iter()
        .filter(|(s, _)| *s)
        .filter_map(|(_, raw)| reliable(raw))
        .filter(|p| {
            matches!(
                event_kind(&p.payload),
                Ok(Event::LobbyChangeMap | Event::StartRace)
            )
        });
    let first = sent.next().expect("first LobbyChangeMap");
    assert_eq!(first.payload, encode_lobby_change_map("autumn_01", 1));
    assert_eq!(first.ordered_index, Some(0), "map changes are ordered");
    let second = sent.next().expect("second LobbyChangeMap");
    assert_eq!(second.payload, encode_lobby_change_map("autumn_02", 2));
    let start = sent.next().expect("StartRace");
    let settings = RaceSettings {
        laps: 3,
        night: true,
        rain: true,
    };
    assert_eq!(
        start.payload,
        encode_start_race("autumn_02", 2, &settings),
        "variant travels in the last word, after laps/night/rain"
    );
    assert!(sent.next().is_none());
}

/// A garage visit is the one flow where a client's request is answered with
/// a chunked payload. The host's answer must be rebuilt byte for byte from
/// the garage it committed at join time, and the three broadcasts that tell
/// the other client about the visit must match.
#[test]
fn garage_visit_matches_capture() {
    let host = fixture("garage_visit_host.txt");
    let sent: Vec<Packet> = host
        .iter()
        .filter(|(sent, _)| *sent)
        .filter_map(|(_, raw)| reliable(raw))
        .collect();
    let received: Vec<Packet> = host
        .iter()
        .filter(|(sent, _)| !sent)
        .filter_map(|(_, raw)| reliable(raw))
        .collect();
    let find = |packets: &[Packet], event: Event| -> Packet {
        packets
            .iter()
            .find(|p| event_kind(&p.payload) == Ok(event))
            .unwrap_or_else(|| panic!("no {event:?}"))
            .clone()
    };

    let request = find(&received, Event::RequestVisitGarage);
    assert_eq!(decode_visit_request(&request.payload), Ok(PlayerId::HOST));

    // The host's garage arrived as a GarageStateCommit; turn it back into
    // the GarageState shape the server keeps.
    let commit = find(&sent, Event::GarageStateCommit);
    let mut garage_state = (Event::GarageState as u32).to_le_bytes().to_vec();
    garage_state.extend_from_slice(&commit.payload[12..]);
    assert_eq!(
        encode_garage_commit(PlayerId::HOST, &garage_state),
        commit.payload
    );

    let first = find(&sent, Event::VisitGarageResponse);
    let chunk = first.chunk.expect("response is chunked");
    let mut response = vec![0u8; chunk.total_size as usize];
    let mut pieces = 0;
    for p in sent
        .iter()
        .filter(|p| p.chunk.map(|c| c.id) == Some(chunk.id))
    {
        let at = p.chunk.unwrap().offset as usize;
        response[at..at + p.payload.len()].copy_from_slice(&p.payload);
        pieces += 1;
    }
    assert_eq!(pieces, chunk.count);
    assert_eq!(
        encode_visit_garage_response(PlayerId::HOST, &garage_state),
        response
    );

    let visitor = PlayerId {
        client_id: 1,
        player_index: 1,
    };
    assert_eq!(
        find(&sent, Event::GarageVisitBroadcast).payload,
        encode_garage_visit_broadcast(visitor, PlayerId::HOST)
    );
    assert_eq!(
        find(&sent, Event::UpdateLocationBroadcast).payload,
        encode_location_in_garage(visitor, PlayerId::HOST)
    );

    // Sender-tagged rewraps: avatar state (Unreliable in, reliable out),
    // location and stop-visit.
    let avatar = host
        .iter()
        .filter(|(sent, _)| !sent)
        .find_map(|(_, raw)| match parse(raw) {
            Ok(Frame::Unreliable(p)) if event_kind(&p) == Ok(Event::UpdateAvatarState) => Some(p),
            _ => None,
        })
        .expect("client sent avatar state");
    let relayed = find(&sent, Event::UpdateAvatarStateBroadcast).payload;
    assert_eq!(
        relayed[..12],
        encode_with_sender(Event::UpdateAvatarStateBroadcast, visitor, &avatar)[..12]
    );
    assert_eq!(relayed.len(), avatar.len() + 8);
    let location = find(&received, Event::UpdateLocation);
    let stop = find(&received, Event::StopGarageVisit);
    // The last location broadcast is the one echoing the client's own
    // UpdateLocation; the earlier one was host-generated on entering.
    let back = sent
        .iter()
        .rfind(|p| event_kind(&p.payload) == Ok(Event::UpdateLocationBroadcast))
        .unwrap();
    assert_eq!(
        back.payload,
        encode_with_sender(Event::UpdateLocationBroadcast, visitor, &location.payload)
    );
    assert_eq!(
        find(&sent, Event::StopGarageVisitBroadcast).payload,
        encode_with_sender(Event::StopGarageVisitBroadcast, visitor, &stop.payload)
    );
}

/// Greeting ids drive the first handshake step; echo it wrong and the client
/// never proceeds to identify itself.
#[test]
fn greeting_is_echoed_verbatim() {
    let greetings: Vec<u32> = fixture("host_fd87.txt")
        .iter()
        .filter_map(|(_, raw)| match parse(raw) {
            Ok(Frame::Greeting(id)) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(greetings, vec![GREETING_ID, GREETING_ID]);
}

/// Malformed input must fail loudly rather than silently producing a frame
/// that would be applied to game state.
#[test]
fn malformed_input_is_rejected() {
    assert!(parse(&[]).is_err());
    assert!(parse(&[9, 0, 0, 0, 0]).is_err(), "unknown kind");
    assert!(parse(&[2, 1]).is_err(), "truncated ack");
    assert!(
        parse(&[0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]).is_err(),
        "absurd length"
    );
    assert!(parse(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7]).is_err());
}

/// The client -> broadcast routing table, read from the binary's
/// `NetworkEvent::broadcast_equivalent` switch. The broadcast re-tags the body
/// with the sender's `PlayerId` and nothing else; a host relays no other
/// client event.
#[test]
fn broadcast_twins_insert_the_sender() {
    assert_eq!(broadcast_twin(6), Some(7)); // Ready
    assert_eq!(broadcast_twin(11), Some(12)); // SyncCarState
    assert_eq!(broadcast_twin(16), Some(17)); // CarDeriative
    assert_eq!(broadcast_twin(29), Some(30)); // UpdateAvatarState
    assert_eq!(broadcast_twin(31), Some(32)); // UpdateLocation
    assert_eq!(broadcast_twin(39), Some(40)); // CarEvent
    assert_eq!(broadcast_twin(41), Some(42)); // StopGarageVisit
    assert_eq!(broadcast_twin(43), Some(46)); // PushCartStarted
    assert_eq!(broadcast_twin(44), Some(47)); // PushCartMoved
    assert_eq!(broadcast_twin(45), Some(48)); // PushCartEnd
    // Variant 0 (a String) maps to itself *without* the PlayerId, so a host
    // relays it unchanged rather than re-tagging it.
    assert_eq!(broadcast_twin(0), None);
    assert_eq!(
        broadcast_twin(4),
        None,
        "CrossedFinish has no broadcast twin"
    );

    let sender = PlayerId {
        client_id: 2,
        player_index: 1,
    };
    // PushCartStarted(43) carries one 8-byte field after the discriminant.
    let mut payload = 43u32.to_le_bytes().to_vec();
    payload.extend_from_slice(&[1u8; 8]);
    let out = encode_with_sender_disc(46, sender, &payload);
    assert_eq!(&out[..4], &46u32.to_le_bytes());
    assert_eq!(&out[4..8], &sender.client_id.to_le_bytes());
    assert_eq!(&out[12..20], &[1u8; 8]);
    assert_eq!(out.len(), 4 + 8 + 8);
}

/// A host relays exactly one event verbatim (variant 0, a `String`); every
/// other non-twin event it consumes, so the server must not forward it.
#[test]
fn only_the_string_variant_is_relayed_verbatim() {
    assert!(broadcast_verbatim(0));
    for disc in [4, 6, 11, 19, 36, 37, 39] {
        assert!(!broadcast_verbatim(disc), "{disc} is not relayed verbatim");
    }
}

/// `Disconnect` carries a `DisconnectReason` `u32` after the discriminant; the
/// capture has only `0` (a clean quit).
#[test]
fn disconnect_carries_a_reason() {
    let mut found = 0;
    for (_sent, raw) in fixture("leave_host.txt") {
        let Some(p) = reliable(&raw) else { continue };
        if event_kind(&p.payload) == Ok(Event::Disconnect) {
            assert_eq!(decode_disconnect(&p.payload).unwrap(), 0);
            found += 1;
        }
    }
    assert!(found > 0, "no Disconnect frame in leave_host.txt");
}

