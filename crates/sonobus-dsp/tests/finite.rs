//! Every dynamics/EQ effect must keep finite input finite at its extreme settings
//! (silence, denormals, full-scale steps; stereo and mono paths).

use sonobus_dsp::{Compressor, Expander, Limiter, ParametricEq};

const SR: u32 = 48000;
const BS: usize = 256;

/// Silence, then denormals (alternating sign, several magnitudes), then full-scale steps
/// (DC +1, DC -1, square waves of 1, 2 and 64 samples), then silence again.
fn signal() -> Vec<f32> {
    let mut s = vec![0.0f32; 4 * BS];
    let denorms = [f32::from_bits(1), f32::MIN_POSITIVE / 2.0, f32::MIN_POSITIVE / 1024.0];
    s.extend((0..4 * BS).map(|i| denorms[i % 3] * if i % 2 == 0 { 1.0 } else { -1.0 }));
    s.extend(std::iter::repeat_n(1.0, 2 * BS));
    s.extend(std::iter::repeat_n(-1.0, 2 * BS));
    for period in [1usize, 2, 64] {
        s.extend((0..2 * BS).map(|i| if (i / period) % 2 == 0 { 1.0 } else { -1.0 }));
    }
    s.extend(std::iter::repeat_n(0.0, 8 * BS));
    s
}

fn run(name: &str, mut fx: impl FnMut(&mut [f32], Option<&mut [f32]>)) {
    for mono in [false, true] {
        let mut l = signal();
        let mut r: Vec<f32> = l.iter().map(|x| -x).collect();
        for (lb, rb) in l.chunks_mut(BS).zip(r.chunks_mut(BS)) {
            fx(lb, if mono { None } else { Some(rb) });
        }
        for (i, x) in l.iter().chain(&r).enumerate() {
            assert!(x.is_finite(), "{name} mono={mono}: sample {i} = {x}");
        }
    }
}

#[test]
fn compressor_extremes() {
    for (thr, ratio, ms, makeup) in [(-60.0, 120.0, 1.0, 60.0), (0.0, 1.0, 1000.0, 0.0), (-60.0, 1.0, 1.0, 60.0)] {
        let mut c = Compressor::new(SR);
        c.set_threshold_db(thr);
        c.set_ratio(ratio);
        c.set_attack_ms(ms);
        c.set_release_ms(ms);
        c.set_makeup_gain_db(makeup);
        c.set_enabled(true);
        run("compressor", |l, r| c.process(l, r));
    }
}

#[test]
fn expander_extremes() {
    for (thr, ratio, ms) in [(-96.0, 1.0, 1.0), (0.0, 120.0, 1.0), (0.0, 1.0, 1000.0), (-96.0, 120.0, 1000.0)] {
        let mut e = Expander::new(SR);
        e.set_threshold_db(thr);
        e.set_ratio(ratio);
        e.set_attack_ms(ms);
        e.set_release_ms(ms);
        e.set_enabled(true);
        run("expander", |l, r| e.process(l, r));
    }
}

#[test]
fn limiter_extremes() {
    for (thr, ratio, ms) in [(-60.0, 120.0, 1.0), (0.0, 1.0, 1000.0)] {
        let mut m = Limiter::new(SR);
        m.set_threshold_db(thr);
        m.set_ratio(ratio);
        m.set_attack_ms(0.01);
        m.set_release_ms(ms);
        m.set_enabled(true);
        run("limiter", |l, r| m.process(l, r));
    }
}

#[test]
fn eq_extremes() {
    for (g, lo, hi, q) in [(24.0, 20.0, 16000.0, 100.0), (-24.0, 2000.0, 500.0, 0.4), (24.0, 2000.0, 500.0, 0.4)] {
        let mut e = ParametricEq::new(SR);
        e.set_low_shelf_gain_db(g);
        e.set_low_shelf_freq_hz(lo);
        e.set_para1_gain_db(g);
        e.set_para1_freq_hz(40.0);
        e.set_para1_q(q);
        e.set_para2_gain_db(-g);
        e.set_para2_freq_hz(10000.0);
        e.set_para2_q(q);
        e.set_high_shelf_gain_db(g);
        e.set_high_shelf_freq_hz(hi);
        e.set_enabled(true);
        run("eq", |l, r| e.process(l, r));
    }
}
