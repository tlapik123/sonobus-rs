//! SonoBus as a CLAP and VST3 plugin (nice-plug): a DAW track sends its input to a SonoBus group
//! and plays the group back, as upstream's JUCE plugin does.
//!
//! - [`SonoBus`]: the plugin. Each instance owns a [`session::Session`] (the app's controller in
//!   hosted mode, its own engine, UDP socket and threads; nothing is global). The host's audio
//!   thread only calls [`sonobus_engine::EngineAudio`]: no allocation, lock or log there
//!   (`tests/rt_alloc.rs`).
//! - [`editor`]: the app's full UI with the plugin's capabilities.
//! - [`params::SbParams`]: upstream's automatable parameter list, ids/ranges/skews 1:1.
//! - Buses as upstream's develop branch (P.cpp:547-595): stereo "Main In"/"Mix Out" plus eight
//!   stereo "Aux N In" (extra input channels for input groups) and eight stereo "Aux N Out"
//!   (each peer's received audio, peer slot N-1).
//! - The plugin never opens audio devices; the host drives the engine.

pub mod editor;
pub mod params;
pub mod session;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use nice_plug::prelude::*;
use sonobus_engine::{AudioConfig, EngineAudio, HostTransport};

use params::SbParams;
use session::{Session, StateField};

/// Aux buses each way (upstream: eight stereo, P.cpp:566-590).
pub const AUX_BUSES: usize = 8;
const STEREO: NonZeroU32 = new_nonzero_u32(2);
const MONO: NonZeroU32 = new_nonzero_u32(1);
/// Input channels the largest layout has: main + aux.
const MAX_INPUTS: usize = 2 + 2 * AUX_BUSES;
const MAX_PEER_OUTPUTS: usize = 2 * AUX_BUSES;
/// How often host automation is forwarded to the engine (and the controller ticked while the
/// editor is closed).
const PARAM_POLL: Duration = Duration::from_millis(10);

const AUX_IN_NAMES: [&str; AUX_BUSES] = [
    "Aux 1 In", "Aux 2 In", "Aux 3 In", "Aux 4 In", "Aux 5 In", "Aux 6 In", "Aux 7 In", "Aux 8 In",
];
const AUX_OUT_NAMES: [&str; AUX_BUSES] = [
    "Aux 1 Out",
    "Aux 2 Out",
    "Aux 3 Out",
    "Aux 4 Out",
    "Aux 5 Out",
    "Aux 6 Out",
    "Aux 7 Out",
    "Aux 8 Out",
];

const NAMES: PortNames = PortNames {
    layout: None,
    main_input: Some("Main In"),
    main_output: Some("Mix Out"),
    aux_inputs: &AUX_IN_NAMES,
    aux_outputs: &AUX_OUT_NAMES,
};

pub struct SonoBus {
    params: Arc<SbParams>,
    session: Arc<Session>,
    rt: Option<Rt>,
    sync: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

/// What `process` uses, set up by `activate`.
struct Rt {
    audio: EngineAudio,
    cfg: AudioConfig,
    peer_outputs: usize,
    main_inputs: usize,
    /// Copies of the host input (main, then aux): the main buffer is processed in place.
    inputs: Vec<Vec<f32>>,
}

impl SonoBus {
    /// An instance whose `PluginDefault.xml` comes from `defaults_dir` (`None`: none).
    pub fn with_defaults_dir(defaults_dir: Option<std::path::PathBuf>) -> Self {
        let session = Arc::new(Session::new(defaults_dir));
        let params = Arc::new(SbParams::new(StateField(session.clone())));
        params.set_from_core(&session.state().params);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = std::thread::Builder::new()
            .name("sonobus-params".into())
            .spawn({
                let (params, session, stop) = (params.clone(), session.clone(), stop.clone());
                let params = Arc::downgrade(&params);
                move || sync_params(&params, &session, &stop)
            })
            .expect("spawn parameter thread");
        SonoBus {
            params,
            session,
            rt: None,
            sync: Some((stop, handle)),
        }
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub fn sb_params(&self) -> &Arc<SbParams> {
        &self.params
    }
}

/// Forwards parameter changes (host automation, the editor) to the engine. Upstream applies
/// them in `parameterChanged` on the thread that set them; here a 10 ms poll keeps every
/// engine call off the audio thread. Also ticks the controller while no editor frame does.
/// Holds the parameters weakly: they own the session (no cycle keeping the instance alive).
fn sync_params(params: &std::sync::Weak<SbParams>, session: &Session, stop: &AtomicBool) {
    let mut last = None;
    while !stop.load(Ordering::Acquire) {
        let Some(params) = params.upgrade() else {
            return;
        };
        if session.take_resync() {
            // after a state load: metronome and monitor solo start off (P.cpp:8778-8786)
            let mut p = params.to_core();
            (p.met_enabled, p.main_monitor_solo) = (false, false);
            params.set_from_core(&p);
            last = None;
        }
        let p = params.to_core();
        if last.as_ref() != Some(&p)
            && let Some(e) = session.engine()
        {
            let q = p.clone();
            e.update_state(move |s| s.params = q);
            last = Some(p);
        }
        drop(params);
        session.idle_tick();
        std::thread::park_timeout(PARAM_POLL);
    }
}

impl Default for SonoBus {
    fn default() -> Self {
        Self::with_defaults_dir(session::settings_dir())
    }
}

impl Drop for SonoBus {
    fn drop(&mut self) {
        if let Some((stop, h)) = self.sync.take() {
            stop.store(true, Ordering::Release);
            h.thread().unpark();
            let _ = h.join();
        }
    }
}

impl Plugin for SonoBus {
    const NAME: &'static str = "SonoBus RS";
    const VENDOR: &'static str = "sonobus-rs";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: Some(STEREO),
            main_output_channels: Some(STEREO),
            aux_input_ports: &[STEREO; AUX_BUSES],
            aux_output_ports: &[STEREO; AUX_BUSES],
            names: NAMES,
        },
        AudioIOLayout {
            main_input_channels: Some(STEREO),
            main_output_channels: Some(STEREO),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: NAMES,
        },
        AudioIOLayout {
            main_input_channels: Some(MONO),
            main_output_channels: Some(STEREO),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: NAMES,
        },
        AudioIOLayout {
            main_input_channels: Some(MONO),
            main_output_channels: Some(MONO),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: NAMES,
        },
    ];

    type SysExMessage = ();
    type BackgroundTask = ();
    type Editor = nice_plug_egui::EguiEditor<editor::SbEditor>;

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _executor: AsyncExecutor<Self>) -> Option<Self::Editor> {
        editor::create(self.session.clone())
    }

    fn activate(
        &mut self,
        layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl ActivateContext<Self>,
    ) -> bool {
        let ch = |c: Option<NonZeroU32>| c.map_or(0, |c| c.get() as usize);
        let main_inputs = ch(layout.main_input_channels);
        let aux_inputs: usize = layout
            .aux_input_ports
            .iter()
            .map(|c| c.get() as usize)
            .sum();
        let peer_outputs = layout.aux_output_ports.len();
        let cfg = AudioConfig {
            sample_rate: buffer_config.sample_rate.round() as u32,
            block_size: buffer_config.max_buffer_size.max(1) as usize,
            input_channels: (main_inputs + aux_inputs).min(MAX_INPUTS),
            output_channels: ch(layout.main_output_channels),
        };
        // hosts may activate again with the same configuration (e.g. after a state load)
        if self
            .rt
            .as_ref()
            .is_some_and(|r| r.cfg == cfg && r.peer_outputs == peer_outputs)
        {
            return true;
        }
        self.rt = None;
        match self.session.start(cfg, main_inputs, &self.params) {
            Ok(mut audio) => {
                audio.set_peer_outputs(peer_outputs, 2);
                let inputs = vec![vec![0.0; cfg.block_size]; cfg.input_channels];
                self.rt = Some(Rt {
                    audio,
                    cfg,
                    peer_outputs,
                    main_inputs,
                    inputs,
                });
                true
            }
            Err(e) => {
                nice_log!("sonobus: could not start the engine: {e}");
                false
            }
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let Some(rt) = &mut self.rt else {
            return ProcessStatus::Normal;
        };
        if self.params.bypass.value() {
            // JUCE's default `processBlockBypassed`: input through, the extra buses silent
            for b in aux.outputs.iter_mut() {
                for c in b.as_slice() {
                    c.fill(0.0);
                }
            }
            return ProcessStatus::Normal;
        }
        let t = context.transport();
        rt.audio.set_host_transport(Some(HostTransport {
            bpm: t.tempo.unwrap_or(0.0),
            ppq: t.pos_beats(),
            playing: t.playing,
        }));
        // hosts never exceed the activated maximum; clamp rather than trust it
        let n = buffer.samples().min(rt.cfg.block_size);
        let main = buffer.as_slice();
        let sources = main[..rt.main_inputs.min(main.len())]
            .iter()
            .map(|c| &**c)
            .chain(
                aux.inputs
                    .iter_mut()
                    .flat_map(|b| b.as_slice().iter().map(|c| &**c)),
            );
        for (dst, src) in rt.inputs.iter_mut().zip(sources) {
            dst[..n].copy_from_slice(&src[..n]);
        }
        let mut it = rt.inputs.iter().map(|c| &c[..n]);
        let ins: [&[f32]; MAX_INPUTS] = std::array::from_fn(|_| it.next().unwrap_or(&[]));
        let mut it = main.iter_mut().map(|c| &mut c[..n]);
        let mut outs: [&mut [f32]; 2] = std::array::from_fn(|_| it.next().unwrap_or(&mut []));
        let n_out = rt.cfg.output_channels.min(2);
        let mut it = aux
            .outputs
            .iter_mut()
            .flat_map(|b| b.as_slice().iter_mut().map(|c| &mut c[..n]));
        let mut peers: [&mut [f32]; MAX_PEER_OUTPUTS] =
            std::array::from_fn(|_| it.next().unwrap_or(&mut []));
        let np = (2 * rt.peer_outputs).min(MAX_PEER_OUTPUTS);
        rt.audio.process_with_peer_outputs(
            &ins[..rt.inputs.len()],
            &mut outs[..n_out],
            &mut peers[..np],
        );
        ProcessStatus::Normal
    }
}

impl ClapPlugin for SonoBus {
    // Upstream ships no CLAP build; this id is new.
    const CLAP_ID: &'static str = "rs.sonobus.sonobus";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("SonoBus network audio (Rust port)");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Utility,
    ];
}

impl Vst3Plugin for SonoBus {
    // New id. Upstream's JUCE plugin (manufacturer code `Sono`, plugin code `NBus`; the
    // instrument variant `IBus`) has JUCE's component UID
    // ABCDEF01-9182FAEB-536F6E6F-4E427573 ('Sono' 'NBus'); never reuse it, so both can be
    // installed side by side.
    const VST3_CLASS_ID: [u8; 16] =
        *b"\x2b\x1d\x5f\xd3\xc0\xe5\x54\xfb\x5b\x92\x4a\x2b\x69\xd9\xf6\x0d";
    // upstream: "Fx|Network"
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Network];
}

nice_export_clap!(SonoBus);
nice_export_vst3!(SonoBus);
