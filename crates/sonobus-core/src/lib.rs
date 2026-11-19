//! SonoBus application-level data formats, wire-compatible with SonoBus 1.7.x.
//!
//! - [`valuetree`]: JUCE `ValueTree::writeToStream` / `readFromStream` binary format.
//! - [`layout`]: the `ChannelLayouts` tree peers exchange in `/sb/clayinfo` and the AOO user format.
//! - [`sbmsg`]: the `/sb/...` peer-to-peer app messages (ping, peer info, chat, latency match, ...).
//! - [`juce_xml`]: JUCE XML (ValueTree <-> XML), JUCE base64 and `copyXmlToBinary` framing.
//! - [`state`]: the typed persistent processor state (`SonoBusAoO` tree).
//! - [`files`]: `SonoBus.settings`, `.sonobus` setups, `PluginDefault.xml`, `GlobalState.xml`,
//!   `soundboards.xml`.

pub mod files;
pub mod juce_xml;
pub mod layout;
pub mod sbmsg;
pub mod state;
pub mod valuetree;

pub use layout::{ChannelLayout, ChannelLayouts};
pub use sbmsg::SbMessage;
pub use valuetree::{ValueTree, Var};
