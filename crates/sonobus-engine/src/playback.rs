//! Control side of file playback (signal-flow step 9) and the soundboard (step 10). Decoding
//! runs on `sonobus-files` player threads; the audio thread only reads their rings
//! ([`crate::mixer::Deck`]).
//!
//! File transport: upstream `loadURLIntoTransport` / `mTransportSource` (P.cpp:5250-5258,
//! 9595-9665). At the end of the file (not looping) it stops and returns to the start.
//!
//! Soundboard: `SoundboardChannelProcessor` + `SamplePlaybackManager` + the button handling of
//! `SoundboardView` (SoundboardChannelProcessor.cpp, SoundboardView.cpp:264-312, 578-645,
//! 1088-1135):
//! - playing a sample that already plays restarts it from the start;
//! - back-to-back stops every other sample except background ones; simultaneous and
//!   background play alongside;
//! - continue-from-last-position resumes where the sample was stopped;
//! - end behaviour loop loops, stop stops, next plays the following sample of the board;
//! - buttons: toggle starts/stops, one-shot (re)starts, hold plays while pressed.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sonobus_core::files::{
    ButtonBehaviour, EndPlaybackBehaviour, PlaybackBehaviour, ReplayBehaviour, SoundSample,
    Soundboards,
};
use sonobus_files::{Player, open_player};

use crate::Notification;
use crate::mixer::{Cmd, Deck, MAX_VOICES};

fn deck(path: &Path, rate: u32) -> io::Result<(Player, Box<Deck>, Arc<AtomicBool>)> {
    let (player, out) = open_player(path, rate)?;
    let playing = Arc::new(AtomicBool::new(false));
    let d = Box::new(Deck {
        out,
        playing: playing.clone(),
        sample_rate: player.info().sample_rate,
    });
    Ok((player, d, playing))
}

fn secs_to_frames(p: &Player, secs: f64) -> u64 {
    (secs.max(0.0) * f64::from(p.info().sample_rate)).round() as u64
}

fn frames_to_secs(p: &Player, frames: u64) -> f64 {
    frames as f64 / f64::from(p.info().sample_rate.max(1))
}

/// The loaded file, as [`crate::Engine::file_status`] reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct FileStatus {
    pub path: PathBuf,
    pub playing: bool,
    pub position_secs: f64,
    pub length_secs: Option<f64>,
    pub looping: bool,
    /// (start, length) in seconds; length 0 = the whole file.
    pub loop_range_secs: (f64, f64),
    pub channels: usize,
    pub sample_rate: u32,
}

pub(crate) struct FileTransport {
    path: PathBuf,
    player: Player,
    playing: Arc<AtomicBool>,
}

impl FileTransport {
    /// Open `path` decoding to `rate` (file I/O: call it without the engine lock); the deck
    /// goes to the audio thread in [`Cmd::File`].
    pub fn open(path: &Path, rate: u32) -> io::Result<(Self, Box<Deck>)> {
        let (player, d, playing) = deck(path, rate)?;
        let t = Self {
            path: path.to_owned(),
            player,
            playing,
        };
        Ok((t, d))
    }

    pub fn channels(&self) -> usize {
        self.player.info().channels
    }

    pub fn set_playing(&self, on: bool) {
        self.playing.store(on, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn seek(&self, secs: f64) {
        self.player.seek(secs_to_frames(&self.player, secs));
    }

    pub fn set_looping(&self, on: bool) {
        self.player.set_looping(on);
    }

    pub fn set_loop_range(&self, start_secs: f64, len_secs: f64) {
        let p = &self.player;
        p.set_loop_range(secs_to_frames(p, start_secs), secs_to_frames(p, len_secs));
    }

    /// End of file while playing: stop and rewind (`changeListenerCallback`, P.cpp:5250-5256).
    pub fn poll(&self, note: &mut dyn FnMut(Notification)) -> bool {
        if self.is_playing() && self.player.is_finished() {
            self.set_playing(false);
            self.player.seek(0);
            note(Notification::FileFinished);
            return true;
        }
        false
    }

    pub fn status(&self) -> FileStatus {
        let p = &self.player;
        let info = p.info();
        let (ls, ll) = p.loop_range();
        FileStatus {
            path: self.path.clone(),
            playing: self.is_playing(),
            position_secs: frames_to_secs(p, p.position()),
            length_secs: info.frames.map(|f| frames_to_secs(p, f)),
            looping: p.is_looping(),
            loop_range_secs: (frames_to_secs(p, ls), frames_to_secs(p, ll)),
            channels: info.channels,
            sample_rate: info.sample_rate,
        }
    }
}

/// (board, sample) index.
pub type SampleKey = (usize, usize);

struct Voice {
    key: SampleKey,
    /// What it was started from: a different sample at the key stops it.
    file: String,
    player: Player,
    playing: Arc<AtomicBool>,
}

/// One playing soundboard sample, for the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceStatus {
    pub board: usize,
    pub sample: usize,
    pub position_secs: f64,
    pub length_secs: Option<f64>,
}

/// A sample to start whose file has to be opened first ([`SoundboardPlayer::open`], off the
/// engine lock), then installed ([`SoundboardPlayer::install`]).
pub(crate) struct PendingSample {
    key: SampleKey,
    /// The sample's `file_url` when it was asked for: a changed sample is not installed.
    file: String,
    path: PathBuf,
}

/// An opened [`PendingSample`].
pub(crate) struct OpenedSample {
    key: SampleKey,
    file: String,
    player: Player,
    deck: Box<Deck>,
    playing: Arc<AtomicBool>,
}

#[derive(Default)]
pub(crate) struct SoundboardPlayer {
    pub boards: Soundboards,
    voices: Vec<Option<Voice>>,
    /// `SoundSample::lastPlaybackPosition` (not persisted upstream either).
    last_pos: HashMap<SampleKey, f64>,
}

fn sample_path(s: &SoundSample) -> Option<PathBuf> {
    if let Some(p) = s.file_path.as_deref().filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let u = s.file_url.strip_prefix("file://")?;
    Some(PathBuf::from(percent_decode(u)))
}

/// `%XX` escapes of a `file://` URL path.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl SoundboardPlayer {
    pub fn new() -> Self {
        Self {
            voices: (0..MAX_VOICES).map(|_| None).collect(),
            ..Self::default()
        }
    }

    fn sample(&self, key: SampleKey) -> Option<&SoundSample> {
        self.boards.boards.get(key.0)?.samples.get(key.1)
    }

    fn voice_index(&self, key: SampleKey) -> Option<usize> {
        self.voices
            .iter()
            .position(|v| v.as_ref().is_some_and(|v| v.key == key))
    }

    pub fn is_active(&self, key: SampleKey) -> bool {
        self.voice_index(key).is_some()
    }

    /// New boards: voices whose sample changed stop, the others take the new gain and loop.
    pub fn set_boards(&mut self, boards: Soundboards, out: &mut dyn FnMut(Cmd)) {
        self.boards = boards;
        for i in 0..self.voices.len() {
            let Some(v) = &self.voices[i] else { continue };
            match self.sample(v.key) {
                Some(s) if s.file_url == v.file => {
                    v.player.set_gain(s.gain);
                    v.player.set_looping(
                        s.end_playback_behaviour_checked() == Some(EndPlaybackBehaviour::Loop),
                    );
                }
                _ => self.remove(i, out),
            }
        }
        self.last_pos.retain(|k, _| {
            self.boards
                .boards
                .get(k.0)
                .is_some_and(|b| k.1 < b.samples.len())
        });
    }

    /// `loadSample` + `play` (SoundboardChannelProcessor.cpp:197-243), step 1: a playing
    /// sample restarts (`None`); otherwise its file still has to be opened.
    pub fn play(&mut self, key: SampleKey) -> io::Result<Option<PendingSample>> {
        let s = self
            .sample(key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such sample"))?;
        if let Some(i) = self.voice_index(key) {
            // already playing: from the start again
            let gain = s.gain;
            let v = self.voices[i].as_ref().expect("found");
            v.player.seek(0);
            v.player.set_gain(gain);
            v.playing.store(true, Ordering::Relaxed);
            return Ok(None);
        }
        let path = sample_path(s)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "sample has no file"))?;
        Ok(Some(PendingSample {
            key,
            file: s.file_url.clone(),
            path,
        }))
    }

    /// Step 2, file I/O without the engine lock.
    pub fn open(p: PendingSample, rate: u32) -> io::Result<OpenedSample> {
        let (player, deck, playing) = deck(&p.path, rate)?;
        Ok(OpenedSample {
            key: p.key,
            file: p.file,
            player,
            deck,
            playing,
        })
    }

    /// Step 3: start it. Its board entry changed meanwhile: dropped (the returned voice is
    /// dropped by the caller, ideally after releasing the lock); a press meanwhile started it:
    /// that one restarts.
    pub fn install(
        &mut self,
        o: OpenedSample,
        out: &mut dyn FnMut(Cmd),
    ) -> (io::Result<()>, Option<OpenedSample>) {
        let key = o.key;
        let Some(s) = self.sample(key).filter(|s| s.file_url == o.file).cloned() else {
            return (Ok(()), Some(o));
        };
        if self.voice_index(key).is_some() {
            return (self.play(key).map(|_| ()), Some(o));
        }
        let OpenedSample {
            player,
            deck: d,
            playing,
            ..
        } = o;
        if s.playback_behaviour_checked() == Some(PlaybackBehaviour::BackToBack) {
            for i in 0..self.voices.len() {
                let bg = self.voices[i].as_ref().is_some_and(|v| {
                    self.sample(v.key)
                        .and_then(SoundSample::playback_behaviour_checked)
                        == Some(PlaybackBehaviour::Background)
                });
                if self.voices[i].is_some() && !bg {
                    self.unload(i, out);
                }
            }
        }
        player.set_gain(s.gain);
        player.set_looping(s.end_playback_behaviour_checked() == Some(EndPlaybackBehaviour::Loop));
        if s.replay_behaviour_checked() == Some(ReplayBehaviour::ContinueFromLastPosition)
            && let Some(&pos) = self.last_pos.get(&key)
        {
            player.seek(secs_to_frames(&player, pos));
        }
        let Some(i) = self.voices.iter().position(Option::is_none) else {
            return (Err(io::Error::other("too many samples playing")), None);
        };
        playing.store(true, Ordering::Relaxed);
        out(Cmd::Voice(i, Some(d)));
        self.voices[i] = Some(Voice {
            key,
            file: s.file_url.clone(),
            player,
            playing,
        });
        (Ok(()), None)
    }

    /// `pause` = `unload`: stop, remember the position.
    pub fn stop(&mut self, key: SampleKey, out: &mut dyn FnMut(Cmd)) {
        if let Some(i) = self.voice_index(key) {
            self.unload(i, out);
        }
    }

    pub fn stop_all(&mut self, out: &mut dyn FnMut(Cmd)) {
        for i in 0..self.voices.len() {
            if self.voices[i].is_some() {
                self.unload(i, out);
            }
        }
    }

    fn unload(&mut self, i: usize, out: &mut dyn FnMut(Cmd)) {
        if let Some(v) = &self.voices[i] {
            v.playing.store(false, Ordering::Relaxed);
            let pos = frames_to_secs(&v.player, v.player.position());
            self.last_pos.insert(v.key, pos);
        }
        self.remove(i, out);
    }

    fn remove(&mut self, i: usize, out: &mut dyn FnMut(Cmd)) {
        if self.voices[i].take().is_some() {
            out(Cmd::Voice(i, None));
        }
    }

    /// A button press (or a programmatic trigger, `triggerSampleAtIndex`): toggle stops a
    /// playing sample, everything else (re)starts it (a new one through [`Self::open`] and
    /// [`Self::install`]).
    pub fn press(
        &mut self,
        key: SampleKey,
        out: &mut dyn FnMut(Cmd),
    ) -> io::Result<Option<PendingSample>> {
        let toggle = self
            .sample(key)
            .and_then(SoundSample::button_behaviour_checked)
            == Some(ButtonBehaviour::Toggle);
        if toggle && self.is_active(key) {
            self.stop(key, out);
            Ok(None)
        } else {
            self.play(key)
        }
    }

    /// A button release: a hold button stops its sample.
    pub fn release(&mut self, key: SampleKey, out: &mut dyn FnMut(Cmd)) {
        if self
            .sample(key)
            .and_then(SoundSample::button_behaviour_checked)
            == Some(ButtonBehaviour::Hold)
        {
            self.stop(key, out);
        }
    }

    /// Samples that reached their end: removed, rewound; "next" queues the following one in
    /// `next` (to open off the lock).
    pub fn poll(
        &mut self,
        out: &mut dyn FnMut(Cmd),
        note: &mut dyn FnMut(Notification),
        next: &mut Vec<PendingSample>,
    ) {
        for i in 0..self.voices.len() {
            let Some(v) = &self.voices[i] else { continue };
            if !(v.playing.load(Ordering::Relaxed) && v.player.is_finished()) {
                continue;
            }
            let key = v.key;
            self.last_pos.insert(key, 0.0);
            self.remove(i, out);
            note(Notification::SampleFinished {
                board: key.0,
                sample: key.1,
            });
            let following = (key.0, key.1 + 1);
            if self
                .sample(key)
                .and_then(SoundSample::end_playback_behaviour_checked)
                == Some(EndPlaybackBehaviour::Next)
                && self.sample(following).is_some()
            {
                match self.play(following) {
                    Ok(p) => next.extend(p),
                    Err(e) => note(Notification::PlaybackError {
                        error: e.to_string(),
                    }),
                }
            }
        }
    }

    pub fn status(&self) -> Vec<VoiceStatus> {
        self.voices
            .iter()
            .flatten()
            .map(|v| VoiceStatus {
                board: v.key.0,
                sample: v.key.1,
                position_secs: frames_to_secs(&v.player, v.player.position()),
                length_secs: v.player.info().frames.map(|f| frames_to_secs(&v.player, f)),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonobus_core::files::Soundboard;
    use sonobus_files::{RecordFormat, RecordSpec, start_recording};

    const SR: u32 = 48000;

    /// A mono WAV of `secs` at `value` (constant), written through the recorder.
    fn wav(name: &str, secs: f64, value: f32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sb-pb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let spec = RecordSpec {
            format: RecordFormat::Wav,
            bits: 16,
            sample_rate: SR,
            channels: 1,
        };
        let (mut w, r) = start_recording(&path, spec).unwrap();
        let x = vec![value; (secs * f64::from(SR)) as usize];
        for c in x.chunks(1024) {
            while !w.write(&[c]) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        r.stop().unwrap()
    }

    fn sample(path: &Path, button: i32, playback: i32, end: i32, replay: i32) -> SoundSample {
        SoundSample {
            name: "s".into(),
            file_path: Some(path.to_string_lossy().into_owned()),
            file_url: format!("file://{}", path.display()),
            button_behaviour: button,
            playback_behaviour: playback,
            end_playback_behaviour: end,
            replay_behaviour: replay,
            ..SoundSample::default()
        }
    }

    /// Stand-in for the audio thread: the installed decks, read block by block.
    #[derive(Default)]
    struct Audio {
        voices: Vec<Option<Box<Deck>>>,
    }

    impl Audio {
        fn apply(&mut self, c: Cmd) {
            if let Cmd::Voice(i, d) = c {
                if self.voices.len() <= i {
                    self.voices.resize_with(i + 1, || None);
                }
                self.voices[i] = d;
            }
        }

        /// Run `blocks` blocks; the summed output of the playing decks, per block (first sample).
        fn run(&mut self, blocks: usize) -> Vec<f32> {
            let mut a = vec![0.0f32; 256];
            (0..blocks)
                .map(|_| {
                    let mut sum = 0.0;
                    for d in self.voices.iter_mut().flatten() {
                        if d.playing.load(Ordering::Relaxed) {
                            d.out.read(&mut [&mut a[..]]);
                            sum += a[128];
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    sum
                })
                .collect()
        }
    }

    /// `press`, with the file opened and installed as the engine does it.
    fn press(sb: &mut SoundboardPlayer, key: SampleKey, out: &mut dyn FnMut(Cmd)) {
        if let Some(p) = sb.press(key, out).unwrap() {
            let o = SoundboardPlayer::open(p, SR).unwrap();
            let (r, dropped) = sb.install(o, out);
            r.unwrap();
            assert!(dropped.is_none());
        }
    }

    fn board(samples: Vec<SoundSample>) -> Soundboards {
        Soundboards {
            boards: vec![Soundboard {
                name: "b".into(),
                samples,
            }],
            ..Soundboards::default()
        }
    }

    /// Wait until the decoders filled their rings, then run.
    fn settle(audio: &mut Audio) {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let _ = audio;
    }

    #[test]
    fn soundboard_behaviours() {
        let long = wav("long.wav", 2.0, 0.25);
        let short = wav("short.wav", 0.1, 0.5);
        let other = wav("other.wav", 2.0, 0.125);
        // 0: toggle simultaneous, 1: one-shot back-to-back next-at-end, 2: hold background
        // continue-from-last, 3: one-shot simultaneous
        let samples = vec![
            sample(&long, 0, 0, 0, 0),
            sample(&short, 2, 1, 2, 0),
            sample(&other, 1, 2, 0, 1),
            sample(&short, 2, 0, 0, 0),
        ];
        let mut sb = SoundboardPlayer::new();
        let mut audio = Audio::default();
        let mut cmds = Vec::new();
        let mut out = |c: Cmd| cmds.push(c);
        sb.set_boards(board(samples), &mut out);

        // toggle: press plays, press again stops
        press(&mut sb, (0, 0), &mut out);
        assert!(sb.is_active((0, 0)));
        press(&mut sb, (0, 0), &mut out);
        assert!(!sb.is_active((0, 0)));

        // simultaneous + background play together
        press(&mut sb, (0, 0), &mut out);
        press(&mut sb, (0, 2), &mut out);
        drop(out);
        cmds.drain(..).for_each(|c| audio.apply(c));
        settle(&mut audio);
        let mix = audio.run(10);
        assert!((mix[9] - 0.375).abs() < 0.01, "both play: {mix:?}");

        // back-to-back stops the toggle sample but not the background one
        let mut out = |c: Cmd| cmds.push(c);
        press(&mut sb, (0, 1), &mut out);
        assert!(!sb.is_active((0, 0)) && sb.is_active((0, 2)) && sb.is_active((0, 1)));

        // hold: release stops; continue-from-last-position resumes later
        sb.release((0, 2), &mut out);
        assert!(!sb.is_active((0, 2)));
        assert!(sb.last_pos[&(0, 2)] > 0.0, "position remembered");
        drop(out);
        cmds.drain(..).for_each(|c| audio.apply(c));
        settle(&mut audio);

        // the short one-shot ends; "next" starts the following sample (2)
        let mut notes = Vec::new();
        for _ in 0..100 {
            audio.run(4);
            let mut out = |c: Cmd| cmds.push(c);
            let mut next = Vec::new();
            sb.poll(&mut out, &mut |n| notes.push(n), &mut next);
            for p in next {
                let o = SoundboardPlayer::open(p, SR).unwrap();
                assert!(sb.install(o, &mut out).1.is_none());
            }
            cmds.drain(..).for_each(|c| audio.apply(c));
            if !notes.is_empty() {
                break;
            }
        }
        assert_eq!(
            notes,
            [Notification::SampleFinished {
                board: 0,
                sample: 1
            }]
        );
        assert!(
            !sb.is_active((0, 1)) && sb.is_active((0, 2)),
            "next one plays"
        );

        // stop all
        let mut out = |c: Cmd| cmds.push(c);
        sb.stop_all(&mut out);
        assert!(sb.status().is_empty());
        // a changed sample at a playing key stops it
        press(&mut sb, (0, 0), &mut out);
        let mut b = sb.boards.clone();
        b.boards[0].samples[0] = sample(&other, 0, 0, 0, 0);
        sb.set_boards(b, &mut out);
        assert!(!sb.is_active((0, 0)));

        // a sample whose entry changes while its file opens is not started; one pressed
        // again meanwhile restarts instead of starting twice
        let p = sb.press((0, 3), &mut out).unwrap().expect("to open");
        let o = SoundboardPlayer::open(p, SR).unwrap();
        let mut b = sb.boards.clone();
        b.boards[0].samples[3] = sample(&long, 2, 0, 0, 0);
        sb.set_boards(b, &mut out);
        let (r, dropped) = sb.install(o, &mut out);
        assert!(r.is_ok() && dropped.is_some() && !sb.is_active((0, 3)));
        let p = sb.press((0, 3), &mut out).unwrap().expect("to open");
        let o = SoundboardPlayer::open(p, SR).unwrap();
        press(&mut sb, (0, 3), &mut out);
        let (r, dropped) = sb.install(o, &mut out);
        assert!(r.is_ok() && dropped.is_some());
        assert_eq!(sb.status().len(), 1, "one voice for the key");
    }

    /// "Continue from last position" resumes where the sample was stopped; a plain sample
    /// starts over.
    #[test]
    fn continue_from_last_position_resumes_there() {
        let long = wav("resume.wav", 2.0, 0.25);
        let mut sb = SoundboardPlayer::new();
        let mut audio = Audio::default();
        let mut cmds = Vec::new();
        let mut out = |c: Cmd| cmds.push(c);
        // 0: toggle, simultaneous, stop at end, continue; 1: the same from the start
        sb.set_boards(
            board(vec![sample(&long, 0, 0, 0, 1), sample(&long, 0, 0, 0, 0)]),
            &mut out,
        );
        let pos = |sb: &SoundboardPlayer, k: usize| {
            sb.status()
                .iter()
                .find(|v| v.sample == k)
                .map(|v| v.position_secs)
        };
        for k in [0, 1] {
            let mut out = |c: Cmd| cmds.push(c);
            press(&mut sb, (0, k), &mut out);
            drop(out);
            cmds.drain(..).for_each(|c| audio.apply(c));
            settle(&mut audio);
            audio.run(100); // 100 x 256 frames: 0.53 s
            let at = pos(&sb, k).unwrap();
            assert!(at > 0.4, "{at}");
            let mut out = |c: Cmd| cmds.push(c);
            press(&mut sb, (0, k), &mut out); // toggle: stop
            assert!(pos(&sb, k).is_none());
            press(&mut sb, (0, k), &mut out); // again
            let again = pos(&sb, k).unwrap();
            if k == 0 {
                assert!((again - at).abs() < 0.01, "resumed at {again}, stopped at {at}");
            } else {
                assert!(again < 0.01, "from the start: {again}");
            }
            sb.stop_all(&mut out);
            drop(out);
            cmds.drain(..).for_each(|c| audio.apply(c));
        }
    }

    /// At most `MAX_VOICES` samples play at once; the next one is refused.
    #[test]
    fn voice_limit() {
        let short = wav("voices.wav", 0.5, 0.1);
        let samples = (0..=MAX_VOICES).map(|_| sample(&short, 1, 0, 0, 0)).collect();
        let mut sb = SoundboardPlayer::new();
        let mut out = |_c: Cmd| {};
        sb.set_boards(board(samples), &mut out);
        for k in 0..MAX_VOICES {
            press(&mut sb, (0, k), &mut out);
        }
        assert_eq!(sb.status().len(), MAX_VOICES);
        let p = sb.press((0, MAX_VOICES), &mut out).unwrap().unwrap();
        let o = SoundboardPlayer::open(p, SR).unwrap();
        let (r, unused) = sb.install(o, &mut out);
        assert!(r.is_err() && unused.is_none());
        assert_eq!(sb.status().len(), MAX_VOICES);
    }

    #[test]
    fn file_url_paths() {
        let s = SoundSample {
            file_url: "file:///tmp/a%20b.wav".into(),
            ..SoundSample::default()
        };
        assert_eq!(sample_path(&s), Some(PathBuf::from("/tmp/a b.wav")));
    }
}
