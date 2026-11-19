//! Drives [`EngineAudio`]: from real devices through a [`Session`], or — with no devices, by
//! choice or while a lost device is being retried — from a "null audio" timer on the manager
//! thread, so networking keeps running either way.
//!
//! One manager thread owns the session (cpal streams are not `Send` everywhere) and does every
//! device open/close, so the UI thread never blocks on a device. Device loss (a stream error,
//! or a stream whose callback stopped) closes the session, falls back to the null timer and
//! retries with backoff 1 s, 2 s, 4 s, ... 10 s ([`Supervisor`]).
//!
//! Getting the engine back from cpal: [`Session::open`] consumes the callback and drops it on
//! failure, and [`Session::close`] returns it as an opaque closure. The callback therefore
//! holds the [`EngineAudio`] in [`RtSlot`], whose `Drop` sends it back to the manager.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sonobus_audio::cpal::traits::{DeviceTrait, HostTrait};
use sonobus_audio::{
    AudioBlock, DeviceId, DeviceInfo, Duplex, ExternalInput, HostId, InputConfig, InputSource,
    InputStats, Session, SessionConfig, cpal, default_margin,
};
use sonobus_engine::{AudioConfig, EngineAudio};

/// A device choice, persisted as `"default"`, `"none"` or a cpal device id (`host:device`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceChoice {
    #[default]
    Default,
    None,
    Id(String),
}

impl DeviceChoice {
    pub fn to_setting(&self) -> String {
        match self {
            DeviceChoice::Default => "default".into(),
            DeviceChoice::None => "none".into(),
            DeviceChoice::Id(s) => s.clone(),
        }
    }

    pub fn from_setting(s: &str) -> Self {
        match s {
            "" | "default" => DeviceChoice::Default,
            "none" => DeviceChoice::None,
            id => DeviceChoice::Id(id.into()),
        }
    }
}

/// The user's audio setup.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceSetup {
    /// false: no devices at all, the null timer drives the engine.
    pub enabled: bool,
    /// cpal host id (lowercase name); `None` = the platform default.
    pub host: Option<String>,
    pub output: DeviceChoice,
    pub input: DeviceChoice,
    /// Channels to open on the input device; `None` = its default.
    pub input_channels: Option<u16>,
    /// An output device whose playback is captured as an extra input after the device input
    /// ("System sound", [`InputSource::Loopback`]).
    pub system_sound: Option<String>,
    /// Audio the platform pushes through [`AudioManager::external_input`] (Android playback
    /// capture), an input after the device input and system sound. Not persisted: capture
    /// needs a fresh consent every run.
    pub external: Option<ExternalSpec>,
    /// Engine rate; the output device must support it, inputs are resampled.
    pub sample_rate: u32,
    /// Engine block size in frames.
    pub block_size: usize,
    /// Requested device buffer; `None` = backend default.
    pub buffer_frames: Option<u32>,
}

impl Default for DeviceSetup {
    fn default() -> Self {
        DeviceSetup {
            enabled: true,
            host: None,
            output: DeviceChoice::Default,
            input: DeviceChoice::Default,
            input_channels: None,
            system_sound: None,
            external: None,
            sample_rate: 48000,
            block_size: 256,
            buffer_frames: None,
        }
    }
}

/// Format of platform-pushed input audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalSpec {
    pub channels: u16,
    pub sample_rate: u32,
}

/// Where the platform pushes [`DeviceSetup::external`] audio, from any one thread at a time:
/// `None` while no session or null driver runs. Use [`push_external`].
pub type ExternalSlot = Arc<Mutex<Option<ExternalInput>>>;

/// Pushes interleaved f32 frames into the slot's input; returns the frames taken (0 while
/// none is open, or for this one buffer while the manager swaps the input). Never blocks.
pub fn push_external(slot: &ExternalSlot, interleaved: &[f32]) -> usize {
    // ponytail: try_lock, not a lock-free swap: the manager holds it only to swap the input.
    match slot.try_lock() {
        Ok(mut x) => x.as_mut().map_or(0, |x| x.push(interleaved)),
        Err(_) => 0,
    }
}

/// Engine input channels kept after the device input for system sound / platform capture, so
/// switching them on mid-session needs no engine rebuild (which drops the session's streams).
pub const RESERVED_EXTRA_CHANNELS: u16 = SYSTEM_SOUND_CHANNELS;

/// Channels assumed for an input opened with its default channel count.
pub const DEFAULT_INPUT_CHANNELS: u16 = 2;
/// Channels captured from a system-sound source.
pub const SYSTEM_SOUND_CHANNELS: u16 = 2;

impl DeviceSetup {
    /// What the engine is built for. The mixer copes with a device that opens a different
    /// channel count; rate and block size must match the session (they are passed explicitly).
    pub fn audio_config(&self) -> AudioConfig {
        AudioConfig {
            sample_rate: self.sample_rate,
            block_size: self.block_size,
            input_channels: usize::from(self.device_input_channels() + self.extra_input_channels().max(RESERVED_EXTRA_CHANNELS)),
            output_channels: 2,
        }
    }

    /// Channels of the device input (0 without one); the system sound follows them.
    pub fn device_input_channels(&self) -> u16 {
        match self.input {
            DeviceChoice::None => 0,
            _ => self.input_channels.unwrap_or(DEFAULT_INPUT_CHANNELS),
        }
    }

    /// Channels of system sound and platform input, which follow the device input.
    pub fn extra_input_channels(&self) -> u16 {
        let sys = if self.system_sound.is_some() { SYSTEM_SOUND_CHANNELS } else { 0 };
        sys + self.external.map_or(0, |e| e.channels)
    }

    pub fn host_id(&self) -> Result<HostId, String> {
        match &self.host {
            Some(h) => h.parse().map_err(|e: cpal::Error| e.to_string()),
            None => Ok(cpal::default_host().id()),
        }
    }

    /// Resolves "default" devices (a cpal query: call off the UI thread).
    fn session_config(&self) -> Result<SessionConfig, String> {
        let host_id = self.host_id()?;
        let host = cpal::host_from_id(host_id).map_err(|e| e.to_string())?;
        let pick = |c: &DeviceChoice, output: bool| -> Result<Option<DeviceId>, String> {
            match c {
                DeviceChoice::None => Ok(None),
                DeviceChoice::Id(s) => s.parse().map(Some).map_err(|e: cpal::Error| e.to_string()),
                DeviceChoice::Default => {
                    let d = if output { host.default_output_device() } else { host.default_input_device() };
                    Ok(d.and_then(|d| d.id().ok()))
                }
            }
        };
        let output = pick(&self.output, true)?;
        let mut inputs = Vec::new();
        if let Some(id) = pick(&self.input, false)? {
            let channels = Some(self.device_input_channels());
            inputs.push(InputConfig { source: InputSource::Device(id), channels });
        }
        if let Some(s) = &self.system_sound {
            let id = s.parse().map_err(|e: cpal::Error| e.to_string())?;
            inputs.push(InputConfig {
                source: InputSource::Loopback(id),
                channels: Some(SYSTEM_SOUND_CHANNELS),
            });
        }
        if let Some(e) = self.external {
            inputs.push(InputConfig {
                source: InputSource::External { channels: e.channels, sample_rate: e.sample_rate },
                channels: None,
            });
        }
        if output.is_none() && inputs.is_empty() {
            return Err(crate::tr("No audio device available").into());
        }
        Ok(SessionConfig {
            host: host_id,
            output,
            output_channels: None,
            inputs,
            sample_rate: Some(self.sample_rate),
            block_size: self.block_size,
            buffer_frames: self.buffer_frames,
        })
    }
}

// ---- retry state machine ----------------------------------------------------------------------

pub const RETRY_MIN: Duration = Duration::from_secs(1);
pub const RETRY_MAX: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceState {
    /// No devices wanted: null audio.
    Off,
    Running,
    /// Devices failed; null audio meanwhile, next attempt at `at`.
    Retrying { error: String, at: Instant },
}

/// Device open/loss bookkeeping with exponential backoff, free of I/O so it can be tested
/// with a fake device.
#[derive(Debug, Clone)]
pub struct Supervisor {
    pub state: DeviceState,
    delay: Duration,
}

impl Default for Supervisor {
    fn default() -> Self {
        Supervisor { state: DeviceState::Off, delay: RETRY_MIN }
    }
}

impl Supervisor {
    /// Result of an open attempt.
    pub fn opened(&mut self, now: Instant, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.state = DeviceState::Running;
                self.delay = RETRY_MIN;
            }
            Err(error) => {
                self.state = DeviceState::Retrying { error, at: now + self.delay };
                self.delay = (self.delay * 2).min(RETRY_MAX);
            }
        }
    }

    /// A running device failed or stalled: first retry after [`RETRY_MIN`].
    pub fn lost(&mut self, now: Instant, error: String) {
        self.delay = RETRY_MIN;
        self.opened(now, Err(error));
    }

    pub fn due(&self, now: Instant) -> bool {
        matches!(&self.state, DeviceState::Retrying { at, .. } if now >= *at)
    }

    pub fn retry_now(&mut self, now: Instant) {
        if let DeviceState::Retrying { at, .. } = &mut self.state {
            *at = now;
        }
    }

    pub fn off(&mut self) {
        *self = Supervisor::default();
    }
}

// ---- manager ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct AudioStatus {
    /// `None` until the engine is started.
    pub state: Option<DeviceState>,
    pub sample_rate: u32,
    pub block_size: usize,
    pub xruns: u64,
    /// Per session input, in setup order (device input, then system sound).
    pub inputs: Vec<InputStats>,
    /// Channels each session input actually opened, in setup order (empty without a device
    /// session: null audio).
    pub input_channels: Vec<u16>,
    /// Audio hosts compiled in and available.
    pub hosts: Vec<String>,
    /// Devices of the last scanned host.
    pub devices: Vec<DeviceInfo>,
    pub scan_error: Option<String>,
    pub scanning: bool,
    /// The engine audio could not be recovered from a session: the controller must rebuild.
    pub engine_lost: bool,
}

impl AudioStatus {
    /// Input buffering latency: the fullest input ring plus one block, in ms.
    pub fn input_latency_ms(&self) -> Option<f32> {
        let max = self.inputs.iter().map(|s| s.fill_frames).max()?;
        (self.sample_rate > 0).then(|| 1000.0 * (max + self.block_size) as f32 / self.sample_rate as f32)
    }

    pub fn underflows(&self) -> u64 {
        self.inputs.iter().map(|s| s.underflows).sum()
    }
}

enum Cmd {
    Start(Box<EngineAudio>, DeviceSetup),
    Setup(DeviceSetup),
    Scan(Option<String>),
    RetryNow,
}

/// Owns the manager thread; dropping it stops audio.
pub struct AudioManager {
    tx: Option<Sender<Cmd>>,
    status: Arc<Mutex<AudioStatus>>,
    external: ExternalSlot,
    thread: Option<JoinHandle<()>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for AudioManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioManager {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(AudioStatus::default()));
        let external = ExternalSlot::default();
        let (s, x) = (status.clone(), external.clone());
        let thread = std::thread::Builder::new()
            .name("sonobus-audio-manager".into())
            .spawn(move || Manager::new(s, x).run(&rx))
            .expect("spawn audio manager");
        AudioManager { tx: Some(tx), status, external, thread: Some(thread) }
    }

    fn send(&self, c: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(c);
        }
    }

    /// Run this engine audio with `setup` (replacing any previous one).
    pub fn start(&self, rt: EngineAudio, setup: DeviceSetup) {
        self.send(Cmd::Start(Box::new(rt), setup));
    }

    /// Reopen the devices with a new setup, same engine audio.
    pub fn set_setup(&self, setup: DeviceSetup) {
        self.send(Cmd::Setup(setup));
    }

    /// Enumerate the devices of `host` (`None` = default host) in the background.
    pub fn scan(&self, host: Option<String>) {
        lock(&self.status).scanning = true;
        self.send(Cmd::Scan(host));
    }

    pub fn retry_now(&self) {
        self.send(Cmd::RetryNow);
    }

    /// Where [`DeviceSetup::external`] audio goes; the same slot for the manager's lifetime.
    pub fn external_input(&self) -> ExternalSlot {
        self.external.clone()
    }

    pub fn status(&self) -> AudioStatus {
        lock(&self.status).clone()
    }
}

impl Drop for AudioManager {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type Callback = Box<dyn FnMut(&mut AudioBlock<'_>) + Send>;

/// The engine audio inside a device callback; sent back to the manager when dropped.
struct RtSlot {
    audio: Option<Box<EngineAudio>>,
    back: SyncSender<Box<EngineAudio>>,
}

impl Drop for RtSlot {
    fn drop(&mut self) {
        if let Some(a) = self.audio.take() {
            let _ = self.back.try_send(a);
        }
    }
}

/// Real-time paced driver for when there is no device: silent device inputs, the platform
/// input if configured, output discarded. Allocation-free per block.
struct NullDriver {
    next: Instant,
    frames: usize,
    block: Duration,
    duplex: Duplex<Callback>,
}

impl NullDriver {
    /// Also returns the producer of the [`DeviceSetup::external`] input.
    fn new(setup: &DeviceSetup, engine: Callback) -> (Self, Option<ExternalInput>) {
        let cfg = setup.audio_config();
        let rate = cfg.sample_rate.max(1);
        let mut duplex = Duplex::new(engine, rate, cfg.block_size.max(1), 2);
        let ext_ch = setup.external.map_or(0, |e| usize::from(e.channels));
        let fixed = cfg.input_channels.saturating_sub(ext_ch).min(sonobus_audio::MAX_CHANNELS - ext_ch);
        if fixed > 0 {
            // never fed: keeps the platform input on its engine channels, silent
            drop(duplex.add_input(fixed, rate, default_margin(rate)));
        }
        let ext = setup.external.map(|e| duplex.add_input(ext_ch, e.sample_rate, default_margin(e.sample_rate)));
        let driver = NullDriver {
            next: Instant::now(),
            frames: cfg.block_size.max(1),
            block: Duration::from_secs_f64(cfg.block_size as f64 / f64::from(rate)),
            duplex,
        };
        (driver, ext)
    }

    /// Processes every block that is due; returns when the next one is.
    fn drive(&mut self) -> Instant {
        let now = Instant::now();
        if now.saturating_duration_since(self.next) > Duration::from_millis(200) {
            self.next = now; // fell far behind (suspended): resume instead of bursting
        }
        while self.next <= now {
            self.duplex.advance(self.frames);
            self.next += self.block;
        }
        self.next
    }
}

struct Manager {
    status: Arc<Mutex<AudioStatus>>,
    sup: Supervisor,
    setup: DeviceSetup,
    rt: Option<Box<EngineAudio>>,
    session: Option<Session<Callback>>,
    null: Option<NullDriver>,
    back_tx: SyncSender<Box<EngineAudio>>,
    back_rx: Receiver<Box<EngineAudio>>,
    last_check: Instant,
    external: ExternalSlot,
}

impl Manager {
    fn new(status: Arc<Mutex<AudioStatus>>, external: ExternalSlot) -> Self {
        let (back_tx, back_rx) = sync_channel(1);
        Manager {
            status,
            sup: Supervisor::default(),
            setup: DeviceSetup::default(),
            rt: None,
            session: None,
            null: None,
            back_tx,
            back_rx,
            last_check: Instant::now(),
            external,
        }
    }

    fn run(mut self, rx: &Receiver<Cmd>) {
        loop {
            let now = Instant::now();
            let wait = match &mut self.null {
                Some(null) => null.drive().saturating_duration_since(Instant::now()),
                None => Duration::from_millis(200),
            }
            .min(Duration::from_millis(200));
            match rx.recv_timeout(wait) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.watch(now);
            self.publish();
        }
        self.close();
        self.stop_null();
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start(rt, setup) => {
                self.close();
                self.stop_null();
                lock(&self.status).engine_lost = false;
                self.rt = Some(rt);
                self.apply(setup);
            }
            Cmd::Setup(setup) => {
                self.close();
                self.stop_null();
                self.apply(setup);
            }
            Cmd::Scan(host) => {
                let status = self.status.clone();
                std::thread::spawn(move || {
                    let hosts = sonobus_audio::hosts().iter().map(|h| h.to_string()).collect();
                    let id = match host {
                        Some(h) => h.parse::<HostId>().map_err(|e| e.to_string()),
                        None => Ok(cpal::default_host().id()),
                    };
                    let devs = id.and_then(|id| sonobus_audio::devices(id).map_err(|e| e.to_string()));
                    let mut s = lock(&status);
                    s.hosts = hosts;
                    s.scanning = false;
                    match devs {
                        Ok(d) => (s.devices, s.scan_error) = (d, None),
                        Err(e) => (s.devices, s.scan_error) = (Vec::new(), Some(e)),
                    }
                });
            }
            Cmd::RetryNow => self.sup.retry_now(Instant::now()),
        }
    }

    fn apply(&mut self, setup: DeviceSetup) {
        let enabled = setup.enabled;
        self.setup = setup;
        self.sup.off();
        if enabled {
            self.try_open();
        } else {
            self.start_null();
        }
    }

    /// The engine audio as a device callback (it comes back through `back_rx` when dropped).
    fn callback(&mut self) -> Option<Callback> {
        let mut slot = RtSlot { audio: Some(self.rt.take()?), back: self.back_tx.clone() };
        Some(Box::new(move |b: &mut AudioBlock<'_>| {
            if let Some(a) = slot.audio.as_mut() {
                a.process(b.inputs, b.outputs);
            }
        }))
    }

    fn start_null(&mut self) {
        self.stop_null();
        if let Some(cb) = self.callback() {
            let (null, ext) = NullDriver::new(&self.setup, cb);
            self.null = Some(null);
            *lock(&self.external) = ext;
        }
    }

    fn stop_null(&mut self) {
        if self.null.take().is_some() {
            *lock(&self.external) = None;
            self.recover();
        }
    }

    fn try_open(&mut self) {
        self.stop_null();
        let Some(cb) = self.callback() else { return };
        let result = self
            .setup
            .session_config()
            .and_then(|cfg| Session::open(cfg, cb).map_err(|e| e.to_string()));
        let now = Instant::now();
        match result {
            Ok(mut s) => {
                let ext = s.config().inputs.iter().position(|i| matches!(i.source, InputSource::External { .. }));
                *lock(&self.external) = ext.and_then(|i| s.take_external_input(i));
                self.session = Some(s);
                self.last_check = now;
                self.sup.opened(now, Ok(()));
            }
            Err(e) => {
                // a failed open dropped the callback: the engine audio is back in the channel
                self.recover();
                self.start_null();
                self.sup.opened(now, Err(e));
            }
        }
    }

    fn recover(&mut self) {
        match self.back_rx.try_recv() {
            Ok(rt) => self.rt = Some(rt),
            Err(_) => lock(&self.status).engine_lost = true,
        }
    }

    fn close(&mut self) {
        if let Some(s) = self.session.take() {
            *lock(&self.external) = None;
            drop(s.close());
            self.recover();
        }
    }

    /// Device health: stream errors right away, stalls once a second; retries when due.
    fn watch(&mut self, now: Instant) {
        if let Some(s) = &mut self.session {
            let mut error = s.events().next().map(|e| e.error.to_string());
            if error.is_none() && now.duration_since(self.last_check) >= Duration::from_secs(1) {
                self.last_check = now;
                if !s.stalled().is_empty() {
                    error = Some(crate::tr("Audio device stopped responding").into());
                }
            }
            if let Some(e) = error {
                self.close();
                self.start_null();
                self.sup.lost(now, e);
            }
        } else if self.sup.due(now) {
            self.try_open();
        }
    }

    fn publish(&self) {
        let mut st = lock(&self.status);
        st.state = (self.rt.is_some() || self.session.is_some() || self.null.is_some()).then(|| self.sup.state.clone());
        let cfg = self.setup.audio_config();
        st.sample_rate = cfg.sample_rate;
        st.block_size = cfg.block_size;
        match &self.session {
            Some(s) => {
                st.xruns = s.xruns();
                st.inputs = s.input_stats();
                st.input_channels = s.input_channels().to_vec();
            }
            None => {
                st.inputs.clear();
                st.input_channels.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake device layer: opens fail while `fail_opens > 0`; a running stream can be broken.
    struct FakeDevice {
        fail_opens: u32,
        running: bool,
        broken: bool,
    }

    impl FakeDevice {
        fn open(&mut self) -> Result<(), String> {
            if self.fail_opens > 0 {
                self.fail_opens -= 1;
                return Err("device not found".into());
            }
            self.running = true;
            Ok(())
        }
    }

    /// The manager loop's decisions, over the fake device and a fake clock (100 ms ticks).
    /// Returns the times (s) of each open attempt.
    fn simulate(dev: &mut FakeDevice, sup: &mut Supervisor, t0: Instant, secs: u64, break_at: Option<u64>) -> Vec<f64> {
        let mut attempts = Vec::new();
        for tick in 0..secs * 10 {
            let now = t0 + Duration::from_millis(tick * 100);
            if break_at == Some(tick / 10) && tick % 10 == 0 && dev.running {
                dev.broken = true;
            }
            if dev.running && dev.broken {
                dev.running = false;
                dev.broken = false;
                sup.lost(now, "stream stalled".into());
            } else if !dev.running && sup.due(now) {
                attempts.push((now - t0).as_secs_f64());
                sup.opened(now, dev.open());
            }
        }
        attempts
    }

    #[test]
    fn retry_backoff_doubles_to_ten_seconds_then_recovers() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        let mut dev = FakeDevice { fail_opens: 7, running: false, broken: false };
        // initial attempt at t=0 fails
        sup.opened(t0, dev.open());
        let attempts = simulate(&mut dev, &mut sup, t0, 60, None);
        // retries after 1, 2, 4, 8, 10, 10 s; the 7th attempt (6th retry) succeeds
        assert_eq!(attempts, vec![1.0, 3.0, 7.0, 15.0, 25.0, 35.0, 45.0]);
        assert_eq!(sup.state, DeviceState::Running);
        assert!(dev.running);
    }

    #[test]
    fn loss_retries_after_one_second() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        let mut dev = FakeDevice { fail_opens: 0, running: false, broken: false };
        sup.opened(t0, dev.open());
        let attempts = simulate(&mut dev, &mut sup, t0, 10, Some(3));
        assert_eq!(attempts, vec![4.0]);
        assert_eq!(sup.state, DeviceState::Running);
    }

    #[test]
    fn retry_now_and_off() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        sup.opened(t0, Err("x".into()));
        assert!(!sup.due(t0));
        sup.retry_now(t0);
        assert!(sup.due(t0));
        sup.off();
        assert_eq!(sup.state, DeviceState::Off);
        assert!(!sup.due(t0 + RETRY_MAX));
    }

    #[test]
    fn null_audio_drives_engine() {
        use sonobus_engine::{Engine, EngineConfig};
        let setup = DeviceSetup { enabled: false, ..DeviceSetup::default() };
        let (engine, rt) = Engine::new(EngineConfig {
            audio: setup.audio_config(),
            udp_port: 0,
            state: Default::default(),
        })
        .unwrap();
        // the metronome only plays while the engine is driven
        engine.update_state(|s| {
            s.params.met_enabled = true;
            s.params.dry = 1.0;
        });
        let m = AudioManager::new();
        m.start(rt, setup);
        let t = Instant::now();
        while engine.snapshot().output.peak.iter().all(|&p| p == 0.0) {
            assert!(t.elapsed() < Duration::from_secs(5), "no output from the null driver");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(m.status().state, Some(DeviceState::Off));
    }

    #[test]
    fn device_setting_round_trip() {
        for c in [DeviceChoice::Default, DeviceChoice::None, DeviceChoice::Id("alsa:hw:0,0".into())] {
            assert_eq!(DeviceChoice::from_setting(&c.to_setting()), c);
        }
    }
}
