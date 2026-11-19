//! Audio device layer: turns platform audio devices into the single fixed-block duplex callback
//! the engine runs on, the way JUCE's `processBlock` is driven.
//!
//! - [`Duplex`] (hardware-free): re-blocks any device buffer size into fixed engine blocks and
//!   pulls one aligned block per engine call from every input.
//! - [`ExternalInput`] / input ports: one lock-free ring per input at its own rate and clock,
//!   resampled to the engine rate with drift compensation against the engine clock. cpal inputs
//!   and application-pushed audio (e.g. Android playback capture via JNI) use the same path.
//! - [`Session`]: opens cpal devices around a `Duplex`; [`devices`] and [`DeviceWatcher`] feed
//!   the UI.
//!
//! The engine callback, `push` and `render` never allocate, lock or block.

mod device;
mod duplex;
mod port;

pub use cpal;
pub use cpal::{DeviceId, Error, ErrorKind, HostId};
pub use device::{
    DeviceChange, DeviceInfo, DeviceWatcher, InputConfig, InputSource, Session, SessionConfig,
    StreamEvent, StreamRole, devices, hosts,
};

pub use duplex::{AudioBlock, Duplex, MAX_CHANNELS, default_margin};
pub use port::{ExternalInput, InputMonitor, InputStats, MAX_DEVIATION};
