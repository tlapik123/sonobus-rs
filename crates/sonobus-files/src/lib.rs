//! Audio file recording and playback for the SonoBus engine (upstream 1.7.2
//! `SonobusPluginProcessor.cpp` `startRecordingToFile` / `loadURLIntoTransport`).
//!
//! Real-time split: the audio thread only touches [`RecordWriter::write`] and
//! [`PlayerOutput::read`], which copy through preallocated lock-free rings; encoding, decoding,
//! resampling and disk I/O run on one background thread per recorder/player.
//!
//! The crate does not decide what gets recorded. Upstream's recording kinds (options bitmask Mix 1,
//! Self 2, MixMinusSelf 4, IndividualUsers 8) are N independent recorders the engine starts:
//! only-mix records `<base>.<ext>`; anything else creates a folder `<base>` (made unique with
//! [`nonexistent_path`]) holding one [`track_file_name`] per kind.

mod naming;
mod play;
mod record;

pub use naming::{
    LocalTime, TrackKind, legal_file_name, nonexistent_path, session_base_name, track_file_name,
};
pub use play::{FileInfo, Player, PlayerOutput, open_player};
pub use record::{
    RING_FRAMES, RecordFormat, RecordSpec, RecordWriter, Recording, start_recording,
    start_recording_after_silence,
};
/// Test seams for other crates' tests (feature `test-hooks`; never in a release build).
#[cfg(feature = "test-hooks")]
pub mod testing {
    pub use crate::play::testing::set_slow_io;
    pub use crate::record::testing::fail_writes;
}
