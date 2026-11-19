//! Compares the reverbs, monitor delay and metronome with reference vectors produced by the
//! upstream SonoBus 1.7.2 C++ (juce::Reverb, MVerb.h, zitaRev.h, juce::dsp::DelayLine,
//! Metronome.cpp), driven the way SonobusPluginProcessor.cpp / ChannelGroup.cpp drive them.
//! Generator: `.locki/tmp/dsp-ref/reverb_harness.cpp`. Data: little-endian f32, all left
//! samples then all right samples, decimated as noted per case. Block size 256.

use sonobus_dsp::{InputReverb, MainReverb, Metronome, MonitorDelay, ReverbModel};

const BS: usize = 256;
/// Absolute tolerance (relative above 1.0).
const TOL: f32 = 1e-5;

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        ((self.0 >> 8) as i32 - 8388608) as f32 / 8388608.0
    }
}

/// Impulse (L at 64, R at 81, -0.5), noise burst [4096, 8192) at 0.5 (or everywhere with
/// `noise_all`), silence after. Mirrors `make_input` in the harness.
fn input(n: usize, noise_all: bool) -> (Vec<f32>, Vec<f32>) {
    let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
    let (mut a, mut b) = (Lcg(12345), Lcg(987654321));
    for i in 0..n {
        let (na, nb) = (a.next(), b.next());
        if noise_all || (4096..8192).contains(&i) {
            l[i] = 0.5 * na;
            r[i] = 0.5 * nb;
        }
    }
    l[64] += 1.0;
    r[81] += -0.5;
    (l, r)
}

fn load(name: &str) -> Vec<f32> {
    let path = format!("{}/tests/data/{name}.f32", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// Checks `got` (already decimated, channels concatenated) against `want`.
fn compare(name: &str, got: &[f32], want: &[f32]) -> f32 {
    assert_eq!(got.len(), want.len(), "{name}: length");
    let mut max_err = 0.0f32;
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        let err = (g - w).abs();
        assert!(err <= TOL * w.abs().max(1.0), "{name}[{i}]: got {g}, want {w}");
        max_err = max_err.max(err);
    }
    eprintln!("{name}: max abs error {max_err:e}");
    max_err
}

fn decimate(l: &[f32], r: &[f32], step: usize) -> Vec<f32> {
    l.iter().step_by(step).chain(r.iter().step_by(step)).copied().collect()
}

const REV_N: usize = 65536;
const REV_DECIM: usize = 8;

fn run_main(sr: u32, model: ReverbModel, noise_all: bool, mut step: impl FnMut(usize, &mut MainReverb)) -> Vec<f32> {
    let (mut l, mut r) = input(REV_N, noise_all);
    let mut m = MainReverb::new(sr);
    m.set_model(model);
    m.set_enabled(true);
    for (b, (bl, br)) in l.chunks_mut(BS).zip(r.chunks_mut(BS)).enumerate() {
        step(b, &mut m);
        if !m.is_active() {
            bl.fill(0.0);
            br.fill(0.0);
        }
        if !m.process(bl, br) {
            bl.fill(0.0);
            br.fill(0.0);
        }
    }
    decimate(&l, &r, REV_DECIM)
}

#[test]
fn main_reverb_models() {
    for (sr, sfx) in [(48000, "48k"), (44100, "44k")] {
        for (model, name) in
            [(ReverbModel::Freeverb, "freeverb"), (ReverbModel::MVerb, "mverb"), (ReverbModel::Zita, "zita")]
        {
            let name = format!("rev_{name}_{sfx}");
            let got = run_main(sr, model, false, |_, _| {});
            compare(&name, &got, &load(&name));
        }
    }
}

#[test]
fn main_reverb_changes_switches_and_enable() {
    let got = run_main(48000, ReverbModel::MVerb, true, |b, m| match b {
        30 => {
            m.set_size(0.8);
            m.set_damping(0.2);
        }
        60 => {
            m.set_level(0.5);
            m.set_predelay_ms(70.0);
        }
        90 => m.set_model(ReverbModel::Zita),
        110 => {
            m.set_size(0.0);
            m.set_damping(1.0);
        }
        130 => {
            m.set_model(ReverbModel::Freeverb);
            m.set_size(0.3);
        }
        150 => m.set_level(0.0),
        155 => {
            m.set_level(0.8);
            m.set_damping(0.0);
        }
        170 => m.set_enabled(false),
        180 => {
            m.set_enabled(true);
            m.set_model(ReverbModel::Zita);
            m.set_level(0.0);
        }
        200 => {
            m.set_level(1.0);
            m.set_predelay_ms(0.0);
        }
        230 => {
            m.set_model(ReverbModel::MVerb);
            m.set_size(1.0);
        }
        _ => {}
    });
    // Deliberate fix: upstream does not fade the last block after disabling (block 170);
    // we fade it out with JUCE's ramp. Everything else must match upstream.
    let mut want = load("rev_switch_48k");
    let mut gains = [0.0f32; BS];
    let (mut g, step) = (1.0f32, -1.0 / BS as f32);
    for x in &mut gains {
        *x = g;
        g += step;
    }
    let per_ch = REV_N / REV_DECIM;
    for ch in 0..2 {
        for (k, gain) in gains.iter().step_by(REV_DECIM).enumerate() {
            want[ch * per_ch + (170 * BS) / REV_DECIM + k] *= gain;
        }
    }
    let silent_after = (171 * BS / REV_DECIM)..(180 * BS / REV_DECIM);
    assert!(want[silent_after].iter().all(|x| *x == 0.0), "tail cut while disabled");
    compare("rev_switch_48k", &got, &want);
}

#[test]
fn input_reverb() {
    let (mut l, mut r) = input(REV_N, true);
    let mut rv = InputReverb::new(48000);
    for (b, (bl, br)) in l.chunks_mut(BS).zip(r.chunks_mut(BS)).enumerate() {
        match b {
            0 | 104 => rv.set_enabled(true),
            100 | 200 => rv.set_enabled(false),
            40 => {
                rv.set_size(0.6);
                rv.set_level(0.7);
                rv.set_damping(0.1);
                rv.set_predelay_ms(50.0);
            }
            _ => {}
        }
        if !rv.process(bl, br) {
            bl.fill(0.0);
            br.fill(0.0);
        }
    }
    compare("inrev_48k", &decimate(&l, &r, REV_DECIM), &load("inrev_48k"));
}

#[test]
fn monitor_delay() {
    let (mut l, mut r) = input(32768, true);
    let mut d = MonitorDelay::new(48000, 2);
    for (b, (bl, br)) in l.chunks_mut(BS).zip(r.chunks_mut(BS)).enumerate() {
        match b {
            0 => d.set_delay_ms(10.0),
            2 => d.set_enabled(true),
            20 => d.set_delay_ms(3.3),
            40 => d.set_enabled(false),
            50 => d.set_delay_ms(100.0),
            60 => d.set_enabled(true),
            80 => d.set_delay_ms(100.01),
            90 => d.set_delay_ms(0.0),
            100 => d.set_delay_ms(1.0),
            101 => d.set_delay_ms(250.0),
            _ => {}
        }
        d.process(&mut [bl, br]);
    }
    let err = compare("mondelay_48k", &decimate(&l, &r, 2), &load("mondelay_48k"));
    assert_eq!(err, 0.0, "pure delay and gain ramps must be bit-exact");
}

/// Synthetic click (mirrors `click_sample` in the harness), as JUCE decodes 16-bit PCM.
fn click(len: i32, mul: i32) -> Vec<f32> {
    (0..len).map(|i| ((((i * mul) % 2000) - 1000) * (len - i) / (len / 16)) as i16 as f32 / 32768.0).collect()
}

fn run_met(name: &str, sr: u32, mut step: impl FnMut(usize, &mut Metronome) -> (f64, bool)) {
    let mut m = Metronome::new(sr, click(400, 104729), click(600, 7919));
    let mut out = vec![0.0f32; 32768];
    for (b, o) in out.chunks_mut(BS).enumerate() {
        let (beat_time, relative) = step(b, &mut m);
        m.render(o, beat_time, relative);
    }
    let err = compare(name, &out, &load(name));
    assert_eq!(err, 0.0, "{name}: scheduling must be sample-exact");
}

#[test]
fn metronome_free_running() {
    run_met("met_free_48k", 48000, |b, m| {
        match b {
            0 => {
                m.set_tempo(400.0);
                m.set_beats_per_bar(4);
                m.set_gain(0.5, true);
                m.reset_relative_start(0.0);
            }
            40 => m.set_gain(0.9, false),
            70 => m.set_tempo(251.0),
            95 => m.set_tempo(399.0),
            _ => {}
        }
        (0.0, true)
    });
}

#[test]
fn metronome_host_sync() {
    run_met("met_sync_44k", 44100, |b, m| {
        if b == 0 {
            m.set_tempo(333.0);
            m.set_beats_per_bar(3);
            m.set_gain(1.0, true);
        }
        if b == 64 {
            m.set_gain(0.25, false);
        }
        (0.37 + (b * BS) as f64 * (333.0 / 60.0) / 44100.0, false)
    });
}
