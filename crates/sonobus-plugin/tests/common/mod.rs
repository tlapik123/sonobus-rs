//! A minimal host: drives the plugin through nice-plug's `Plugin` trait (activate + process with
//! real `Buffer`s and `AuxiliaryBuffers`, a transport) without any plugin format wrapper.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nice_plug::prelude::*;
use sonobus_engine::Connection;
use sonobus_plugin::session::Connect;
use sonobus_plugin::{AUX_BUSES, SonoBus};

pub const SR: f32 = 48_000.0;
pub const BLOCK: usize = 256;
/// Output kept for tone analysis: half a second.
const KEEP: usize = 24_000;

pub struct Ctx {
    pub transport: Transport,
}

impl ProcessContext<SonoBus> for Ctx {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }
    fn execute_background(&self, _: ()) {}
    fn execute_gui(&self, _: ()) {}
    fn transport(&self) -> &Transport {
        &self.transport
    }
    fn next_event(&mut self) -> Option<PluginNoteEvent<SonoBus>> {
        None
    }
    fn try_send_event(
        &mut self,
        _: PluginNoteEvent<SonoBus>,
    ) -> Result<(), (PluginNoteEvent<SonoBus>, nice_plug::context::process::SendEventError)> {
        Ok(())
    }
    fn set_latency_samples(&self, _: u32) {}
    fn request_restart(&self) {}
    fn set_current_voice_capacity(&self, _: u32) {}
}

impl ActivateContext<SonoBus> for Ctx {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }
    fn execute(&self, _: ()) {}
    fn set_latency_samples(&self, _: u32) {}
    fn set_current_voice_capacity(&self, _: u32) {}
}

/// A unique scratch directory under cargo's per-test tmp dir.
pub fn tmp_dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub struct Host {
    pub plugin: SonoBus,
    pub ctx: Ctx,
    pub sr: f32,
    pub freq: f64,
    phase: f64,
    /// Main in/out (in place), 16 aux input and 16 aux output channels.
    pub main: Vec<Vec<f32>>,
    pub aux_in: Vec<Vec<f32>>,
    pub aux_out: Vec<Vec<f32>>,
    /// Last half second of main out (0, 1) and aux out (2..18).
    pub out: Vec<VecDeque<f32>>,
    /// Frames processed since the transport was last reset.
    pub frames: u64,
    /// Callback size `run_until` uses.
    pub block_len: usize,
}

impl Host {
    pub fn new(freq: f64) -> Self {
        Self::with_dir(freq, None)
    }

    pub fn with_dir(freq: f64, dir: Option<PathBuf>) -> Self {
        Host {
            plugin: SonoBus::with_defaults_dir(dir),
            ctx: Ctx { transport: Transport::new(SR) },
            sr: SR,
            freq,
            phase: 0.0,
            main: vec![vec![0.0; 8192]; 2],
            aux_in: vec![vec![0.0; 8192]; 2 * AUX_BUSES],
            aux_out: vec![vec![0.0; 8192]; 2 * AUX_BUSES],
            out: vec![VecDeque::new(); 2 + 2 * AUX_BUSES],
            frames: 0,
            block_len: BLOCK,
        }
    }

    /// Activates with layout `layout` of `AUDIO_IO_LAYOUTS` (0: stereo + aux).
    pub fn activate(&mut self, sr: f32, max_block: u32, layout: usize) -> bool {
        self.sr = sr;
        self.ctx.transport = Transport::new(sr);
        let cfg = BufferConfig {
            sample_rate: sr,
            min_buffer_size: None,
            max_buffer_size: max_block,
            process_mode: ProcessMode::Realtime,
        };
        let l = SonoBus::AUDIO_IO_LAYOUTS[layout];
        self.plugin.activate(&l, &cfg, &mut self.ctx)
    }

    /// Fills the main input with the sine (both channels).
    pub fn fill(&mut self, n: usize) {
        for i in 0..n {
            let v = 0.25 * (self.phase * std::f64::consts::TAU).sin() as f32;
            self.phase = (self.phase + self.freq / f64::from(self.sr)).fract();
            self.main[0][i] = v;
            self.main[1][i] = v;
        }
    }

    /// One host callback of `n` frames; `armed` wraps exactly the `process` call.
    pub fn process(&mut self, n: usize, armed: &mut dyn FnMut(&mut dyn FnMut())) {
        let mut buffer = Buffer::default();
        let (main, aux_in, aux_out) = (&mut self.main, &mut self.aux_in, &mut self.aux_out);
        // SAFETY: every slice is `n` long and outlives the process call below
        unsafe { buffer.set_slices(n, |s| s.extend(main.iter_mut().map(|c| &mut c[..n]))) };
        let mut ins = bus(aux_in, n);
        let mut outs = bus(aux_out, n);
        let mut aux = AuxiliaryBuffers { inputs: &mut ins, outputs: &mut outs };
        let (plugin, ctx) = (&mut self.plugin, &mut self.ctx);
        armed(&mut || {
            plugin.process(&mut buffer, &mut aux, ctx);
        });
    }

    /// One block: sine in, process, keep the output, advance the transport.
    pub fn block(&mut self, n: usize) {
        self.fill(n);
        self.process(n, &mut |f| f());
        let chans = self.main.iter().chain(self.aux_out.iter());
        for (ring, c) in self.out.iter_mut().zip(chans) {
            for &x in &c[..n] {
                if ring.len() == KEEP {
                    ring.pop_front();
                }
                ring.push_back(x);
            }
        }
        self.advance(n);
    }

    pub fn advance(&mut self, n: usize) {
        self.frames += n as u64;
        let t = &mut self.ctx.transport;
        if t.playing
            && let (Some(b), Some(bpm)) = (t.pos_beats.as_mut(), t.tempo)
        {
            *b += bpm / 60.0 * n as f64 / f64::from(self.sr);
        }
    }

    /// Amplitude of a sine at `freq` in the kept output channel `ch` (Hann-windowed DFT bin).
    pub fn tone(&self, ch: usize, freq: f64) -> f32 {
        let x = &self.out[ch];
        let n = x.len().max(1);
        let (mut re, mut im, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
        for (i, &v) in x.iter().enumerate() {
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
            let a = std::f64::consts::TAU * freq * i as f64 / f64::from(self.sr);
            re += w * f64::from(v) * a.cos();
            im -= w * f64::from(v) * a.sin();
            wsum += w;
        }
        (2.0 * (re * re + im * im).sqrt() / wsum.max(1e-9)) as f32
    }

    pub fn port(&self) -> u16 {
        self.plugin.session().engine().expect("active").local_port()
    }

    pub fn join(&self, server: SocketAddr, user: &str, group: &str) {
        self.plugin.session().connect(Connect {
            host: "127.0.0.1".into(),
            port: server.port(),
            user: user.into(),
            user_password: String::new(),
            group: group.into(),
            group_password: "pw".into(),
            public: false,
        });
    }

    /// Peers we receive audio from.
    pub fn receiving(&self) -> usize {
        let Some(e) = self.plugin.session().engine() else { return 0 };
        let s = e.snapshot();
        if s.connection != Connection::Connected {
            return 0;
        }
        s.peers.iter().filter(|p| p.recv_active).count()
    }
}

/// Stereo `Buffer`s over `chans`, `n` frames each.
fn bus(chans: &mut [Vec<f32>], n: usize) -> Vec<Buffer<'_>> {
    chans
        .chunks_mut(2)
        .map(|pair| {
            let mut b = Buffer::default();
            // SAFETY: every slice is `n` long and outlives the process call
            unsafe { b.set_slices(n, |s| s.extend(pair.iter_mut().map(|c| &mut c[..n]))) };
            b
        })
        .collect()
}

/// Runs every host's audio in lockstep at real-time pace until `done` (checked every block);
/// false after `secs`.
pub fn run_until(hosts: &mut [&mut Host], secs: f64, done: impl Fn(&[&mut Host]) -> bool) -> bool {
    let start = Instant::now();
    let block = Duration::from_secs_f64(BLOCK as f64 / f64::from(SR));
    let mut next = start;
    while start.elapsed().as_secs_f64() < secs {
        for h in hosts.iter_mut() {
            let n = h.block_len;
            h.block(n);
        }
        if done(hosts) {
            return true;
        }
        next += block;
        match next.checked_duration_since(Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = Instant::now(),
        }
    }
    false
}

pub fn run(hosts: &mut [&mut Host], secs: f64) {
    run_until(hosts, secs, |_| false);
}

/// Network tests share the machine's real-time budget: one at a time.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
