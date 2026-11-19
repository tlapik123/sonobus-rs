//! One channel group's processing, ported from `ChannelGroup::processBlock / processPan /
//! processMonitor / processReverbSend` (ChannelGroup.cpp:204-617) with its effect chain
//! (expander -> compressor -> EQ -> limiter, only for 1- or 2-channel groups).
//!
//! Buffers are channel arrays; `*_ch` arguments are the logical channel counts of the JUCE
//! buffers the C++ passes (loops stop there exactly as upstream's do).

use sonobus_core::state::{
    ChannelGroup, Dynamics, MonitorDelay as DelayState, ParametricEq as EqState,
};
use sonobus_dsp::gain::{
    DEFAULT_CENTER_PAN_LAW, add_from, add_from_with_ramp, apply_gain_ramp, pan_gains,
};
use sonobus_dsp::{Compressor, Expander, Limiter, MonitorDelay, ParametricEq, auto_makeup_gain_db};

use super::MAX_CHANNELS;

/// `centerPanLaw` is never changed or persisted upstream.
const LAW: f32 = DEFAULT_CENTER_PAN_LAW;

pub(crate) fn idx(v: i32) -> usize {
    v.max(0) as usize
}

/// `ChannelGroup::ProcessState`, zero-initialised like the C++.
#[derive(Clone)]
pub(crate) struct ProcState {
    pub last_level: f32,
    last_pan: [f32; MAX_CHANNELS],
    last_stereo: [f32; 2],
}

impl Default for ProcState {
    fn default() -> Self {
        Self {
            last_level: 0.0,
            last_pan: [0.0; MAX_CHANNELS],
            last_stereo: [0.0; 2],
        }
    }
}

fn pan_of(g: &ChannelGroup, i: usize) -> f32 {
    g.pans.get(i).copied().unwrap_or(0.0)
}

/// `[left, right]` pan-law gain of `pan` for output side `left` (the first dest channel).
fn side(pan: f32, left: bool) -> f32 {
    pan_gains(pan, LAW)[usize::from(!left)]
}

fn two<T>(v: &mut [T], a: usize) -> (&mut T, &mut T) {
    let (x, y) = v.split_at_mut(a + 1);
    (&mut x[a], &mut y[0])
}

/// Shared mixing core of processPan/processMonitor/processReverbSend: `count` source channels
/// from `from_start` into `dest_n` destination channels. `pan_nch`/`last_nch` select the stereo
/// pan pair (== 2) for the current/last pan (upstream uses different counts in revsend).
/// `last_gain` `None` = processPan semantics (no ramp on gain changes, plain adds otherwise).
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn pan_mix<F: AsRef<[f32]>, T: AsMut<[f32]>>(
    g: &ChannelGroup,
    st: &mut ProcState,
    from: &[F],
    from_ch: usize,
    from_start: usize,
    count: usize,
    (pan_nch, last_nch): (usize, usize),
    to: &mut [T],
    to_ch: usize,
    (dest, dest_n): (usize, usize),
    n: usize,
    gain: f32,
    last_gain: Option<f32>,
) {
    let from_ch = from_ch.min(from.len());
    let to_ch = to_ch.min(to.len());
    let add = |dst: &mut [f32], src: &[f32]| match last_gain {
        None => add_from(&mut dst[..n], &src[..n], gain),
        Some(l) => add_from_with_ramp(&mut dst[..n], &src[..n], l, gain),
    };
    if dest_n == 2 {
        for ch in dest..(dest + 2).min(to_ch) {
            let left = ch == dest;
            for (pani, i) in (from_start..(from_start + count).min(from_ch)).enumerate() {
                let s = usize::from(pani != 0);
                let upan = if pan_nch != 2 {
                    pan_of(g, pani)
                } else {
                    g.pan_stereo[s]
                };
                let lastpan = if last_nch != 2 {
                    st.last_pan.get(pani).copied().unwrap_or(0.0)
                } else {
                    st.last_stereo[s]
                };
                let pg = side(upan, left) * gain;
                let changed = (upan - lastpan).abs() > 0.00001
                    || last_gain.is_some_and(|l| (l - gain).abs() > 0.00001);
                let (dst, src) = (&mut to[ch].as_mut()[..n], &from[i].as_ref()[..n]);
                if changed {
                    let plg = side(lastpan, left) * last_gain.unwrap_or(gain);
                    add_from_with_ramp(dst, src, plg, pg);
                } else {
                    add_from(dst, src, pg);
                }
            }
        }
    } else if dest_n == 1 {
        for i in from_start..(from_start + count).min(from_ch) {
            if dest < to_ch {
                add(to[dest].as_mut(), from[i].as_ref());
            }
        }
    } else {
        for (ch, i) in (dest..to_ch).zip(from_start..(from_start + count).min(from_ch)) {
            add(to[ch].as_mut(), from[i].as_ref());
        }
    }
    st.last_stereo = g.pan_stereo;
    for (k, p) in st.last_pan.iter_mut().enumerate().take(idx(g.num_channels)) {
        *p = pan_of(g, k);
    }
}

/// Effects and per-path state of one channel group.
pub(crate) struct GroupDsp {
    expander: Expander,
    compressor: Compressor,
    eq: ParametricEq,
    limiter: Limiter,
    /// Monitor delay line and the channel count it was made for (provisioned off the audio
    /// thread; upstream allocates it on first enable).
    delay: Option<(Box<MonitorDelay>, usize)>,
    /// The effect parameters last committed (`None`: commit on the next snapshot).
    committed: Option<(Dynamics, Dynamics, EqState, Dynamics, DelayState)>,
    main: ProcState,
    mon: ProcState,
    in_rev: ProcState,
    rev: ProcState,
}

impl GroupDsp {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            expander: Expander::new(sample_rate),
            compressor: Compressor::new(sample_rate),
            eq: ParametricEq::new(sample_rate),
            limiter: Limiter::new(sample_rate),
            delay: None,
            committed: None,
            main: ProcState::default(),
            mon: ProcState::default(),
            in_rev: ProcState::default(),
            rev: ProcState::default(),
        }
    }

    /// `commitAllParams`, skipped when the effect parameters are the ones last committed.
    pub fn commit(&mut self, g: &ChannelGroup) {
        let fx = (g.expander, g.compressor, g.eq, g.limiter, g.monitor_delay);
        if self.committed == Some(fx) {
            return;
        }
        self.committed = Some(fx);
        let e = &g.expander;
        self.expander.set_enabled(e.enabled);
        self.expander.set_threshold_db(e.threshold_db);
        self.expander.set_ratio(e.ratio);
        self.expander.set_attack_ms(e.attack_ms);
        self.expander.set_release_ms(e.release_ms);
        let c = &g.compressor;
        self.compressor.set_enabled(c.enabled);
        self.compressor.set_threshold_db(c.threshold_db);
        self.compressor.set_ratio(c.ratio);
        self.compressor.set_attack_ms(c.attack_ms);
        self.compressor.set_release_ms(c.release_ms);
        // upstream computes auto makeup when the params are set (P.cpp:1770-1783)
        let makeup = if c.auto_makeup {
            auto_makeup_gain_db(
                c.threshold_db,
                c.ratio.clamp(1.0, sonobus_dsp::RATIO_ENGINE_MAX),
            )
        } else {
            c.makeup_gain_db
        };
        self.compressor.set_makeup_gain_db(makeup);
        let q = &g.eq;
        self.eq.set_enabled(q.enabled);
        self.eq.set_low_shelf_gain_db(q.low_shelf_gain);
        self.eq.set_low_shelf_freq_hz(q.low_shelf_freq);
        self.eq.set_para1_gain_db(q.para1_gain);
        self.eq.set_para1_freq_hz(q.para1_freq);
        self.eq.set_para1_q(q.para1_q);
        self.eq.set_para2_gain_db(q.para2_gain);
        self.eq.set_para2_freq_hz(q.para2_freq);
        self.eq.set_para2_q(q.para2_q);
        self.eq.set_high_shelf_gain_db(q.high_shelf_gain);
        self.eq.set_high_shelf_freq_hz(q.high_shelf_freq);
        let l = &g.limiter;
        self.limiter.set_enabled(l.enabled);
        self.limiter.set_threshold_db(l.threshold_db);
        self.limiter.set_ratio(l.ratio);
        self.limiter.set_attack_ms(l.attack_ms);
        self.limiter.set_release_ms(l.release_ms);
        if let Some((d, _)) = &mut self.delay {
            d.set_delay_ms(f64::from(g.monitor_delay.delay_time_ms));
            d.set_enabled(g.monitor_delay.enabled);
        }
    }

    #[cfg(test)]
    pub fn committed_is(&self, g: &ChannelGroup) -> bool {
        self.committed == Some((g.expander, g.compressor, g.eq, g.limiter, g.monitor_delay))
    }

    /// A new monitor delay line: commit everything again on the next snapshot.
    pub fn set_delay(
        &mut self,
        d: (Box<MonitorDelay>, usize),
    ) -> Option<(Box<MonitorDelay>, usize)> {
        self.committed = None;
        self.delay.replace(d)
    }

    /// Clear the effects' filter and envelope state (after non-finite audio got in).
    pub fn reset_fx(&mut self) {
        self.expander.reset();
        self.compressor.reset();
        self.eq.reset();
        self.limiter.reset();
    }

    /// processBlock from a separate buffer (input groups): gain into `to[dest..]`, then effects
    /// and the input-reverb send.
    #[allow(clippy::too_many_arguments)]
    pub fn input_block(
        &mut self,
        g: &ChannelGroup,
        from: &[&[f32]],
        to: &mut [Vec<f32>],
        to_ch: usize,
        (dest, dest_n): (usize, usize),
        n: usize,
        gain: f32,
        rev: Option<(&mut [Vec<f32>], bool)>,
    ) {
        let dogain = self.dogain(g, gain);
        let start = idx(g.chan_start);
        for (i, d) in (start..start + idx(g.num_channels)).zip(dest..dest + dest_n) {
            if i >= from.len() || d >= to_ch {
                break;
            }
            add_from_with_ramp(&mut to[d][..n], &from[i][..n], self.main.last_level, dogain);
        }
        self.main.last_level = dogain;
        self.effects(g, to, to_ch, dest, dest_n, n);
        if let Some((bus, enabled)) = rev {
            let cnt = idx(g.num_channels).min(dest_n);
            Self::rev_send(
                g,
                &mut self.in_rev,
                to,
                to_ch,
                (dest, cnt),
                bus,
                n,
                enabled,
                true,
                1.0,
            );
        }
    }

    /// processBlock in place (remote peer groups): gain ramp on the group's channels, effects.
    pub fn inplace_block(
        &mut self,
        g: &ChannelGroup,
        buf: &mut [Vec<f32>],
        buf_ch: usize,
        n: usize,
        gain: f32,
    ) {
        let dogain = self.dogain(g, gain);
        let start = idx(g.chan_start);
        for c in buf
            .iter_mut()
            .take(buf_ch)
            .skip(start)
            .take(idx(g.num_channels))
        {
            apply_gain_ramp(&mut c[..n], self.main.last_level, dogain);
        }
        self.main.last_level = dogain;
        self.effects(g, buf, buf_ch, start, idx(g.num_channels), n);
    }

    fn dogain(&self, g: &ChannelGroup, gain: f32) -> f32 {
        let d = if g.muted { 0.0 } else { g.gain } * gain;
        if g.invert_polarity { -d } else { d }
    }

    fn effects(
        &mut self,
        g: &ChannelGroup,
        to: &mut [Vec<f32>],
        to_ch: usize,
        dest: usize,
        dest_n: usize,
        n: usize,
    ) {
        let numchan = idx(g.num_channels);
        if !(1..=2).contains(&numchan) {
            return;
        }
        let to_ch = to_ch.min(to.len());
        let stereo = to_ch > dest + 1 && numchan == 2 && dest_n >= 2;
        if !stereo && dest >= to_ch {
            return;
        }
        fn run(
            to: &mut [Vec<f32>],
            stereo: bool,
            dest: usize,
            n: usize,
            mut f: impl FnMut(&mut [f32], Option<&mut [f32]>),
        ) {
            if stereo {
                let (l, r) = two(to, dest);
                f(&mut l[..n], Some(&mut r[..n]));
            } else {
                f(&mut to[dest][..n], None);
            }
        }
        run(to, stereo, dest, n, |l, r| self.expander.process(l, r));
        run(to, stereo, dest, n, |l, r| self.compressor.process(l, r));
        run(to, stereo, dest, n, |l, r| self.eq.process(l, r));
        run(to, stereo, dest, n, |l, r| self.limiter.process(l, r));
    }

    /// processPan: `from[from_start..]` panned into `to[dest..dest + dest_n]`.
    #[allow(clippy::too_many_arguments)]
    pub fn pan<F: AsRef<[f32]>, T: AsMut<[f32]>>(
        &mut self,
        g: &ChannelGroup,
        from: &[F],
        from_ch: usize,
        from_start: usize,
        to: &mut [T],
        to_ch: usize,
        dests: (usize, usize),
        n: usize,
        gain: f32,
    ) {
        if from_ch == 0 {
            return;
        }
        let nch = idx(g.num_channels);
        pan_mix(
            g,
            &mut self.main,
            from,
            from_ch,
            from_start,
            nch,
            (nch, nch),
            to,
            to_ch,
            dests,
            n,
            gain,
            None,
        );
    }

    /// processReverbSend with the default (`revProcState`) state; `in_send` picks
    /// `inreverbsend` over `monreverbsend`.
    #[allow(clippy::too_many_arguments)]
    pub fn reverb_send(
        &mut self,
        g: &ChannelGroup,
        from: &[Vec<f32>],
        from_ch: usize,
        (from_start, from_n): (usize, usize),
        bus: &mut [Vec<f32>],
        n: usize,
        enabled: bool,
        gain: f32,
    ) {
        Self::rev_send(
            g,
            &mut self.rev,
            from,
            from_ch,
            (from_start, from_n),
            bus,
            n,
            enabled,
            false,
            gain,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn rev_send<F: AsRef<[f32]>>(
        g: &ChannelGroup,
        st: &mut ProcState,
        from: &[F],
        from_ch: usize,
        (from_start, from_n): (usize, usize),
        bus: &mut [Vec<f32>],
        n: usize,
        enabled: bool,
        in_send: bool,
        gain: f32,
    ) {
        let send = if in_send {
            g.in_reverb_send
        } else {
            g.mon_reverb_send
        };
        let target = gain * send * if enabled { 1.0 } else { 0.0 };
        let last = st.last_level;
        pan_mix(
            g,
            st,
            from,
            from_ch,
            from_start,
            from_n,
            (from_n, idx(g.num_channels)),
            bus,
            2,
            (0, 2),
            n,
            target,
            Some(last),
        );
        st.last_level = target;
    }

    /// processMonitor: monitor level, optional monitor delay, pan into `to`, and the monitor
    /// reverb send (`rev`: bus, enabled, extra gain) scaled by the monitor level.
    #[allow(clippy::too_many_arguments)]
    pub fn monitor<F: AsRef<[f32]>, T: AsMut<[f32]>>(
        &mut self,
        g: &ChannelGroup,
        from: &[F],
        from_ch: usize,
        from_start: usize,
        to: &mut [T],
        to_ch: usize,
        dests: (usize, usize),
        n: usize,
        gain: f32,
        rev: Option<(&mut [Vec<f32>], bool, f32)>,
        scratch: &mut [Vec<f32>],
    ) {
        let targmon = g.monitor * gain;
        let nch = idx(g.num_channels);
        let from_ch = from_ch.min(from.len());
        let delay = match &mut self.delay {
            Some((d, dch)) if *dch == nch && nch <= scratch.len() => Some(d),
            _ => None,
        };
        if let Some(d) = delay {
            // through the delay line (an identity copy while it is idle)
            for (c, s) in scratch.iter_mut().enumerate().take(nch) {
                match from
                    .get(from_start + c)
                    .filter(|_| from_start + c < from_ch)
                {
                    Some(src) => s[..n].copy_from_slice(&src.as_ref()[..n]),
                    None => s[..n].fill(0.0),
                }
            }
            let mut it = scratch.iter_mut();
            let mut chans: [&mut [f32]; MAX_CHANNELS] =
                std::array::from_fn(|_| it.next().map_or(&mut [][..], |c| &mut c[..n]));
            d.process(&mut chans[..nch]);
            self.monitor_mix(g, scratch, nch, 0, to, to_ch, dests, n, targmon, rev);
        } else {
            self.monitor_mix(
                g, from, from_ch, from_start, to, to_ch, dests, n, targmon, rev,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn monitor_mix<F: AsRef<[f32]>, T: AsMut<[f32]>>(
        &mut self,
        g: &ChannelGroup,
        from: &[F],
        from_ch: usize,
        from_start: usize,
        to: &mut [T],
        to_ch: usize,
        dests: (usize, usize),
        n: usize,
        targmon: f32,
        rev: Option<(&mut [Vec<f32>], bool, f32)>,
    ) {
        let nch = idx(g.num_channels);
        let last = self.mon.last_level;
        pan_mix(
            g,
            &mut self.mon,
            from,
            from_ch,
            from_start,
            nch,
            (nch, nch),
            to,
            to_ch,
            dests,
            n,
            targmon,
            Some(last),
        );
        if let Some((bus, enabled, revgain)) = rev {
            let cnt = nch.min(from_ch);
            Self::rev_send(
                g,
                &mut self.rev,
                from,
                from_ch,
                (from_start, cnt),
                bus,
                n,
                enabled,
                false,
                targmon * revgain,
            );
        }
        self.mon.last_level = targmon;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(nch: i32) -> ChannelGroup {
        ChannelGroup {
            num_channels: nch,
            pans: vec![0.0; nch as usize],
            ..Default::default()
        }
    }

    #[test]
    fn pan_matrix_mono_and_stereo() {
        let mut d = GroupDsp::new(48000);
        let from = [vec![1.0f32; 8], vec![0.5; 8]];
        // mono group, centre: 0.596 to both sides, no ramp (last pan 0 == pan 0)
        let mut g = group(1);
        let mut to = [vec![0.0f32; 8], vec![0.0; 8]];
        d.pan(&g, &from, 2, 0, &mut to, 2, (0, 2), 8, 1.0);
        assert_eq!(to[0], [LAW; 8]);
        assert_eq!(to[1], [LAW; 8]);
        // hard right after the ramp block
        g.pans[0] = 1.0;
        let mut to = [vec![0.0f32; 8], vec![0.0; 8]];
        d.pan(&g, &from, 2, 0, &mut to, 2, (0, 2), 8, 1.0);
        assert_eq!(to[0][0], LAW); // ramp starts at the old gain
        let mut to = [vec![0.0f32; 8], vec![0.0; 8]];
        d.pan(&g, &from, 2, 0, &mut to, 2, (0, 2), 8, 0.5);
        assert_eq!(to[0], [0.0; 8]);
        assert_eq!(to[1], [0.5; 8]);
        // stereo group with pans -1/+1 is an exact copy (after the first ramp block)
        let g = group(2);
        let mut d = GroupDsp::new(48000);
        let mut to = [vec![0.0f32; 8], vec![0.0; 8]];
        d.pan(&g, &from, 2, 0, &mut to, 2, (0, 2), 8, 1.0);
        let mut to = [vec![0.0f32; 8], vec![0.0; 8]];
        d.pan(&g, &from, 2, 0, &mut to, 2, (0, 2), 8, 1.0);
        assert_eq!(to, [vec![1.0; 8], vec![0.5; 8]]);
        // one dest channel: plain sum; straight-through to 3+ channels
        let mut to = [vec![0.0f32; 8]];
        d.pan(&g, &from, 2, 0, &mut to, 1, (0, 1), 8, 2.0);
        assert_eq!(to[0], [3.0; 8]);
        let mut to = [vec![0.0f32; 8], vec![0.0; 8], vec![0.0; 8], vec![0.0; 8]];
        d.pan(&g, &from, 2, 0, &mut to, 4, (1, 3), 8, 1.0);
        assert_eq!(to, [vec![0.0; 8], vec![1.0; 8], vec![0.5; 8], vec![0.0; 8]]);
    }

    #[test]
    fn gain_mute_polarity_ramp() {
        let mut d = GroupDsp::new(48000);
        let mut g = group(1);
        g.limiter.enabled = false;
        g.gain = 0.5;
        g.invert_polarity = true;
        d.commit(&g);
        let src = vec![1.0f32; 4];
        let from: [&[f32]; 1] = [&src];
        let mut to = vec![vec![0.0f32; 4]];
        d.input_block(&g, &from, &mut to, 1, (0, 1), 4, 1.0, None);
        assert_eq!(to[0], [0.0, -0.125, -0.25, -0.375]); // ramp from 0
        let mut to = vec![vec![0.0f32; 4]];
        d.input_block(&g, &from, &mut to, 1, (0, 1), 4, 1.0, None);
        assert_eq!(to[0], [-0.5; 4]);
        g.muted = true;
        let mut to = vec![vec![0.0f32; 4]];
        d.input_block(&g, &from, &mut to, 1, (0, 1), 4, 1.0, None);
        let mut to = vec![vec![0.0f32; 4]];
        d.input_block(&g, &from, &mut to, 1, (0, 1), 4, 1.0, None);
        assert_eq!(to[0], [0.0; 4]);
        // in place
        let mut buf = vec![vec![1.0f32; 4], vec![1.0; 4]];
        let mut g2 = group(1);
        g2.chan_start = 1;
        let mut d2 = GroupDsp::new(48000);
        d2.inplace_block(&g2, &mut buf, 2, 4, 1.0);
        d2.inplace_block(&g2, &mut buf, 2, 4, 2.0);
        assert_eq!(buf[0], [1.0; 4], "other channels untouched");
        assert_eq!(buf[1][0], 0.0);
    }
}
