//! Steam lobby + P2P bridge for `beatermp`.
//!
//! BeaterCore's in-game Server list is a Steam Matchmaking lobby list, and a
//! player who picks an entry is sent over Steam's P2P sockets to the lobby
//! owner. To appear there, a `beatermp` host therefore needs both a lobby and a
//! P2P endpoint that speaks the game's protocol. [`translate`] holds the seam
//! between Steam's raw `NetworkEvent` messages and the UDP `Frame` envelope the
//! server speaks; the `beatermp-steam` binary (`steam` feature) drives Steam
//! and relays to a local `beatermp`.
//!
//! See `docs/notes/steam-browser.md` for the reverse-engineered contract.

pub mod translate;
