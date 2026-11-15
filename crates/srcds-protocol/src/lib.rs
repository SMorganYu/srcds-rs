//! Source engine wire formats, implemented independently from public documentation
//! (Valve Developer Wiki "Server queries") and black-box observation of a reference
//! Garry's Mod dedicated server. See `docs/PROTOCOL.md`.
//!
//! Every decoder in this crate is bounded: it never allocates more than the size of
//! the input it was given, and returns [`ProtocolError`] instead of panicking on
//! malformed data.

pub mod a2s;
pub mod bitbuf;
pub mod bytes;
pub mod connectionless;
pub mod error;
pub mod handshake;
pub mod lzss;
pub mod netchan;
pub mod netmsg;
pub mod sendtable;
pub mod split;
pub mod stringtable;
pub mod usercmd;

pub use error::ProtocolError;

/// Maximum size of a single UDP datagram the engine sends or accepts
/// (`NET_MAX_PAYLOAD`-class limit; the conventional routable MTU used by Source is 1400).
pub const MAX_ROUTABLE_PAYLOAD: usize = 1400;

/// Upper bound on datagrams we are willing to read at all.
pub const MAX_DATAGRAM: usize = 65_507;
