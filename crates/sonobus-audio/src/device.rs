//! cpal devices: enumeration for the UI, and a [`Session`] that opens the configured output and
//! inputs and bridges them into one [`Duplex`] engine callback.
//!
//! cpal has no duplex stream, so every input runs on its own stream and clock and feeds an
//! [`ExternalInput`] ring; the output stream's callback runs the engine. Without an output
//! device the first device input's callback drives the engine instead.
//!
//! System-sound ("loopback") capture, by cpal host (cpal 0.18.2):
//! - WASAPI: an input stream on a render endpoint is opened with `AUDCLNT_STREAMFLAGS_LOOPBACK`
//!   (`host/wasapi/device.rs:854`).
//! - CoreAudio: an input stream on a device without input creates a process tap
//!   (`CATapDescription`, all processes) plus a private aggregate device and records that
//!   (`host/coreaudio/macos/device.rs:727`, `loopback.rs:86`). Needs the macOS 14.2+ tap API;
//!   a device that also has inputs opens its inputs instead.
//! - PipeWire: `Audio/Sink` nodes are listed as duplex (`host/pipewire/device.rs:910`) and an
//!   input stream on one sets `stream.capture.sink` (`device.rs:167`), capturing its monitor.
//! - PulseAudio: sinks cannot be opened for input (`host/pulseaudio/mod.rs:326`); every sink has
//!   a `<sink name>.monitor` source, listed among the inputs, which is what we open.
//! - ALSA, JACK, ASIO, AAudio: none.
//!
//! Per-application capture ([`InputSource::App`]): PipeWire only (cargo feature `pipewire`).
//! cpal lists every `Stream/Output/Audio` node (an application's playback stream) as an
//! output-only device (`host/pipewire/device.rs:920`). An input stream built on one is a
//! capture stream with `target.object` = that node's serial and `DONT_RECONNECT`
//! (`device.rs:171`, `stream.rs:1039`): the session manager links it to the application's
//! output ports, and when the application's stream goes away cpal reports `DeviceNotAvailable`
//! (`stream.rs:266`) instead of falling back to the default source. No `supports_input` check
//! stands in the way (`traits.rs:273`). The capture runs on the PipeWire host whatever host
//! the session uses: every input has its own stream and clock anyway.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, DeviceId, Error, ErrorKind, FromSample, Host, HostId, SampleFormat,
    SizedSample, StreamConfig, SupportedBufferSize, SupportedStreamConfig,
    SupportedStreamConfigRange,
};

use crate::{AudioBlock, Duplex, ExternalInput, InputMonitor, default_margin};

const STANDARD_RATES: [u32; 11] =
    [8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000];

/// Audio backends compiled in and usable on this system.
pub fn hosts() -> Vec<HostId> {
    cpal::available_hosts()
}

/// The host "System default" means. cpal prefers PipeWire whenever its feature is compiled in
/// and a daemon runs (`platform/mod.rs:833`); SonoBus keeps the default it had without it
/// (PulseAudio if compiled in and running, else ALSA on Linux; the platform host elsewhere), so
/// saved `alsa:` device ids keep working. PipeWire stays selectable, and per-application capture
/// uses it whatever the session host is.
pub fn default_host() -> HostId {
    let hosts = cpal::available_hosts();
    ["PulseAudio", "ALSA"]
        .into_iter()
        .find_map(|n| hosts.iter().copied().find(|h| h.name() == n))
        .unwrap_or_else(|| cpal::default_host().id())
}

/// One device as the UI lists it. Inputs are devices with `input_channels > 0`, outputs those
/// with `output_channels > 0`, system-sound sources those with `loopback`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    /// Persist with `to_string()`, restore with `parse()`; stable across runs where the backend
    /// allows (ALSA PCM names, WASAPI endpoint ids, CoreAudio UIDs, PipeWire node names).
    pub id: DeviceId,
    pub name: String,
    pub input_channels: u16,
    pub output_channels: u16,
    /// Standard rates any input or output configuration accepts.
    pub sample_rates: Vec<u32>,
    /// Device buffer size range in frames, if the backend reports one.
    pub buffer_size: Option<(u32, u32)>,
    pub default_input: bool,
    pub default_output: bool,
    /// Can be opened as [`InputSource::Loopback`] to capture what it plays.
    pub loopback: bool,
}

/// Lists the devices of `host`. Devices that vanish or refuse queries mid-scan get empty
/// capabilities rather than an error.
pub fn devices(host: HostId) -> Result<Vec<DeviceInfo>, Error> {
    let host = cpal::host_from_id(host)?;
    let def_in = host.default_input_device().and_then(|d| d.id().ok());
    let def_out = host.default_output_device().and_then(|d| d.id().ok());
    let mut out = Vec::new();
    for dev in host.devices()? {
        let Ok(id) = dev.id() else { continue };
        if is_app_stream(host.id(), &dev) {
            continue; // an application's playback, not a device: see `app_sources`
        }
        let ins: Vec<_> = dev.supported_input_configs().map(Iterator::collect).unwrap_or_default();
        let outs: Vec<_> = dev.supported_output_configs().map(Iterator::collect).unwrap_or_default();
        let all = || ins.iter().chain(&outs);
        let buffer_size = all()
            .filter_map(|c| match *c.buffer_size() {
                SupportedBufferSize::Range { min, max } => Some((min, max)),
                SupportedBufferSize::Unknown => None,
            })
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)));
        out.push(DeviceInfo {
            name: dev.description().map(|d| d.name().to_owned()).unwrap_or_else(|_| dev.to_string()),
            input_channels: ins.iter().map(SupportedStreamConfigRange::channels).max().unwrap_or(0),
            output_channels: outs.iter().map(SupportedStreamConfigRange::channels).max().unwrap_or(0),
            sample_rates: STANDARD_RATES
                .into_iter()
                .filter(|&r| all().any(|c| c.contains_rate(r)))
                .collect(),
            buffer_size,
            default_input: def_in.as_ref() == Some(&id),
            default_output: def_out.as_ref() == Some(&id),
            loopback: loopback_capable(host.id(), &dev),
            id,
        });
    }
    Ok(out)
}

fn loopback_capable(host: HostId, dev: &Device) -> bool {
    match host.name() {
        "WASAPI" | "PipeWire" | "PulseAudio" => dev.supports_output(),
        "CoreAudio" => dev.supports_output() && !dev.supports_input(),
        _ => false,
    }
}

/// One application's playback that [`InputSource::App`] can capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSource {
    /// Persist it; stable across runs (the PipeWire `node.name`, usually the application name).
    pub id: String,
    pub name: String,
}

/// The PipeWire host, if compiled in (cargo feature `pipewire`) and a daemon is running.
fn pipewire_host() -> Option<HostId> {
    cpal::available_hosts().into_iter().find(|h| h.name() == "PipeWire")
}

/// Whether single applications can be captured here (PipeWire). Cheap: no daemon round trip.
pub fn app_capture_supported() -> bool {
    pipewire_host().is_some()
}

/// Applications playing right now, ours excluded (capturing them would loop our output back).
/// Empty when [`app_capture_supported`] is false. Talks to the daemon: not on the UI thread.
/// Every call is a daemon round trip (a new cpal PipeWire host binds every node, up to 2 s on a
/// stuck daemon); the next [`InputSource::App`] opened on this thread within [`APP_REUSE`] uses
/// this listing instead of making another.
pub fn app_sources() -> Vec<AppSource> {
    let streams = list_app_streams();
    let nodes = streams.iter().filter_map(|d| {
        let id = d.id().ok()?.id().to_owned();
        let desc = d.description().map(|x| x.name().to_owned()).unwrap_or_default();
        Some((id, desc))
    });
    let out = app_list(nodes, &own_names());
    LAST_APPS.with(|l| *l.borrow_mut() = Some((Instant::now(), streams)));
    out
}

/// How long an [`app_sources`] listing is reused for opening an application.
pub const APP_REUSE: Duration = Duration::from_secs(3);

thread_local! {
    /// The application streams behind this thread's last [`app_sources`] call.
    static LAST_APPS: RefCell<Option<(Instant, Vec<Device>)>> = const { RefCell::new(None) };
}

fn list_app_streams() -> Vec<Device> {
    let Some(host) = pipewire_host().and_then(|h| cpal::host_from_id(h).ok()) else { return Vec::new() };
    let Ok(devs) = host.devices() else { return Vec::new() };
    devs.filter(|d| is_app_stream(host.id(), d)).collect()
}

/// The stream of application `app`: from this thread's last listing if recent, else a new one.
fn find_app_stream(app: &str) -> Option<Device> {
    let is = |d: &Device| d.id().is_ok_and(|i| i.id() == app);
    let recent = LAST_APPS.with(|l| {
        let l = l.borrow();
        let (_, devs) = l.as_ref().filter(|(at, _)| at.elapsed() < APP_REUSE)?;
        devs.iter().find(|d| is(d)).cloned()
    });
    recent.or_else(|| list_app_streams().into_iter().find(is))
}

/// A PipeWire `Stream/Output/Audio` node: the only output-only devices but cpal's synthetic
/// default output (sinks are listed as duplex, `host/pipewire/device.rs:916`).
fn is_app_stream(host: HostId, dev: &Device) -> bool {
    host.name() == "PipeWire"
        && dev.supports_output()
        && !dev.supports_input()
        && dev.id().is_ok_and(|id| id.id() != "output_default")
}

/// Names our own streams' nodes get. A native PipeWire stream (session on the PipeWire host) is
/// named after `application.name`, which defaults to the program name (`pw_stream_new`,
/// stream.c), with cpal's stream name (`cpal-playback-<pid>`) as the last fallback; a stream
/// through pipewire-alsa (session on the ALSA host) is `alsa_playback.<program name>`.
// ponytail: by name, so other processes of the same program are hidden too; matching
// `application.process.id` needs node properties cpal does not expose (a direct pipewire-rs
// registry listener).
fn own_names() -> Vec<String> {
    let base = |p: std::path::PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned());
    let exe = std::env::current_exe().ok().and_then(base);
    let arg0 = std::env::args_os().next().map(std::path::PathBuf::from).and_then(base);
    exe.into_iter()
        .chain(arg0)
        .flat_map(|n| [format!("alsa_playback.{n}"), format!("alsa_capture.{n}"), n])
        .collect()
}

/// `(node.name, node.description)` of application streams to the list: ours dropped, one entry
/// per name (opening picks the first stream of that name).
// ponytail: keyed by node.name, so two streams of one app (two browser tabs) are one entry;
// key by object.serial too if users need to pick between them.
fn app_list(nodes: impl Iterator<Item = (String, String)>, own: &[String]) -> Vec<AppSource> {
    let mut out: Vec<AppSource> = Vec::new();
    for (id, desc) in nodes {
        if id.starts_with("cpal-") || own.contains(&id) || out.iter().any(|a| a.id == id) {
            continue;
        }
        let name = match desc.as_str() {
            "" | "unknown" => id.clone(),
            d if d == id => id.clone(),
            d => format!("{id}: {d}"),
        };
        out.push(AppSource { id, name });
    }
    out
}

/// Polls a host's device list and reports what appeared or disappeared (cpal has no hot-plug
/// notification). Call every second or so from a non-audio thread.
pub struct DeviceWatcher {
    host: Host,
    known: HashSet<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceChange {
    pub added: Vec<DeviceId>,
    pub removed: Vec<DeviceId>,
}

impl DeviceWatcher {
    pub fn new(host: HostId) -> Result<Self, Error> {
        let host = cpal::host_from_id(host)?;
        let known = device_ids(&host)?;
        Ok(DeviceWatcher { host, known })
    }

    pub fn poll(&mut self) -> Result<Option<DeviceChange>, Error> {
        let now = device_ids(&self.host)?;
        Ok(diff(&mut self.known, now))
    }
}

fn device_ids(host: &Host) -> Result<HashSet<DeviceId>, Error> {
    Ok(host.devices()?.filter_map(|d| d.id().ok()).collect())
}

fn diff(known: &mut HashSet<DeviceId>, now: HashSet<DeviceId>) -> Option<DeviceChange> {
    let added: Vec<_> = now.difference(known).cloned().collect();
    let removed: Vec<_> = known.difference(&now).cloned().collect();
    *known = now;
    (!added.is_empty() || !removed.is_empty()).then_some(DeviceChange { added, removed })
}

/// Where an engine input's audio comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum InputSource {
    /// A capture device.
    Device(DeviceId),
    /// What an output device plays ("share system sound"); see [`DeviceInfo::loopback`].
    Loopback(DeviceId),
    /// One application's playback, by [`AppSource::id`] (see [`app_sources`]). PipeWire only;
    /// elsewhere opening it fails with `UnsupportedOperation`. When the application stops its
    /// stream the session reports a `DeviceNotAvailable` [`StreamEvent`], as for a lost device.
    App(String),
    /// Audio pushed by the application through [`Session::take_external_input`], e.g. Android
    /// playback capture arriving from a Java `AudioRecord` thread via JNI.
    External { channels: u16, sample_rate: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct InputConfig {
    pub source: InputSource,
    /// Channels to open; `None` = the device default. Ignored for `External`.
    pub channels: Option<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    pub host: HostId,
    /// `None` = no output device: the first device input drives the engine.
    pub output: Option<DeviceId>,
    /// Output channels; `None` = device default (2 when there is no output device).
    pub output_channels: Option<u16>,
    /// Inputs in engine-channel order.
    pub inputs: Vec<InputConfig>,
    /// Engine rate; `None` = the output device's default (else the first input's).
    pub sample_rate: Option<u32>,
    /// Engine block size in frames, fixed for the session.
    pub block_size: usize,
    /// Requested device buffer size; `None` = backend default.
    pub buffer_frames: Option<u32>,
}

/// Which stream of a session something happened to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamRole {
    Output,
    /// Index into [`SessionConfig::inputs`].
    Input(usize),
}

/// A non-xrun stream error reported by the backend (xruns are only counted).
#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub role: StreamRole,
    pub error: Error,
}

struct Resolved {
    device: Device,
    config: StreamConfig,
    format: SampleFormat,
}

/// Returns the engine to the session when the stream owning it is dropped (stream teardown is
/// not real-time). While the stream runs, the engine is owned by its callback: no lock there.
struct Holder<F> {
    duplex: Option<Duplex<F>>,
    slot: Arc<Mutex<Option<Duplex<F>>>>,
}

impl<F> Drop for Holder<F> {
    fn drop(&mut self) {
        if let Some(d) = self.duplex.take() {
            *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
        }
    }
}

/// Open devices driving one engine callback.
pub struct Session<F> {
    config: SessionConfig,
    sample_rate: u32,
    streams: Vec<(StreamRole, cpal::Stream, Arc<AtomicU64>, u64)>,
    slot: Arc<Mutex<Option<Duplex<F>>>>,
    externals: Vec<Option<ExternalInput>>,
    monitors: Vec<InputMonitor>,
    /// Channels each input opened, in configuration order.
    input_channels: Vec<u16>,
    xruns: Arc<AtomicU64>,
    tx: Sender<StreamEvent>,
    rx: Receiver<StreamEvent>,
}

fn not_found(id: &DeviceId) -> Error {
    Error::with_message(ErrorKind::DeviceNotAvailable, format!("device {id} not found"))
}

fn find(host: &Host, id: &DeviceId) -> Result<Device, Error> {
    // AAudio's default device ("aaudio:-1") is not in `devices()`, so `device_by_id` misses it.
    host.device_by_id(id)
        .or_else(|| {
            [host.default_output_device(), host.default_input_device()]
                .into_iter()
                .flatten()
                .find(|d| d.id().is_ok_and(|i| i == *id))
        })
        .ok_or_else(|| not_found(id))
}

/// Picks a configuration: requested channels and rate, preferring f32, then the default
/// format. Backends that list no configurations get the default config with the overrides.
fn pick(
    configs: &[SupportedStreamConfigRange],
    default: Option<SupportedStreamConfig>,
    rate: Option<u32>,
    channels: Option<u16>,
    buffer: Option<u32>,
) -> Result<(StreamConfig, SampleFormat), Error> {
    let rate = rate.or(default.map(|d| d.sample_rate()));
    let channels = channels.or(default.map(|d| d.channels()));
    let def_fmt = default.map(|d| d.sample_format());
    let best = configs
        .iter()
        .filter(|c| channels.is_none_or(|n| c.channels() == n) && rate.is_none_or(|r| c.contains_rate(r)))
        .max_by_key(|c| (c.sample_format() == SampleFormat::F32, Some(c.sample_format()) == def_fmt));
    let (ch, rate, fmt, range) = match (best, default) {
        (Some(c), _) => (c.channels(), rate.unwrap_or(c.max_sample_rate()), c.sample_format(), *c.buffer_size()),
        (None, Some(d)) if configs.is_empty() => {
            (channels.unwrap_or(d.channels()), rate.unwrap_or(d.sample_rate()), d.sample_format(), *d.buffer_size())
        }
        _ => {
            return Err(Error::with_message(
                ErrorKind::UnsupportedConfig,
                format!("no configuration with {channels:?} channels at {rate:?} Hz"),
            ));
        }
    };
    let buffer_size = match (buffer, range) {
        (Some(b), SupportedBufferSize::Range { min, max }) => BufferSize::Fixed(b.clamp(min, max)),
        _ => BufferSize::Default,
    };
    Ok((StreamConfig { channels: ch, sample_rate: rate, buffer_size }, fmt))
}

fn resolve(
    device: Device,
    input: bool,
    rate: Option<u32>,
    channels: Option<u16>,
    buffer: Option<u32>,
    rate_required: bool,
) -> Result<Resolved, Error> {
    let (configs, default): (Vec<_>, _) = if input {
        let c = device.supported_input_configs().map(Iterator::collect).unwrap_or_default();
        (c, device.default_input_config().ok())
    } else {
        let c = device.supported_output_configs().map(Iterator::collect).unwrap_or_default();
        (c, device.default_output_config().ok())
    };
    let (config, format) = match pick(&configs, default, rate, channels, buffer) {
        Err(_) if !rate_required && rate.is_some() => pick(&configs, default, None, channels, buffer)?,
        r => r?,
    };
    Ok(Resolved { device, config, format })
}

fn resolve_input(
    host: &Host,
    ic: &InputConfig,
    rate: Option<u32>,
    buffer: Option<u32>,
) -> Result<Option<Resolved>, Error> {
    let r = match &ic.source {
        InputSource::External { .. } => return Ok(None),
        InputSource::App(app) => {
            if !app_capture_supported() {
                return Err(Error::with_message(
                    ErrorKind::UnsupportedOperation,
                    "per-application capture needs PipeWire",
                ));
            }
            let dev = find_app_stream(app).ok_or_else(|| {
                Error::with_message(ErrorKind::DeviceNotAvailable, format!("application {app} is not playing"))
            })?;
            // Its output side tells the rate; PipeWire converts channels for the capture stream,
            // so the requested count stands even when the application plays mono or 5.1.
            let mut r = resolve(dev, false, rate, None, buffer, false)?;
            r.config.channels = ic.channels.unwrap_or(r.config.channels);
            r
        }
        InputSource::Device(id) => resolve(find(host, id)?, true, rate, ic.channels, buffer, false)?,
        InputSource::Loopback(id) => match host.id().name() {
            "PulseAudio" => {
                let monitor = DeviceId::new(id.host(), format!("{}.monitor", id.id()));
                resolve(find(host, &monitor)?, true, rate, ic.channels, buffer, false)?
            }
            // These hosts open the output device itself for capture; its output side tells
            // the format (WASAPI and CoreAudio list no input configs for it).
            "WASAPI" | "CoreAudio" | "PipeWire" => {
                resolve(find(host, id)?, false, rate, ic.channels, buffer, false)?
            }
            other => {
                return Err(Error::with_message(
                    ErrorKind::UnsupportedOperation,
                    format!("{other} cannot capture system sound"),
                ));
            }
        },
    };
    Ok(Some(r))
}

macro_rules! by_format {
    ($fmt:expr, $func:ident ( $($arg:expr),* )) => {
        match $fmt {
            SampleFormat::F32 => $func::<f32, F>($($arg),*),
            SampleFormat::F64 => $func::<f64, F>($($arg),*),
            SampleFormat::I8 => $func::<i8, F>($($arg),*),
            SampleFormat::I16 => $func::<i16, F>($($arg),*),
            SampleFormat::I24 => $func::<cpal::I24, F>($($arg),*),
            SampleFormat::I32 => $func::<i32, F>($($arg),*),
            SampleFormat::U8 => $func::<u8, F>($($arg),*),
            SampleFormat::U16 => $func::<u16, F>($($arg),*),
            SampleFormat::U32 => $func::<u32, F>($($arg),*),
            other => Err(Error::with_message(
                ErrorKind::UnsupportedConfig,
                format!("sample format {other} is not supported"),
            )),
        }
    };
}

fn on_error(
    role: StreamRole,
    tx: Sender<StreamEvent>,
    xruns: Arc<AtomicU64>,
) -> impl FnMut(Error) + Send + 'static {
    move |error| {
        if error.kind() == ErrorKind::Xrun {
            xruns.fetch_add(1, Relaxed);
        } else {
            let _ = tx.send(StreamEvent { role, error });
        }
    }
}

fn build_input<T, F>(
    r: &Resolved,
    mut producer: ExternalInput,
    mut driver: Option<Holder<F>>,
    count: Arc<AtomicU64>,
    err: impl FnMut(Error) + Send + 'static,
) -> Result<cpal::Stream, Error>
where
    T: SizedSample,
    f32: FromSample<T>,
    F: FnMut(&mut AudioBlock<'_>) + Send + 'static,
{
    let ch = r.config.channels as usize;
    r.device.build_input_stream::<T, _, _>(
        r.config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            count.fetch_add(1, Relaxed);
            producer.push(data);
            if let Some(d) = driver.as_mut().and_then(|h| h.duplex.as_mut()) {
                d.advance(data.len() / ch);
            }
        },
        err,
        None,
    )
}

fn build_output<T, F>(
    r: &Resolved,
    mut holder: Holder<F>,
    count: Arc<AtomicU64>,
    err: impl FnMut(Error) + Send + 'static,
) -> Result<cpal::Stream, Error>
where
    T: SizedSample + FromSample<f32>,
    F: FnMut(&mut AudioBlock<'_>) + Send + 'static,
{
    r.device.build_output_stream::<T, _, _>(
        r.config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            count.fetch_add(1, Relaxed);
            if let Some(d) = holder.duplex.as_mut() {
                d.render(data);
            }
        },
        err,
        None,
    )
}

impl<F: FnMut(&mut AudioBlock<'_>) + Send + 'static> Session<F> {
    /// Opens and starts every configured device stream around `engine`.
    pub fn open(config: SessionConfig, engine: F) -> Result<Self, Error> {
        let host = cpal::host_from_id(config.host)?;
        let out = Self::resolve_output(&host, &config, config.sample_rate)?;
        let mut rate = config.sample_rate.or(out.as_ref().map(|o| o.config.sample_rate));
        let mut ins = Vec::new();
        for ic in &config.inputs {
            let r = resolve_input(&host, ic, rate, config.buffer_frames)?;
            if out.is_none() && rate.is_none() {
                rate = r.as_ref().map(|r| r.config.sample_rate);
            }
            ins.push(r);
        }
        let Some(driver_rate) = out.as_ref().map(|o| o.config.sample_rate).or(rate) else {
            return Err(Error::with_message(
                ErrorKind::InvalidInput,
                "nothing drives the engine: configure an output device or a device input",
            ));
        };
        Self::check_driver(&out, &ins, driver_rate)?;
        let out_ch = out
            .as_ref()
            .map(|o| o.config.channels)
            .or(config.output_channels)
            .unwrap_or(2) as usize;
        let total_in: usize = ins
            .iter()
            .zip(&config.inputs)
            .map(|(r, ic)| match (r, &ic.source) {
                (Some(r), _) => r.config.channels as usize,
                (None, InputSource::External { channels, .. }) => *channels as usize,
                _ => 0,
            })
            .sum();
        if config.block_size == 0 || out_ch > crate::MAX_CHANNELS || total_in > crate::MAX_CHANNELS {
            return Err(Error::with_message(ErrorKind::InvalidInput, "bad block size or too many channels"));
        }
        let mut duplex = Duplex::new(engine, driver_rate, config.block_size, out_ch);
        let mut producers = Vec::new();
        let mut externals = Vec::new();
        let mut monitors = Vec::new();
        let mut input_channels = Vec::new();
        for (r, ic) in ins.iter().zip(&config.inputs) {
            let (ch, rate) = match (r, &ic.source) {
                (Some(r), _) => (r.config.channels, r.config.sample_rate),
                (None, InputSource::External { channels, sample_rate }) => (*channels, *sample_rate),
                _ => unreachable!("resolve_input yields None only for External"),
            };
            let p = duplex.add_input(ch as usize, rate, default_margin(rate));
            monitors.push(p.monitor());
            input_channels.push(ch);
            if r.is_some() {
                producers.push(Some(p));
                externals.push(None);
            } else {
                producers.push(None);
                externals.push(Some(p));
            }
        }
        let (tx, rx) = channel();
        let mut s = Session {
            config,
            sample_rate: driver_rate,
            streams: Vec::new(),
            slot: Arc::new(Mutex::new(None)),
            monitors,
            input_channels,
            externals,
            xruns: Arc::new(AtomicU64::new(0)),
            tx,
            rx,
        };
        s.connect(out, ins, duplex, producers)?;
        Ok(s)
    }

    fn resolve_output(host: &Host, config: &SessionConfig, rate: Option<u32>) -> Result<Option<Resolved>, Error> {
        config
            .output
            .as_ref()
            .map(|id| resolve(find(host, id)?, false, rate, config.output_channels, config.buffer_frames, true))
            .transpose()
    }

    /// In input-only mode the driving input must run at the engine rate.
    fn check_driver(out: &Option<Resolved>, ins: &[Option<Resolved>], rate: u32) -> Result<(), Error> {
        match (out, ins.iter().flatten().next()) {
            (None, Some(d)) if d.config.sample_rate != rate => Err(Error::with_message(
                ErrorKind::UnsupportedConfig,
                format!("driving input cannot run at the engine rate {rate} Hz"),
            )),
            _ => Ok(()),
        }
    }

    /// Builds and starts the streams. On failure the engine is back in `slot`.
    fn connect(
        &mut self,
        out: Option<Resolved>,
        ins: Vec<Option<Resolved>>,
        duplex: Duplex<F>,
        producers: Vec<Option<ExternalInput>>,
    ) -> Result<(), Error> {
        let mut holder = Some(Holder { duplex: Some(duplex), slot: self.slot.clone() });
        let res = (|| {
            if let Some(o) = &out {
                let count = Arc::new(AtomicU64::new(0));
                let err = on_error(StreamRole::Output, self.tx.clone(), self.xruns.clone());
                let h = holder.take().expect("holder");
                let stream = by_format!(o.format, build_output(o, h, count.clone(), err))?;
                self.streams.push((StreamRole::Output, stream, count, 0));
            }
            for (i, (r, p)) in ins.iter().zip(producers).enumerate() {
                let (Some(r), Some(p)) = (r, p) else { continue };
                let count = Arc::new(AtomicU64::new(0));
                let err = on_error(StreamRole::Input(i), self.tx.clone(), self.xruns.clone());
                let h = holder.take();
                let stream = by_format!(r.format, build_input(r, p, h, count.clone(), err))?;
                self.streams.push((StreamRole::Input(i), stream, count, 0));
            }
            for (_, s, _, _) in &self.streams {
                s.play()?;
            }
            Ok(())
        })();
        if res.is_err() {
            self.streams.clear();
        }
        drop(holder);
        res
    }

    /// Takes the engine back after all streams were dropped.
    fn take_duplex(&self) -> Result<Duplex<F>, Error> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(d) = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Ok(d);
            }
            if Instant::now() > deadline {
                return Err(Error::with_message(ErrorKind::Other, "backend did not release the engine"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn block_size(&self) -> usize {
        self.config.block_size
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Health of every input, in configuration order.
    pub fn input_stats(&self) -> Vec<crate::InputStats> {
        self.monitors.iter().map(InputMonitor::stats).collect()
    }

    /// Channels each input actually opened (the device default when not requested), in
    /// configuration order.
    pub fn input_channels(&self) -> &[u16] {
        &self.input_channels
    }

    /// The stream whose callback runs the engine: the output, else the first device input.
    pub fn driver(&self) -> Option<StreamRole> {
        self.streams.first().map(|s| s.0)
    }

    /// Stops input `index`'s stream (e.g. a captured application that went away); that input
    /// delivers silence from then on, the other streams run on. Refused (false) for the
    /// [`driver`](Self::driver): reopen instead.
    pub fn close_input(&mut self, index: usize) -> bool {
        let role = StreamRole::Input(index);
        if self.driver() == Some(role) {
            return false;
        }
        self.streams.retain(|s| s.0 != role);
        true
    }

    /// Xruns reported by any stream of this session.
    pub fn xruns(&self) -> u64 {
        self.xruns.load(Relaxed)
    }

    /// The producer for an [`InputSource::External`] input, once.
    pub fn take_external_input(&mut self, index: usize) -> Option<ExternalInput> {
        self.externals.get_mut(index)?.take()
    }

    /// Stream errors since the last call (device gone, stream invalidated, rerouted, ...).
    pub fn events(&self) -> impl Iterator<Item = StreamEvent> + '_ {
        self.rx.try_iter()
    }

    /// Streams whose callback has not run since the previous call. Call periodically (with
    /// a period well above the device buffer duration) to detect a stream that stopped.
    pub fn stalled(&mut self) -> Vec<StreamRole> {
        let mut out = Vec::new();
        for (role, _, count, last) in &mut self.streams {
            let now = count.load(Relaxed);
            if now == *last {
                out.push(*role);
            }
            *last = now;
        }
        out
    }

    /// Closes and reopens every device stream with the same configuration, keeping the engine,
    /// the engine rate, block size and external inputs. Use after a stream error, a stall or a
    /// device reappearing. On failure the session holds no streams and can be reopened again.
    // ponytail: reopens all streams (one glitch everywhere); a per-stream reopen needs a
    // command queue to swap one input's ring inside the running callback.
    pub fn reopen(&mut self) -> Result<(), Error> {
        self.streams.clear();
        let mut duplex = self.take_duplex()?;
        let res = (|| {
            let host = cpal::host_from_id(self.config.host)?;
            let out = Self::resolve_output(&host, &self.config, Some(self.sample_rate))?;
            let mut ins = Vec::new();
            for ic in &self.config.inputs {
                ins.push(resolve_input(&host, ic, Some(self.sample_rate), self.config.buffer_frames)?);
            }
            Self::check_driver(&out, &ins, self.sample_rate)?;
            Ok((out, ins))
        })();
        let (out, ins) = match res {
            Ok(v) => v,
            Err(e) => {
                *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(duplex);
                return Err(e);
            }
        };
        let mut producers = Vec::new();
        for (i, r) in ins.iter().enumerate() {
            producers.push(r.as_ref().map(|r| {
                let rate = r.config.sample_rate;
                let p = duplex.reset_input(i, rate, default_margin(rate));
                self.monitors[i] = p.monitor();
                p
            }));
        }
        self.connect(out, ins, duplex, producers)
    }

    /// Stops every stream and returns the engine.
    pub fn close(mut self) -> Result<F, Error> {
        self.streams.clear();
        Ok(self.take_duplex()?.into_engine())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_reports_added_and_removed_once() {
        let h = cpal::default_host().id();
        let id = |s: &str| DeviceId::new(h, s);
        let mut known: HashSet<_> = [id("a"), id("b")].into();
        let change = diff(&mut known, [id("b"), id("c")].into()).unwrap();
        assert_eq!(change, DeviceChange { added: vec![id("c")], removed: vec![id("a")] });
        assert_eq!(diff(&mut known, [id("b"), id("c")].into()), None);
    }

    #[test]
    fn app_list_drops_own_streams_and_duplicates() {
        let nodes = [
            ("Firefox", "unknown"),
            ("sonobus", "unknown"),
            ("cpal-playback-42", ""),
            ("mpv", "mpv"),
            ("Firefox", "Firefox"),
            ("spotify", "Spotify Premium"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()));
        let got = app_list(nodes.into_iter(), &["sonobus".to_owned()]);
        let ids: Vec<_> = got.iter().map(|a| (a.id.as_str(), a.name.as_str())).collect();
        assert_eq!(ids, [("Firefox", "Firefox"), ("mpv", "mpv"), ("spotify", "spotify: Spotify Premium")]);
        // whatever this binary is called, its own streams are excluded
        assert!(!own_names().is_empty());
        let own = own_names();
        let exe = own.iter().find(|n| !n.starts_with("alsa_")).unwrap().clone();
        let ours = [exe.clone(), format!("alsa_playback.{exe}"), format!("alsa_capture.{exe}")];
        assert!(app_list(ours.map(|n| (n, String::new())).into_iter(), &own).is_empty());
    }

    #[test]
    fn default_host_is_not_pipewire() {
        // "System default" keeps meaning what it did before the pipewire feature existed
        #[cfg(all(target_os = "linux", not(feature = "pulseaudio")))]
        assert_eq!(default_host(), HostId::Alsa);
        assert_ne!(default_host().name(), "PipeWire");
    }

    #[test]
    fn app_input_without_pipewire_is_unsupported() {
        if app_capture_supported() {
            return; // a PipeWire daemon is running: covered by the manual test
        }
        assert!(app_sources().is_empty());
        let host = cpal::default_host();
        let ic = InputConfig { source: InputSource::App("Firefox".into()), channels: Some(2) };
        let err = resolve_input(&host, &ic, None, None).err().expect("no PipeWire: error");
        assert_eq!(err.kind(), ErrorKind::UnsupportedOperation);
    }

    #[test]
    fn pick_prefers_f32_and_requested_rate_and_clamps_buffer() {
        let range = |ch, fmt| SupportedStreamConfigRange::new(ch, 44100, 48000, SupportedBufferSize::Range { min: 64, max: 1024 }, fmt);
        let cfgs = [range(2, SampleFormat::I16), range(2, SampleFormat::F32), range(1, SampleFormat::F32)];
        let (c, f) = pick(&cfgs, None, Some(48000), Some(2), Some(32)).unwrap();
        assert_eq!((c.channels, c.sample_rate, c.buffer_size, f), (2, 48000, BufferSize::Fixed(64), SampleFormat::F32));
        assert!(pick(&cfgs, None, Some(96000), Some(2), None).is_err());
        // A backend listing nothing falls back to its default config.
        let def = range(2, SampleFormat::I16).with_sample_rate(44100);
        let (c, f) = pick(&[], Some(def), None, None, None).unwrap();
        assert_eq!((c.channels, c.sample_rate, f), (2, 44100, SampleFormat::I16));
    }
}
