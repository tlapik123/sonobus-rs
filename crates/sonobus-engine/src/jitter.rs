//! Per-peer receive jitter buffer control: the four modes, the auto-adjust algorithm, safety
//! muting and the latency estimate, as pure state machines over event times (ms, monotonic).
//!
//! Upstream: block-lost handler P.cpp:3890-3971, sink ping handler 4006-4100,
//! `updateSafetyMuting` 2234-2263, `setRemotePeerBufferTime` 5274-5308,
//! `setRemotePeerAutoresizeBufferMode` 5324-5346, `getRemotePeerLatencyInfo` 5599-5728.
//! Upstream's "time 0 = never" sentinels are `Option`s here.

/// `stats::RunCumulantor1D` (RunCumulantor.h, RunningCumulant.c `push_aggregate`): weighted
/// running mean and variance; callers decay `z` to forget old samples.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cumulant {
    pub z: f32,
    pub mean: f32,
    /// Variance (upstream calls it `s2xx`).
    pub var: f32,
}

impl Cumulant {
    pub fn push(&mut self, x: f32) {
        let q = self.z;
        let dx = (x - self.mean) / (1.0 + q);
        self.var += (0.0 - self.var) / (1.0 + q) + q * dx * dx;
        self.mean += dx;
        self.z += 1.0;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn reset_to(&mut self, v: f32) {
        *self = Self {
            z: 1.0,
            mean: v,
            var: 0.0,
        };
    }
}

/// `AutoNetBufferMode` (P.h:84-89); the value stored in `defnetauto` / `netbufauto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JitterMode {
    /// "Manual"
    Off = 0,
    /// "Auto Up": grow on drops, never shrink.
    AutoIncrease = 1,
    /// "Auto": grow on drops, shrink after 10 s without drops.
    #[default]
    AutoFull = 2,
    /// "Initial Auto": start at 0 and grow until 7 s pass without drops.
    InitialAuto = 3,
}

impl JitterMode {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Off,
            1 => Self::AutoIncrease,
            3 => Self::InitialAuto,
            _ => Self::AutoFull,
        }
    }
}

/// Jitter buffer state of one peer.
#[derive(Debug, Clone)]
pub struct Jitter {
    pub mode: JitterMode,
    /// `buffertimeMs`; the sink gets at least one audio block whatever this says.
    pub buffer_ms: f32,
    /// `netBufAutoBaseline`: Auto never shrinks below this.
    pub baseline_ms: f32,
    pub init_completed: bool,
    /// Set by group latency match (stage 2); disables shrinking.
    pub latency_matched: bool,
    /// `resetSafetyMuted`: received audio is muted until the stream looks stable.
    pub safety_muted: bool,
    pub dropped: i64,
    pub resent: i64,
    last_drop: Option<f64>,
    reset_drop: f64,
    last_drop_count: i64,
    last_decrease: Option<f64>,
    fast_drop_rate: Cumulant,
    pub fill: Cumulant,
    pub fill_slow: Cumulant,
}

/// What the caller must do after an event: push the new buffer size to the sink and send
/// `/sb/pinfo` to the peer.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    Buffer,
}

/// Largest jitter buffer: upstream's buffer slider maximum (PeersContainerView.cpp:473). Every
/// buffer the engine sets is clamped to it, whatever the arithmetic that produced it.
pub const MAX_BUFFER_MS: f32 = 5000.0;
/// Largest latency figure taken from a peer (`/sb/pinfo` latencies and buffer, `/sb/latinfo`,
/// `/sb/suggestlat`, ping round trips): real ones are tens to hundreds of ms.
pub const MAX_PEER_LATENCY_MS: f32 = 2000.0;

/// A latency in ms from the network: NaN or infinite is `None`, the rest is clamped to
/// `0..=MAX_PEER_LATENCY_MS`.
pub fn peer_ms(v: f64) -> Option<f32> {
    v.is_finite()
        .then(|| v.clamp(0.0, f64::from(MAX_PEER_LATENCY_MS)) as f32)
}

/// A buffer length to set: NaN is 0, the rest is clamped to `0..=MAX_BUFFER_MS`.
pub fn buffer_ms(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, MAX_BUFFER_MS) }
}

impl Jitter {
    /// A new peer (`doAddRemotePeerIfNecessary`, P.cpp:5999-6020): `buffer_ms` from the default
    /// or the peer cache.
    pub fn new(mode: JitterMode, buffer_ms: f32, now: f64) -> Self {
        let buffer_ms = if mode == JitterMode::InitialAuto {
            0.0
        } else {
            self::buffer_ms(buffer_ms)
        };
        let mut fast_drop_rate = Cumulant::default();
        fast_drop_rate.reset_to(0.0);
        Self {
            mode,
            buffer_ms,
            baseline_ms: 0.0,
            init_completed: false,
            latency_matched: false,
            safety_muted: buffer_ms < 3.0,
            dropped: 0,
            resent: 0,
            last_drop: None,
            reset_drop: now,
            last_drop_count: 0,
            last_decrease: None,
            fast_drop_rate,
            fill: Cumulant::default(),
            fill_slow: Cumulant::default(),
        }
    }

    /// Buffer the sink actually uses, and what upstream reports (`getRemotePeerBufferTime`).
    pub fn effective_ms(&self, block_ms: f32) -> f32 {
        self.buffer_ms.max(block_ms)
    }

    fn clear_drops(&mut self, now: f64) {
        self.resent = 0;
        self.dropped = 0;
        self.last_drop_count = 0;
        self.reset_drop = now;
        self.fast_drop_rate.reset_to(0.0);
    }

    /// `setRemotePeerBufferTime` (also the "reset" button with 0).
    pub fn set_buffer(&mut self, ms: f32, now: f64, block_ms: f32) {
        let ms = buffer_ms(ms);
        self.buffer_ms = ms;
        self.fill.reset();
        self.fill_slow.reset();
        self.baseline_ms = block_ms;
        self.init_completed = false;
        self.last_drop = Some(now);
        self.safety_muted = ms == 0.0;
        self.latency_matched = false;
        self.clear_drops(now);
    }

    /// `setRemotePeerAutoresizeBufferMode`. Switching to Initial Auto restarts from 0.
    pub fn set_mode(&mut self, mode: JitterMode, now: f64, block_ms: f32) -> Change {
        self.mode = mode;
        let mut change = Change::None;
        match mode {
            JitterMode::AutoFull => self.baseline_ms = block_ms,
            JitterMode::InitialAuto => {
                self.set_buffer(0.0, now, block_ms);
                change = Change::Buffer;
            }
            _ => {}
        }
        self.clear_drops(now);
        change
    }

    /// "Reset drop count" (`resetRemotePeerPacketStats`).
    pub fn reset_stats(&mut self, now: f64) {
        self.resent = 0;
        self.dropped = 0;
        self.reset_drop = now;
        self.fast_drop_rate.reset_to(0.0);
    }

    /// Sink `BLOCK_LOST` event.
    pub fn on_blocks_lost(
        &mut self,
        count: i32,
        now: f64,
        block_ms: f32,
        drop_rate_thresh: f32,
    ) -> Change {
        self.dropped += i64::from(count);
        if self.mode == JitterMode::Off {
            return Change::None;
        }
        let thresh = if self.mode == JitterMode::InitialAuto {
            1.0
        } else {
            drop_rate_thresh
        };
        let init_done = self.mode == JitterMode::InitialAuto && self.init_completed;
        let mut change = Change::None;
        match self.last_drop {
            Some(last) if !init_done => {
                let dt = (now - last) * 1e-3;
                if dt > 0.5 {
                    // any burst counts as one drop instance
                    if 1.0 / dt > f64::from(thresh) {
                        self.buffer_ms += block_ms;
                        self.fill.reset();
                        self.fill_slow.reset();
                        change = Change::Buffer;
                        if self.mode == JitterMode::AutoFull
                            && self.last_decrease.is_some_and(|t| (now - t) * 1e-3 < 2.0)
                        {
                            self.baseline_ms = self.buffer_ms;
                        }
                    }
                    self.push_drop_rate(dt);
                    self.last_drop = Some(now);
                    self.last_drop_count = self.dropped;
                }
            }
            last => {
                if let Some(last) = last {
                    self.push_drop_rate((now - last) * 1e-3);
                }
                self.last_drop = Some(now);
                self.last_drop_count = self.dropped;
            }
        }
        change
    }

    fn push_drop_rate(&mut self, dt: f64) {
        // upstream divides unguarded; a zero interval would poison the average with inf/NaN
        if dt > 0.0 {
            self.fast_drop_rate
                .push(((self.dropped - self.last_drop_count) as f64 / dt) as f32);
        }
    }

    /// Sink `PING` event (every ~2 s while the stream plays).
    pub fn on_ping(&mut self, now: f64, block_ms: f32) -> Change {
        let since_drop = (now - self.last_drop.unwrap_or(self.reset_drop)) * 1e-3;
        if self.mode != JitterMode::Off {
            if !self.init_completed && since_drop > 7.0 {
                self.init_completed = true;
                self.safety_muted = false;
                self.clear_drops(now);
            }
        } else {
            self.safety_muted = false;
        }
        if self.mode != JitterMode::AutoFull {
            return Change::None;
        }
        let Some(last_decrease) = self.last_decrease else {
            self.last_decrease = Some(now);
            return Change::None;
        };
        if self.buffer_ms > self.baseline_ms && !self.latency_matched {
            let quiet = self.last_drop.is_none_or(|t| (now - t) * 1e-3 > 10.0);
            if (now - last_decrease) * 1e-3 > 10.0 && quiet {
                self.buffer_ms = (self.buffer_ms - block_ms).max(self.baseline_ms);
                self.fill.reset();
                self.fill_slow.reset();
                self.last_decrease = Some(now);
                return Change::Buffer;
            }
            Change::None
        } else {
            self.last_decrease = Some(now);
            Change::None
        }
    }

    /// `updateSafetyMuting`: called for every received data packet while safety muted.
    pub fn on_packet(&mut self, now: f64) {
        if !self.safety_muted {
            return;
        }
        let rate = self.fast_drop_rate.mean;
        let since_reset = (now - self.reset_drop) * 1e-3;
        let since_drop = self.last_drop.map_or(since_reset, |t| (now - t) * 1e-3);
        if since_reset > 0.5
            && ((rate > 0.0 && rate < 2.0)
                || (rate == 0.0 && since_drop > 0.75)
                || self.buffer_ms > 15.0)
        {
            self.safety_muted = false;
        }
        self.fast_drop_rate.z *= 0.965;
        self.push_drop_rate(since_drop);
    }

    /// One audio block's `aoo_opt_buffer_fill_ratio` sample (P.cpp:7829-7836).
    pub fn push_fill(&mut self, ratio: f32) {
        self.fill.z *= 0.95;
        self.fill.push(ratio);
        self.fill_slow.z *= 0.99;
        self.fill_slow.push(ratio);
    }

    /// Jitter estimate shown to the user: `2 * s2xx_slow * buffer` (P.cpp:5684).
    pub fn jitter_ms(&self, block_ms: f32) -> f32 {
        2.0 * self.fill_slow.var * self.effective_ms(block_ms)
    }
}

/// App-level ping (`/sb/ping`) state: RTT smoothing of `handlePingEvent` (P.cpp:3426-3452).
#[derive(Debug, Clone, Default)]
pub struct Ping {
    pub last_ms: f32,
    pub smooth: Cumulant,
    /// A `/sb/pngack` arrived: AOO source pings no longer update the estimate.
    pub new_style: bool,
}

impl Ping {
    pub fn push(&mut self, rtt_ms: f64) {
        // a forged or garbled time stamp gives a negative or absurd round trip
        let Some(rtt) = peer_ms(rtt_ms).filter(|_| rtt_ms >= 0.0) else {
            return;
        };
        self.last_ms = rtt;
        if rtt_ms < 600.0 {
            self.smooth.z *= 0.5;
            self.smooth.push(self.last_ms);
        }
    }
}

/// What a peer told us in `/sb/pinfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RemoteInfo {
    pub jitter_buffer_ms: f32,
    pub in_latency_ms: f32,
    pub out_latency_ms: f32,
    pub recording: bool,
    pub net_type: i32,
    /// Any `/sb/pinfo` arrived: the new-style latency estimate applies.
    pub valid: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LatencyInfo {
    pub ping_ms: f32,
    pub incoming_ms: f32,
    pub outgoing_ms: f32,
    pub round_trip_ms: f32,
    pub jitter_ms: f32,
    /// The peer sent no `/sb/pinfo`: rough legacy estimate.
    pub legacy: bool,
}

/// `getRemotePeerLatencyInfo` without the measured-latency (latency test) branch.
pub fn latency(
    jitter: &Jitter,
    ping: &Ping,
    remote: &RemoteInfo,
    block_ms: f32,
    send_opus: bool,
    recv_opus: bool,
) -> LatencyInfo {
    let ping_ms = ping.smooth.mean;
    let buf = jitter.effective_ms(block_ms);
    let jitter_ms = jitter.jitter_ms(block_ms);
    if remote.valid {
        let codec = |opus| if opus { 2.5 } else { 0.0 };
        let incoming_ms = codec(recv_opus) + remote.in_latency_ms + ping_ms * 0.5 + buf;
        let outgoing_ms =
            codec(send_opus) + remote.out_latency_ms + ping_ms * 0.5 + remote.jitter_buffer_ms;
        LatencyInfo {
            ping_ms,
            incoming_ms,
            outgoing_ms,
            round_trip_ms: incoming_ms + outgoing_ms,
            jitter_ms,
            legacy: false,
        }
    } else {
        // `totalEstLatency`, recomputed instead of cached: ping + 2 * buffer + block
        let round_trip_ms = ping_ms + 2.0 * jitter.buffer_ms + block_ms;
        let incoming_ms = 2.0 * block_ms + ping_ms * 0.5 + buf;
        LatencyInfo {
            ping_ms,
            incoming_ms,
            outgoing_ms: round_trip_ms - incoming_ms,
            round_trip_ms,
            jitter_ms,
            legacy: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: f32 = 256.0 * 1000.0 / 48000.0;

    #[test]
    fn cumulant_matches_running_cumulant_c() {
        let mut c = Cumulant::default();
        for x in [1.0, 2.0, 3.0, 4.0] {
            c.push(x);
        }
        assert_eq!(c.z, 4.0);
        assert!((c.mean - 2.5).abs() < 1e-6);
        assert!((c.var - 1.25).abs() < 1e-6); // population variance
        c.reset_to(7.0);
        assert_eq!((c.z, c.mean, c.var), (1.0, 7.0, 0.0));
    }

    #[test]
    fn auto_full_grows_on_drops_and_shrinks_after_quiet() {
        let mut j = Jitter::new(JitterMode::AutoFull, 1.0, 0.0);
        assert!(j.safety_muted, "below 3 ms starts muted");
        // first loss only records the time
        assert_eq!(j.on_blocks_lost(2, 1000.0, BLOCK, 0.5), Change::None);
        // within 0.5 s: ignored entirely
        assert_eq!(j.on_blocks_lost(1, 1400.0, BLOCK, 0.5), Change::None);
        // 1 s later: rate 1/s > 0.5 -> +1 block
        assert_eq!(j.on_blocks_lost(1, 2000.0, BLOCK, 0.5), Change::Buffer);
        assert!((j.buffer_ms - (1.0 + BLOCK)).abs() < 1e-4);
        assert_eq!(j.dropped, 4);
        // 3 s later: rate 0.33/s < 0.5 -> no growth
        assert_eq!(j.on_blocks_lost(1, 5000.0, BLOCK, 0.5), Change::None);
        // pings: the first one only arms the decrease timer
        assert_eq!(j.on_ping(6000.0, BLOCK), Change::None);
        // 10 s since arming but drop only 9 s ago... wait until both are > 10 s
        assert_eq!(j.on_ping(15_000.0, BLOCK), Change::None);
        assert_eq!(j.on_ping(16_100.0, BLOCK), Change::Buffer);
        assert!((j.buffer_ms - 1.0).abs() < 1e-4 || j.buffer_ms == j.baseline_ms);
        assert!(j.init_completed);
        assert!(!j.safety_muted);
    }

    #[test]
    fn auto_full_drop_soon_after_decrease_sets_baseline() {
        let mut j = Jitter::new(JitterMode::AutoFull, 20.0, 0.0);
        j.set_buffer(20.0, 0.0, BLOCK);
        assert_eq!(j.on_ping(1000.0, BLOCK), Change::None); // arms
        assert_eq!(j.on_ping(12_000.0, BLOCK), Change::Buffer); // 20 -> 20 - block
        let after = j.buffer_ms;
        // 12.5 s since the last drop: 0.08/s, below threshold
        assert_eq!(j.on_blocks_lost(1, 12_500.0, BLOCK, 0.5), Change::None);
        // a drop 1.1 s after the decrease: grows back and pins the baseline there
        assert_eq!(j.on_blocks_lost(1, 13_100.0, BLOCK, 0.5), Change::Buffer);
        assert!((j.buffer_ms - (after + BLOCK)).abs() < 1e-4);
        assert_eq!(j.baseline_ms, j.buffer_ms);
        // never shrinks below the baseline
        assert_eq!(j.on_ping(40_000.0, BLOCK), Change::None);
    }

    #[test]
    fn auto_increase_never_shrinks_and_manual_never_moves() {
        let mut j = Jitter::new(JitterMode::AutoIncrease, 10.0, 0.0);
        let _ = j.on_blocks_lost(1, 100.0, BLOCK, 0.5);
        assert_eq!(j.on_blocks_lost(1, 1000.0, BLOCK, 0.5), Change::Buffer);
        for t in 1..20 {
            assert_eq!(j.on_ping(1000.0 + 5000.0 * t as f64, BLOCK), Change::None);
        }
        assert!((j.buffer_ms - (10.0 + BLOCK)).abs() < 1e-4);

        let mut m = Jitter::new(JitterMode::Off, 1.0, 0.0);
        assert!(m.safety_muted);
        for t in 1..5 {
            assert_eq!(
                m.on_blocks_lost(5, 700.0 * t as f64, BLOCK, 0.5),
                Change::None
            );
        }
        assert_eq!(m.dropped, 20);
        assert_eq!(m.buffer_ms, 1.0);
        // manual: the first sink ping clears the safety mute
        assert_eq!(m.on_ping(3000.0, BLOCK), Change::None);
        assert!(!m.safety_muted);
    }

    #[test]
    fn initial_auto_starts_at_zero_uses_threshold_one_and_stops_after_quiet() {
        let mut j = Jitter::new(JitterMode::InitialAuto, 30.0, 0.0);
        assert_eq!(j.buffer_ms, 0.0);
        assert!(j.safety_muted);
        let _ = j.on_blocks_lost(1, 100.0, BLOCK, 20.0);
        // 0.8 s apart: 1.25/s > 1.0 (the fixed Initial Auto threshold, not 20)
        assert_eq!(j.on_blocks_lost(1, 900.0, BLOCK, 20.0), Change::Buffer);
        // 1.5 s apart: 0.67/s < 1.0
        assert_eq!(j.on_blocks_lost(1, 2400.0, BLOCK, 20.0), Change::None);
        assert!(!j.init_completed);
        assert_eq!(j.on_ping(9000.0, BLOCK), Change::None);
        assert!(!j.init_completed, "only 6.6 s without drops");
        assert_eq!(j.on_ping(9500.0, BLOCK), Change::None);
        assert!(j.init_completed && !j.safety_muted);
        assert_eq!(j.dropped, 0, "counters cleared when init completes");
        // done: drops no longer grow it
        let b = j.buffer_ms;
        for t in 0..10 {
            let _ = j.on_blocks_lost(1, 10_000.0 + 600.0 * t as f64, BLOCK, 20.0);
        }
        assert_eq!(j.buffer_ms, b);
        // switching to Initial Auto again restarts at 0
        assert_eq!(
            j.set_mode(JitterMode::InitialAuto, 20_000.0, BLOCK),
            Change::Buffer
        );
        assert_eq!(j.buffer_ms, 0.0);
        assert!(!j.init_completed && j.safety_muted);
    }

    #[test]
    fn safety_unmute_rules() {
        // no drops for 0.75 s after a reset and more than 0.5 s since reset
        let mut j = Jitter::new(JitterMode::AutoFull, 1.0, 0.0);
        j.on_packet(400.0);
        assert!(j.safety_muted, "within 0.5 s of reset");
        j.on_packet(800.0);
        assert!(!j.safety_muted);
        // a large buffer unmutes regardless of drops (after 0.5 s)
        let mut j = Jitter::new(JitterMode::Off, 1.0, 0.0);
        j.buffer_ms = 20.0;
        let _ = j.on_blocks_lost(3, 10.0, BLOCK, 0.5);
        j.on_packet(600.0);
        assert!(!j.safety_muted);
        // `set_buffer(0)` re-arms it
        j.set_buffer(0.0, 1000.0, BLOCK);
        assert!(j.safety_muted);
        assert!(j.fast_drop_rate.mean.is_finite());
        j.on_packet(1000.0); // zero interval: no NaN
        assert!(j.fast_drop_rate.mean.is_finite());
    }

    #[test]
    fn latency_estimates() {
        let mut j = Jitter::new(JitterMode::Off, 20.0, 0.0);
        j.buffer_ms = 20.0;
        let mut p = Ping::default();
        p.push(30.0);
        p.push(10_000.0); // ignored for smoothing, shown capped
        assert_eq!(p.last_ms, MAX_PEER_LATENCY_MS);
        assert_eq!(p.smooth.mean, 30.0);
        // forged time stamps: ignored
        for bad in [-5.0, -1e18, f64::NAN, f64::INFINITY] {
            p.push(bad);
        }
        assert_eq!((p.last_ms, p.smooth.mean), (MAX_PEER_LATENCY_MS, 30.0));
        let r = RemoteInfo {
            jitter_buffer_ms: 15.0,
            in_latency_ms: 5.0,
            out_latency_ms: 5.0,
            valid: true,
            ..Default::default()
        };
        let l = latency(&j, &p, &r, 5.0, true, false);
        assert_eq!(l.incoming_ms, 0.0 + 5.0 + 15.0 + 20.0);
        assert_eq!(l.outgoing_ms, 2.5 + 5.0 + 15.0 + 15.0);
        assert_eq!(l.round_trip_ms, l.incoming_ms + l.outgoing_ms);
        assert!(!l.legacy);
        let l = latency(&j, &p, &RemoteInfo::default(), 5.0, true, true);
        assert!(l.legacy);
        assert_eq!(l.round_trip_ms, 30.0 + 40.0 + 5.0);
        assert_eq!(l.incoming_ms, 10.0 + 15.0 + 20.0);
    }
}
