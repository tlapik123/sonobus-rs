//! Compares the Rust effects with reference vectors produced by the upstream Faust C++
//! headers (SonoBus 1.7.2), driven the way `ChannelGroup.cpp` drives them.
//! Data: `tests/data/<case>.f32` = little-endian f32 `left[N] ++ right[N] ++ meter[NB]`
//! (meter = compressor `outgain` after each block, 0 for other effects).
//! Input signal and cases mirror the C++ harness exactly.

use sonobus_dsp::{Compressor, Expander, Limiter, ParametricEq, auto_makeup_gain_db};

const N: usize = 2048;
const BS: usize = 256;
const NB: usize = N / BS;
const SR: u32 = 48000;
/// Relative tolerance (absolute below 1.0). The reference was made on Linux with glibc's libm.
/// Other platforms' math libraries differ in the last bits of the coefficient math, and the
/// recursive filters and the dB meter amplify that: macOS arm64 was up to 5.2e-5 off.
const TOL: f32 = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    1e-5
} else {
    2e-4
};

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        ((self.0 >> 8) as i32 - 8388608) as f32 / 8388608.0
    }
}

/// 128 samples of digital silence, then: left = noise at stepped levels, then a
/// 50 Hz -> 8 kHz linear sweep at stepped levels; right = independent noise at
/// stepped levels (one step is silent).
fn input() -> (Vec<f32>, Vec<f32>) {
    let (mut l, mut r) = (vec![0.0f32; N], vec![0.0f32; N]);
    let (mut a, mut b) = (Lcg(12345), Lcg(987654321));
    let llev = [0.002f32, 0.02, 0.2, 0.9, 0.05, 0.5, 0.01];
    let slev = [0.05f32, 0.6, 1.0, 0.003];
    let rlev = [0.3f32, 0.001, 0.7, 0.0, 0.1, 0.8, 0.02, 0.4];
    for i in 0..N {
        let na = a.next();
        let nb = b.next();
        if i < 128 {
            continue;
        }
        if i < 1024 {
            l[i] = na * llev[(i - 128) / 128];
        } else {
            let t = (i - 1024) as f64 / 48000.0;
            let tt = 1024.0 / 48000.0;
            let phase =
                2.0 * std::f64::consts::PI * (50.0 * t + (8000.0 - 50.0) * t * t / (2.0 * tt));
            l[i] = phase.sin() as f32 * slev[(i - 1024) / 256];
        }
        r[i] = nb * rlev[(i - 128) / 256];
    }
    (l, r)
}

fn load(name: &str) -> Vec<f32> {
    let path = format!("{}/tests/data/{name}.f32", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert_eq!(bytes.len(), (2 * N + NB) * 4);
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// Runs `block(b, left, right)` over 256-sample blocks and checks against the reference.
/// `meter()` is sampled after each block. Returns the max abs error and the number of
/// reference NaNs (which must be exact zeros in Rust).
fn check(
    name: &str,
    mono: bool,
    mut block: impl FnMut(usize, &mut [f32], Option<&mut [f32]>) -> f32,
) -> (f32, usize) {
    let (mut l, mut r) = input();
    if mono {
        r.fill(0.0);
    }
    let mut meter = Vec::new();
    for (b, (lb, rb)) in l.chunks_mut(BS).zip(r.chunks_mut(BS)).enumerate() {
        meter.push(block(b, lb, if mono { None } else { Some(rb) }));
    }
    let want = load(name);
    let got: Vec<f32> = l.into_iter().chain(r).chain(meter).collect();
    let mut max_err = 0.0f32;
    let mut nans = 0;
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        if w.is_nan() {
            // Only `exp_ratio1`: upstream's NaN-on-silence bug, fixed here (see expander.rs).
            assert_eq!(*g, 0.0, "{name}[{i}]: upstream NaN must be silence");
            nans += 1;
            continue;
        }
        let err = (g - w).abs();
        assert!(
            err <= TOL * w.abs().max(1.0),
            "{name}[{i}]: got {g}, want {w}"
        );
        max_err = max_err.max(err);
    }
    eprintln!("{name}: max abs error {max_err:e}, {nans} upstream NaNs");
    (max_err, nans)
}

fn comp(thr: f32, ratio: f32, att: f32, rel: f32) -> Compressor {
    let mut c = Compressor::new(SR);
    c.set_threshold_db(thr);
    c.set_ratio(ratio);
    c.set_attack_ms(att);
    c.set_release_ms(rel);
    c.set_makeup_gain_db(auto_makeup_gain_db(thr, ratio));
    c.set_enabled(true);
    c
}

#[test]
fn compressor() {
    let mut c = Compressor::new(SR);
    c.set_enabled(true);
    check("comp_default", false, |_, l, r| {
        c.process(l, r);
        c.gain_reduction_db()
    });

    let mut c = comp(-60.0, 20.0, 1.0, 1000.0);
    check("comp_extreme_mono", true, |_, l, r| {
        c.process(l, r);
        c.gain_reduction_db()
    });

    let mut c = Compressor::new(SR);
    c.set_enabled(true);
    check("comp_change_bypass", false, |b, l, r| {
        if b == 3 {
            c.set_threshold_db(-40.0);
            c.set_ratio(8.0);
            c.set_makeup_gain_db(auto_makeup_gain_db(-40.0, 8.0));
        }
        if b == 5 {
            c.set_enabled(false); // block 5 still processed, 6..7 pass through
        }
        c.process(l, r);
        c.gain_reduction_db()
    });
}

#[test]
fn expander() {
    let mut e = Expander::new(SR);
    e.set_enabled(true);
    check("exp_default", false, |_, l, r| {
        e.process(l, r);
        0.0
    });

    let mut e = Expander::new(SR);
    e.set_threshold_db(-30.0);
    e.set_ratio(20.0);
    e.set_attack_ms(1.0);
    e.set_release_ms(1.0);
    e.set_enabled(true);
    check("exp_extreme_mono", true, |_, l, r| {
        e.process(l, r);
        0.0
    });

    // Ratio 1 on digital silence yields NaN upstream (128 samples x 2 channels in the
    // reference); Rust outputs exact silence there instead.
    let mut e = Expander::new(SR);
    e.set_threshold_db(-96.0);
    e.set_ratio(1.0);
    e.set_enabled(true);
    let (_, nans) = check("exp_ratio1", false, |_, l, r| {
        e.process(l, r);
        0.0
    });
    assert_eq!(nans, 2 * 128);
}

fn eq_set(q: &mut ParametricEq, v: [f32; 10]) {
    q.set_low_shelf_gain_db(v[0]);
    q.set_low_shelf_freq_hz(v[1]);
    q.set_para1_gain_db(v[2]);
    q.set_para1_freq_hz(v[3]);
    q.set_para1_q(v[4]);
    q.set_para2_gain_db(v[5]);
    q.set_para2_freq_hz(v[6]);
    q.set_para2_q(v[7]);
    q.set_high_shelf_gain_db(v[8]);
    q.set_high_shelf_freq_hz(v[9]);
}

#[test]
fn parametric_eq() {
    let mut q = ParametricEq::new(SR);
    q.set_enabled(true);
    check("eq_default", false, |_, l, r| {
        q.process(l, r);
        0.0
    });

    let mut q = ParametricEq::new(SR);
    q.set_enabled(true);
    eq_set(
        &mut q,
        [
            24.0, 2000.0, -24.0, 40.0, 0.4, 24.0, 10000.0, 100.0, -24.0, 500.0,
        ],
    );
    check("eq_extreme", false, |_, l, r| {
        q.process(l, r);
        0.0
    });

    let mut q = ParametricEq::new(SR);
    q.set_enabled(true);
    check("eq_change_mono", true, |b, l, r| {
        if b == 2 {
            eq_set(
                &mut q,
                [
                    -12.0, 120.0, 9.0, 2500.0, 8.0, -6.0, 700.0, 0.7, 6.0, 6000.0,
                ],
            );
        }
        q.process(l, r);
        0.0
    });
}

#[test]
fn limiter() {
    let mut m = Limiter::new(SR);
    m.set_enabled(true);
    check("lim_default", false, |_, l, r| {
        m.process(l, r);
        0.0
    });

    let mut m = Limiter::new(SR);
    m.set_threshold_db(-12.0);
    m.set_ratio(120.0);
    m.set_release_ms(1.0);
    m.set_enabled(true);
    check("lim_hot_mono", true, |_, l, r| {
        m.process(l, r);
        0.0
    });
}
