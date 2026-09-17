//! Frame <-> raw-`NetworkEvent` translation between beatermp's UDP protocol
//! and the game's Steam transport.
//!
//! BeaterCore's Steam path does *not* use the kind-0/1/2/3 `Frame` envelope the
//! UDP path uses (see `docs/notes/steam-browser.md`): it sends bare bincode
//! `NetworkEvent` payloads and leans on Steam's own reliable/unreliable flags.
//! This module is the seam. It wraps client events in `Frame`s for beatermp,
//! and turns beatermp's frames back into single Steam messages, terminating
//! beatermp's reliability (acks, retransmits, chunking) on the way through.

use std::collections::{HashMap, HashSet};

use beatermp_codec::{encode, event_kind, parse, Event, Frame, Packet};

/// One message the bridge must hand to the Steam peer.
#[derive(Debug, PartialEq, Eq)]
pub enum ToSteam {
    Reliable(Vec<u8>),
    Unreliable(Vec<u8>),
}

/// A chunked beatermp payload being reassembled before it goes to Steam.
struct Reassembly {
    buf: Vec<u8>,
    have: usize,
}

impl Reassembly {
    fn new(total: usize) -> Self {
        Reassembly {
            buf: vec![0; total],
            have: 0,
        }
    }
}

/// Per-Steam-peer translation state.
pub struct Peer {
    next_seq: u32,
    /// beatermp sequence numbers already forwarded, so a retransmit is acked
    /// but not delivered to Steam twice.
    forwarded: HashSet<u32>,
    /// Chunked beatermp payloads still being reassembled, by chunk id.
    inbound: HashMap<u32, Reassembly>,
}

impl Default for Peer {
    fn default() -> Self {
        Peer {
            next_seq: 1,
            forwarded: HashSet::new(),
            inbound: HashMap::new(),
        }
    }
}

/// The client events that travel unreliably (Steam flag 0, UDP kind 1); every
/// other event is reliable. Matches the capture-pinned send modes in
/// `docs/notes/parity.md`.
fn client_event_is_unreliable(payload: &[u8]) -> bool {
    matches!(
        event_kind(payload),
        Ok(Event::CarState) | Ok(Event::Ping) | Ok(Event::Pong) | Ok(Event::UpdateAvatarState)
    )
}

impl Peer {
    pub fn new() -> Self {
        Peer::default()
    }

    /// A Steam message from the client -> a datagram for the beatermp socket.
    ///
    /// Steam's own flags are not used to decide this: the event discriminant
    /// is exact, whereas `NetworkingMessage` does not expose how it was sent.
    pub fn from_steam(&mut self, payload: &[u8], now_secs: u64) -> Vec<u8> {
        if client_event_is_unreliable(payload) {
            encode(&Frame::Unreliable(payload.to_vec()))
        } else {
            let packet = Packet {
                payload: payload.to_vec(),
                seq: self.next_seq,
                resend: 0,
                sent_at: now_secs,
                ordered_index: None,
                chunk: None,
            };
            self.next_seq += 1;
            encode(&Frame::Reliable(packet))
        }
    }

    /// A datagram from beatermp -> the bytes to send back to beatermp (usually
    /// an ack) plus the messages for the Steam peer.
    pub fn from_beatermp(&mut self, datagram: &[u8]) -> (Option<Vec<u8>>, Vec<ToSteam>) {
        let Ok(frame) = parse(datagram) else {
            return (None, Vec::new());
        };
        match frame {
            // The greeting echo is a UDP-only connectivity check, and acks are
            // for beatermp's reliability layer, which ends here.
            Frame::Greeting(_) | Frame::Ack(_) => (None, Vec::new()),
            Frame::Unreliable(payload) => (None, vec![ToSteam::Unreliable(payload)]),
            Frame::Reliable(packet) => {
                let ack = encode(&Frame::Ack(packet.seq));
                (Some(ack), self.deliver(packet))
            }
        }
    }

    fn deliver(&mut self, packet: Packet) -> Vec<ToSteam> {
        if !self.forwarded.insert(packet.seq) {
            return Vec::new();
        }
        match packet.chunk {
            None => vec![ToSteam::Reliable(packet.payload)],
            Some(chunk) => {
                let total = chunk.total_size as usize;
                let offset = chunk.offset as usize;
                let end = offset + packet.payload.len();
                let entry = self
                    .inbound
                    .entry(chunk.id)
                    .or_insert_with(|| Reassembly::new(total));
                if end > entry.buf.len() {
                    return Vec::new();
                }
                entry.buf[offset..end].copy_from_slice(&packet.payload);
                entry.have += packet.payload.len();
                if entry.have < entry.buf.len() {
                    return Vec::new();
                }
                let done = self.inbound.remove(&chunk.id).expect("present");
                vec![ToSteam::Reliable(done.buf)]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beatermp_codec::{encode_clock, Chunk, CHUNK_SIZE};

    fn reliable(seq: u32, payload: Vec<u8>, chunk: Option<Chunk>) -> Vec<u8> {
        encode(&Frame::Reliable(Packet {
            payload,
            seq,
            resend: 0,
            sent_at: 0,
            ordered_index: None,
            chunk,
        }))
    }

    #[test]
    fn client_ping_becomes_an_unreliable_frame() {
        let mut p = Peer::new();
        let ping = encode_clock(Event::Ping, 1.0);
        let out = p.from_steam(&ping, 0);
        assert_eq!(out[0], 1); // kind 1
        assert_eq!(&out[1..], &ping[..]);
    }

    #[test]
    fn client_action_becomes_a_numbered_reliable_frame() {
        let mut p = Peer::new();
        let ready = {
            let mut v = (Event::Ready as u32).to_le_bytes().to_vec();
            v.push(1);
            v
        };
        let first = p.from_steam(&ready, 0);
        let second = p.from_steam(&ready, 0);
        let seq_of = |bytes: &[u8]| match parse(bytes).unwrap() {
            Frame::Reliable(p) => p.seq,
            other => panic!("expected a reliable frame, got {other:?}"),
        };
        assert_eq!(first[0], 0); // kind 0
                                 // seq counts from 1 and increments per frame.
        assert_eq!(seq_of(&first), 1);
        assert_eq!(seq_of(&second), 2);
    }

    #[test]
    fn acks_are_swallowed() {
        let mut p = Peer::new();
        assert_eq!(p.from_beatermp(&encode(&Frame::Ack(7))), (None, vec![]));
    }

    #[test]
    fn reliable_event_is_acked_and_forwarded() {
        let mut p = Peer::new();
        let event = vec![2u8, 0, 0, 0]; // RaceGo, bodyless
        let (ack, msgs) = p.from_beatermp(&reliable(5, event.clone(), None));
        assert_eq!(ack, Some(encode(&Frame::Ack(5))));
        assert_eq!(msgs, vec![ToSteam::Reliable(event)]);
    }

    #[test]
    fn unreliable_event_is_forwarded_unflagged() {
        let mut p = Peer::new();
        let event = vec![21u8, 0, 0, 0, 0, 0, 0, 0]; // Ping
        let (ack, msgs) = p.from_beatermp(&encode(&Frame::Unreliable(event.clone())));
        assert_eq!(ack, None);
        assert_eq!(msgs, vec![ToSteam::Unreliable(event)]);
    }

    #[test]
    fn chunked_event_is_reassembled_into_one_message() {
        let mut p = Peer::new();
        let payload: Vec<u8> = (0..(CHUNK_SIZE + 22)).map(|i| i as u8).collect();
        let count = payload.len().div_ceil(CHUNK_SIZE) as u16;
        let first = payload[..CHUNK_SIZE].to_vec();
        let second = payload[CHUNK_SIZE..].to_vec();

        let (ack1, msgs1) = p.from_beatermp(&reliable(
            1,
            first,
            Some(Chunk {
                id: 9,
                offset: 0,
                total_size: payload.len() as u32,
                count,
            }),
        ));
        assert_eq!(ack1, Some(encode(&Frame::Ack(1))));
        assert!(msgs1.is_empty(), "incomplete chunk must not reach Steam");

        let (ack2, msgs2) = p.from_beatermp(&reliable(
            2,
            second,
            Some(Chunk {
                id: 9,
                offset: CHUNK_SIZE as u32,
                total_size: payload.len() as u32,
                count,
            }),
        ));
        assert_eq!(ack2, Some(encode(&Frame::Ack(2))));
        assert_eq!(msgs2, vec![ToSteam::Reliable(payload)]);
    }

    #[test]
    fn retransmit_is_acked_once_but_not_delivered_twice() {
        let mut p = Peer::new();
        let event = vec![6u8, 0, 0, 0, 1]; // Ready(true)
        let (_, first) = p.from_beatermp(&reliable(3, event.clone(), None));
        let (ack, second) = p.from_beatermp(&reliable(3, event, None));
        assert_eq!(first.len(), 1);
        assert_eq!(ack, Some(encode(&Frame::Ack(3))));
        assert!(second.is_empty());
    }
}
