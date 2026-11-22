//! SonoBus protocol v2 over iroh (QUIC).
//!
//! [`wire`], [`auth`], [`stream`] and [`adapt`] are sans-IO (no tokio); [`rt`] owns the network
//! on one dedicated `sb-net` thread running a current-thread tokio runtime and exposes a
//! synchronous API.

pub mod adapt;
pub mod auth;
pub mod lan;
pub mod rdv;
pub mod rt;
pub mod stream;
pub mod wire;

pub use auth::RdvAddr;
/// The datagram buffer type of [`Sink`] and [`PeerTx::send`].
pub use bytes::Bytes;
pub use rdv::RendezvousServer;
pub use rt::{
    CloseReason, Config, ConnId, ControlError, Event, Events, Join, Net, PathInfo, PeerAddr,
    PeerInfo, PeerTx, RelayChoice, SendError, Sink, generate_secret_key,
};
pub use wire::SessionMode;
