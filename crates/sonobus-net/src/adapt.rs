//! Per-subscriber stream adaptation (pure policy, no I/O, clock passed in). Fed with the
//! receiver's [`Report`]s, RTT samples and the local datagram queue state, [`Adapt`] picks the
//! quality level (encoding: main 0 / low 1, or a bitrate ladder rung, best first) and the
//! redundancy `k` for one subscriber, and the Opus expected-loss percentage for the voice preset.
//! The transport runs a fixed congestion window: this is the only back-off.
//!
//! Signals:
//! - baseline = the smallest RTT of the last [`BASE_WINDOW`]; it follows a lower minimum at
//!   once but rises by at most [`BASE_DRIFT`] per second (a longer route is absorbed, a
//!   standing queue is not mistaken for one quickly);
//! - delay = the smallest RTT of the last [`RECENT`] (a standing queue, not a spike);
//! - loss = lost / blocks since the previous report.
//!
//! Rules, on every report:
//! - congested = delay > baseline + [`QUEUE_DELAY`] (absolute, so a queue that filled before
//!   or fills slowly counts), or loss while the RTT rose more than [`TREND_RISE`] within
//!   [`TREND_WINDOW`] (smallest samples of [`TREND_SMOOTH`] at either end: one spike is no
//!   trend), or the local datagram queue depleted for [`STALL_HOLD`]
//!   ([`Adapt::on_send_buffer`], which also acts without reports). It ends with a report that
//!   says otherwise, or [`REPORT_TIMEOUT`] without any report once the queue is not depleted.
//!   Congested: `k = 0` at once,
//!   one level down at most every [`DOWN_HOLD`], `k` is never raised; the send rate
//!   (`level bitrate x (k + 1)`) never increases while congestion lasts;
//! - random loss: loss above [`LOSS_THRESHOLD`] for [`RANDOM_LOSS_HOLD`], not congested →
//!   `k + 1` (at most [`MAX_K`]);
//! - [`CLEAN_HOLD`] without loss and with the delay within [`NEAR_BASE`] of the baseline →
//!   one notch back up (level first, then `k`); each notch needs its own clean period;
//! - other changes at most once per [`HYSTERESIS`];
//! - a receiver's QUALITY_REQ pins it: `preferred_format` (when it names a level) fixes the
//!   level, `max_bitrate` (when non-zero) caps level and `k` so `(k + 1) * bitrate` fits.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::wire::{QualityReq, Report};

pub const LOSS_THRESHOLD: f64 = 0.02;
pub const RANDOM_LOSS_HOLD: Duration = Duration::from_secs(3);
pub const TREND_WINDOW: Duration = Duration::from_secs(2);
pub const TREND_RISE: Duration = Duration::from_millis(30);
pub const BASE_WINDOW: Duration = Duration::from_secs(10);
/// How fast the baseline may rise, per second.
pub const BASE_DRIFT: Duration = Duration::from_millis(2);
pub const RECENT: Duration = Duration::from_secs(1);
/// Queueing delay above the baseline that means congestion.
pub const QUEUE_DELAY: Duration = Duration::from_millis(30);
/// Queueing delay below which the path counts as clean.
pub const NEAR_BASE: Duration = Duration::from_millis(15);
pub const CLEAN_HOLD: Duration = Duration::from_secs(20);
pub const HYSTERESIS: Duration = Duration::from_secs(3);
pub const DOWN_HOLD: Duration = Duration::from_secs(2);
/// Local datagram queue depleted this long: congested, without waiting for reports.
pub const STALL_HOLD: Duration = Duration::from_secs(1);
pub const MAX_K: u8 = 2;
/// No report for this long: congestion no longer holds NACKs and redundancy back.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(5);
/// The RTT trend compares the smallest samples of this long at both ends of the window.
pub const TREND_SMOOTH: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Decision {
    /// Index into the levels given to [`Adapt::new`].
    pub level: usize,
    pub redundancy: u8,
}

#[derive(Debug, Clone)]
pub struct Adapt {
    levels: Vec<u32>,
    /// Policy result before the receiver's request is applied.
    policy: Decision,
    last_change: Option<Instant>,
    last_down: Option<Instant>,
    lossy_since: Option<Instant>,
    clean_since: Instant,
    last_high: Option<u32>,
    /// RTT samples of the last [`BASE_WINDOW`].
    rtts: VecDeque<(Instant, Duration)>,
    /// Baseline and when it was last updated.
    base: Option<(Duration, Instant)>,
    stall_since: Option<Instant>,
    last_report: Instant,
    congested: bool,
    /// Smoothed loss fraction.
    loss: f64,
    req: Option<QualityReq>,
}

impl Adapt {
    /// `levels`: bitrates in bit/s, best first (at least one is assumed).
    pub fn new(now: Instant, levels: Vec<u32>) -> Self {
        let levels = if levels.is_empty() { vec![0] } else { levels };
        Self {
            levels,
            policy: Decision::default(),
            last_change: None,
            last_down: None,
            lossy_since: None,
            clean_since: now,
            last_high: None,
            rtts: VecDeque::new(),
            base: None,
            stall_since: None,
            last_report: now,
            congested: false,
            loss: 0.0,
            req: None,
        }
    }

    /// The path changed (direct ↔ relayed): forget its RTT history, the next samples set
    /// the baseline.
    pub fn reset_baseline(&mut self) {
        self.rtts.clear();
        self.base = None;
    }

    pub fn on_rtt(&mut self, now: Instant, rtt: Duration) {
        self.rtts.push_back((now, rtt));
        while self
            .rtts
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > BASE_WINDOW)
        {
            self.rtts.pop_front();
        }
        let min = self.rtts.iter().map(|x| x.1).min().unwrap_or(rtt);
        self.base = Some(match self.base {
            Some((b, at)) if min > b => {
                let dt = now.saturating_duration_since(at).as_secs_f64();
                (min.min(b + BASE_DRIFT.mul_f64(dt)), now)
            }
            _ => (min, now),
        });
    }

    fn rising(&self, now: Instant) -> bool {
        let win = self
            .rtts
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= TREND_WINDOW);
        let Some(&(start, _)) = win.clone().next() else {
            return false;
        };
        let early = win
            .clone()
            .filter(|(t, _)| t.saturating_duration_since(start) <= TREND_SMOOTH);
        let late = win.filter(|(t, _)| now.saturating_duration_since(*t) <= TREND_SMOOTH);
        match (early.map(|x| x.1).min(), late.map(|x| x.1).min()) {
            (Some(a), Some(b)) => b.saturating_sub(a) > TREND_RISE,
            _ => false,
        }
    }

    /// Queueing delay: the smallest RTT of the last [`RECENT`] above the baseline.
    pub fn queue_delay(&self, now: Instant) -> Option<Duration> {
        let (base, _) = self.base?;
        let recent = self
            .rtts
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= RECENT)
            .map(|x| x.1)
            .min()?;
        Some(recent.saturating_sub(base))
    }

    fn stalled(&self, now: Instant) -> bool {
        self.stall_since
            .is_some_and(|t| now.saturating_duration_since(t) >= STALL_HOLD)
    }

    /// Congested at the last evaluation (NACK resends should pause).
    pub fn congested(&self) -> bool {
        self.congested
    }

    /// Congested and already at the lowest level: only a cheaper stream would help.
    pub fn exhausted(&self) -> bool {
        self.congested && self.policy.level + 1 >= self.levels.len()
    }

    /// Clean for [`CLEAN_HOLD`] at the best level without redundancy: room for a costlier
    /// stream (the caller may undo a fallback).
    pub fn has_headroom(&self, now: Instant) -> bool {
        !self.congested
            && self.policy == Decision::default()
            && now.saturating_duration_since(self.clean_since) >= CLEAN_HOLD
    }

    /// The datagram send queue towards the subscriber is (nearly) full, or datagrams are being
    /// dropped locally. Depleted for [`STALL_HOLD`] counts as congestion and steps down even
    /// when no report arrives.
    pub fn on_send_buffer(&mut self, now: Instant, depleted: bool) -> Decision {
        if !depleted {
            self.stall_since = None;
            if now.saturating_duration_since(self.last_report) >= REPORT_TIMEOUT {
                self.congested = false;
            }
            return self.decision();
        }
        self.stall_since.get_or_insert(now);
        if self.stalled(now) {
            self.congested = true;
            self.step_down(now);
        }
        self.decision()
    }

    /// `k = 0` at once, one level down at most every [`DOWN_HOLD`].
    fn step_down(&mut self, now: Instant) {
        self.clean_since = now;
        self.lossy_since = None;
        let mut d = self.policy;
        d.redundancy = 0;
        let since = |t: Instant| now.saturating_duration_since(t);
        if d.level + 1 < self.levels.len() && self.last_down.is_none_or(|t| since(t) >= DOWN_HOLD) {
            d.level += 1;
            self.last_down = Some(now);
        }
        if d != self.policy {
            self.policy = d;
            self.last_change = Some(now);
        }
    }

    /// The receiver's QUALITY_REQ (`None` clears it); applies at once.
    pub fn set_request(&mut self, req: Option<QualityReq>) {
        self.req = req;
    }

    pub fn on_report(&mut self, now: Instant, r: &Report) -> Decision {
        self.last_report = now;
        let expected = self.last_high.map(|h| r.highest_seq.wrapping_sub(h));
        self.last_high = Some(r.highest_seq);
        // the first report, a restarted sequence or a stalled stream say nothing about loss
        let loss = match expected {
            Some(n) if n > 0 && n < 1 << 20 => (f64::from(r.lost) / f64::from(n)).min(1.0),
            _ => return self.decision(),
        };
        self.loss += (loss - self.loss) * 0.3;
        let delay = self.queue_delay(now);
        self.congested = delay.is_some_and(|d| d > QUEUE_DELAY)
            || (loss > 0.0 && self.rising(now))
            || self.stalled(now);
        if self.congested {
            self.step_down(now);
            return self.decision();
        }
        if loss > LOSS_THRESHOLD {
            self.lossy_since.get_or_insert(now);
        } else {
            self.lossy_since = None;
        }
        if loss > 0.0 || delay.is_none_or(|d| d > NEAR_BASE) {
            self.clean_since = now;
        }
        let since = |t: Instant| now.saturating_duration_since(t);
        let mut d = self.policy;
        let mut up = false;
        if self
            .lossy_since
            .is_some_and(|t| since(t) >= RANDOM_LOSS_HOLD)
        {
            d.redundancy = (d.redundancy + 1).min(MAX_K);
        } else if since(self.clean_since) >= CLEAN_HOLD {
            if d.level > 0 {
                d.level -= 1;
            } else {
                d.redundancy = d.redundancy.saturating_sub(1);
            }
            up = true;
        }
        if d != self.policy && self.last_change.is_none_or(|t| since(t) >= HYSTERESIS) {
            self.policy = d;
            self.last_change = Some(now);
            // the next step needs its own evidence
            if self.lossy_since.is_some() {
                self.lossy_since = Some(now);
            }
            if up {
                self.clean_since = now;
            }
        }
        self.decision()
    }

    /// Current result, the receiver's request applied.
    pub fn decision(&self) -> Decision {
        let mut d = self.policy;
        let last = self.levels.len() - 1;
        if let Some(q) = self.req {
            if usize::from(q.preferred_format) <= last {
                // a pin never lifts the quality above what congestion allows
                d.level =
                    usize::from(q.preferred_format).max(if self.congested { d.level } else { 0 });
            }
            if q.max_bitrate > 0 {
                let cap = self
                    .levels
                    .iter()
                    .position(|&b| b <= q.max_bitrate)
                    .unwrap_or(last);
                d.level = d.level.max(cap);
            }
        }
        d.redundancy = d.redundancy.min(self.max_redundancy(d.level));
        d
    }

    /// Most redundant blocks the receiver's `max_bitrate` allows at `level`, so that
    /// `(k + 1) * bitrate` fits (`u8::MAX` without a cap). Bounds any `k` sent at that level,
    /// including one the sender raises on its own.
    pub fn max_redundancy(&self, level: usize) -> u8 {
        match self.req {
            Some(q) if q.max_bitrate > 0 => {
                let per = u64::from(self.levels[level.min(self.levels.len() - 1)].max(1));
                (u64::from(q.max_bitrate) / per).saturating_sub(1).min(255) as u8
            }
            _ => u8::MAX,
        }
    }

    /// Expected loss for the voice preset's Opus FEC, percent.
    pub fn opus_loss_percent(&self) -> u8 {
        (self.loss * 100.0).round().clamp(0.0, 100.0) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCKS: u32 = 400; // per report (2.5 ms blocks)

    struct Script {
        a: Adapt,
        t0: Instant,
        sec: u64,
        high: u32,
    }

    impl Script {
        fn new(levels: Vec<u32>) -> Self {
            let t0 = Instant::now();
            Self {
                a: Adapt::new(t0, levels),
                t0,
                sec: 0,
                high: 0,
            }
        }

        /// One second: RTT samples every 100 ms from `rtt(t)` (t in seconds since the start),
        /// then a report with `lost` blocks.
        fn second_with(&mut self, lost: u16, rtt: &dyn Fn(f64) -> f64) -> Decision {
            for i in 0..10 {
                let t = self.sec as f64 + i as f64 / 10.0;
                let at = self.t0 + Duration::from_secs_f64(t);
                self.a.on_rtt(at, Duration::from_secs_f64(rtt(t) / 1e3));
            }
            self.sec += 1;
            self.high = self.high.wrapping_add(BLOCKS);
            let r = Report {
                highest_seq: self.high,
                lost,
                ..Report::default()
            };
            self.a
                .on_report(self.t0 + Duration::from_secs(self.sec), &r)
        }

        /// One second with the RTT ramping by `rtt_step_ms` per second from `rtt_ms`.
        fn second(&mut self, lost: u16, rtt_ms: f64, rtt_step_ms: f64) -> Decision {
            self.second_with(lost, &|t| rtt_ms + rtt_step_ms * t)
        }

        fn run(&mut self, secs: u64, lost: u16, rtt_step_ms: f64) -> Vec<Decision> {
            (0..secs)
                .map(|_| self.second(lost, 20.0, rtt_step_ms))
                .collect()
        }

        fn run_with(&mut self, secs: u64, lost: u16, rtt: &dyn Fn(f64) -> f64) -> Vec<Decision> {
            (0..secs).map(|_| self.second_with(lost, rtt)).collect()
        }
    }

    fn d(level: usize, redundancy: u8) -> Decision {
        Decision { level, redundancy }
    }

    fn pairs(ds: &[Decision]) -> Vec<(usize, u8)> {
        ds.iter().map(|x| (x.level, x.redundancy)).collect()
    }

    /// Send rate of a decision: the level's bitrate times the copies.
    fn rate(levels: &[u32], x: &Decision) -> u64 {
        u64::from(levels[x.level]) * (u64::from(x.redundancy) + 1)
    }

    #[test]
    fn random_loss_raises_k_with_hysteresis() {
        let mut s = Script::new(vec![96_000, 32_000]);
        s.second(0, 20.0, 0.0); // baseline report
        // 5 % loss, flat RTT: k = 1 once loss has lasted 3 s, k = 2 three seconds later, no more
        let ds = s.run(12, 20, 0.0);
        assert_eq!(&ds[..3], &[d(0, 0); 3]);
        assert_eq!(ds[3], d(0, 1));
        assert_eq!(&ds[4..6], &[d(0, 1); 2]);
        assert_eq!(ds[6], d(0, 2));
        assert!(ds[7..].iter().all(|&x| x == d(0, 2)));
        assert!(!s.a.congested());
        // 1 % loss is below the threshold: nothing changes, and it is not "clean" either
        assert!(s.run(30, 4, 0.0).iter().all(|&x| x == d(0, 2)));
        assert!(s.a.opus_loss_percent() >= 1);
        // 20 s clean: one notch down in k per 20 s
        let ds = s.run(41, 0, 0.0);
        assert_eq!(ds[18], d(0, 2));
        assert_eq!(ds[19], d(0, 1));
        assert_eq!(ds[39], d(0, 0));
        assert_eq!(s.a.opus_loss_percent(), 0);
    }

    /// The review's trace: 10 % loss, delay climbing 20 → 220 ms in 1.5 s, then flat (an
    /// already-inflated queue). The old rise-only test took the flat part for random loss and
    /// went (1,0) → (1,2): three copies of the low encoding. Now: lowest level, k = 0, and the
    /// send rate never goes up while the queue stands.
    #[test]
    fn standing_queue_with_loss_ends_lowest_without_redundancy() {
        let ramp = |t: f64| 20.0 + (t.min(1.5) / 1.5) * 200.0;
        for levels in [vec![96_000, 32_000], vec![128_000, 96_000, 64_000, 32_000]] {
            let mut s = Script::new(levels.clone());
            let mut ds = vec![s.second_with(0, &ramp)];
            ds.extend(s.run_with(14, 40, &ramp));
            eprintln!("rising-then-flat {levels:?}: {:?}", pairs(&ds));
            let last = levels.len() - 1;
            assert_eq!(*ds.last().unwrap(), d(last, 0));
            assert!(ds.iter().all(|x| x.redundancy == 0), "{ds:?}");
            assert!(
                ds.windows(2)
                    .all(|w| rate(&levels, &w[1]) <= rate(&levels, &w[0]))
            );
            assert!(s.a.congested() && s.a.exhausted());
        }
        // k already raised by random loss before the queue builds: dropped at once
        let mut s = Script::new(vec![96_000, 32_000]);
        s.second(0, 20.0, 0.0);
        assert_eq!(s.run(4, 20, 0.0)[3], d(0, 1));
        let ds = s.run_with(3, 40, &|t| if t < 6.0 { 20.0 } else { 220.0 });
        assert_eq!(ds[1], d(1, 0), "{ds:?}");
    }

    /// A queue growing 10 ms/s (20 ms per trend window, under the rise test's 30 ms): the
    /// absolute test catches it once it stands 30 ms above the baseline.
    #[test]
    fn slow_queue_growth_is_detected() {
        let mut s = Script::new(vec![96_000, 32_000]);
        let slow = |t: f64| 20.0 + 10.0 * t;
        let ds = s.run_with(8, 20, &slow);
        eprintln!("slow queue: {:?}", pairs(&ds));
        assert!(ds.iter().all(|x| x.redundancy == 0), "{ds:?}");
        assert_eq!(*ds.last().unwrap(), d(1, 0));
        // without loss too: a standing queue is congestion
        let mut s = Script::new(vec![96_000, 32_000]);
        assert_eq!(*s.run_with(8, 0, &slow).last().unwrap(), d(1, 0));
    }

    /// A single-encoding stream gets a bitrate ladder from the caller; congestion walks it
    /// down one rung per `DOWN_HOLD`.
    #[test]
    fn ladder_steps_down_under_a_standing_queue() {
        let levels = vec![96_000, 72_000, 48_000, 36_000, 24_000];
        let mut s = Script::new(levels.clone());
        s.second(0, 20.0, 0.0);
        let ds = s.run_with(10, 8, &|t| if t < 1.0 { 20.0 } else { 120.0 });
        let lv: Vec<usize> = ds.iter().map(|x| x.level).collect();
        assert_eq!(lv, [1, 1, 2, 2, 3, 3, 4, 4, 4, 4]);
        assert!(s.a.exhausted());
    }

    #[test]
    fn congestion_drops_k_and_level_then_recovers() {
        let mut s = Script::new(vec![96_000, 32_000]);
        s.second(0, 20.0, 0.0);
        let ds = s.run(4, 20, 0.0);
        assert_eq!(ds[3], d(0, 1));
        // loss with RTT rising 25 ms/s: congestion once the rise exceeds 30 ms in 2 s (the
        // second report), acted on at once (no hysteresis wait)
        let t0 = s.sec as f64;
        let ds = s.run_with(4, 8, &|t| 20.0 + 25.0 * (t - t0));
        assert_eq!(ds[0], d(0, 1), "{ds:?}");
        assert!(
            ds[1..].iter().all(|&x| x == d(1, 0)),
            "lowest level reached: {ds:?}"
        );
        // still queued (RTT high), no loss: not clean
        assert!(s.run_with(10, 0, &|_| 200.0).iter().all(|&x| x == d(1, 0)));
        // the queue drains; 20 s at the baseline steps back up
        let ds = s.run(21, 0, 0.0);
        assert_eq!(ds[18], d(1, 0));
        assert_eq!(ds[20], d(0, 0));
        assert!(!s.a.congested());
        assert!(!s.a.has_headroom(s.t0 + Duration::from_secs(s.sec)));
        s.run(20, 0, 0.0);
        assert!(s.a.has_headroom(s.t0 + Duration::from_secs(s.sec)));
    }

    /// A longer route: the baseline rises with it (slowly), so the session recovers without
    /// the delay ever returning to the old minimum.
    #[test]
    fn route_change_is_absorbed() {
        let mut s = Script::new(vec![96_000, 32_000]);
        s.run(3, 0, 0.0);
        let ds = s.run_with(80, 0, &|_| 70.0);
        assert_eq!(ds[0], d(1, 0), "a 50 ms step looks like a queue at first");
        assert_eq!(*ds.last().unwrap(), d(0, 0), "{:?}", pairs(&ds));
    }

    #[test]
    fn local_send_queue_stall_steps_down_without_reports() {
        let t0 = Instant::now();
        let mut a = Adapt::new(t0, vec![96_000, 64_000, 32_000]);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(a.on_send_buffer(at(0), true), d(0, 0));
        assert_eq!(a.on_send_buffer(at(900), true), d(0, 0));
        assert_eq!(a.on_send_buffer(at(1000), true), d(1, 0));
        assert!(a.congested());
        assert_eq!(a.on_send_buffer(at(2000), true), d(1, 0));
        assert_eq!(a.on_send_buffer(at(3000), true), d(2, 0));
        // recovered: the stall clock restarts
        a.on_send_buffer(at(3100), false);
        assert_eq!(a.on_send_buffer(at(3200), true), d(2, 0));
    }

    /// Congestion with no report since: once the stall is gone and no report came for
    /// `REPORT_TIMEOUT`, NACKs and redundancy are no longer held back.
    #[test]
    fn congestion_clears_when_reports_stop() {
        let t0 = Instant::now();
        let mut a = Adapt::new(t0, vec![96_000, 32_000]);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        a.on_send_buffer(at(0), true);
        a.on_send_buffer(at(1000), true);
        assert!(a.congested());
        a.on_send_buffer(at(1100), false);
        // no report since the start
        let end = REPORT_TIMEOUT.as_millis() as u64;
        a.on_send_buffer(at(end - 100), false);
        assert!(a.congested(), "not before the timeout");
        a.on_send_buffer(at(end), false);
        assert!(!a.congested());
        // reports flowing: a congested report holds until the next report says otherwise
        let mut s = Script::new(vec![96_000, 32_000]);
        s.second(0, 20.0, 0.0);
        s.run_with(2, 0, &|_| 200.0);
        assert!(s.a.congested());
        let now = s.t0 + Duration::from_secs(s.sec);
        s.a.on_send_buffer(now + Duration::from_millis(500), false);
        assert!(s.a.congested());
        s.a.on_send_buffer(now + REPORT_TIMEOUT, false);
        assert!(!s.a.congested());
    }

    /// One RTT spike in a lossy report is not a rising trend.
    #[test]
    fn a_single_rtt_spike_with_loss_is_not_congestion() {
        let mut s = Script::new(vec![96_000, 32_000]);
        s.second(0, 20.0, 0.0);
        let t1 = s.sec as f64;
        let ds = s.run_with(1, 8, &|t| if t >= t1 + 0.85 { 200.0 } else { 20.0 });
        assert_eq!(ds[0], d(0, 0));
        assert!(!s.a.congested());
        // a sustained rise with loss still is
        let t2 = s.sec as f64;
        s.run_with(2, 8, &|t| if t >= t2 + 0.5 { 80.0 } else { 20.0 });
        assert!(s.a.congested());
    }

    /// A new path (direct to relayed): its RTT is the new baseline, not a queue.
    #[test]
    fn reset_baseline_takes_the_new_path_as_is() {
        let mut s = Script::new(vec![96_000, 32_000]);
        s.run(3, 0, 0.0);
        s.a.reset_baseline();
        let ds = s.run_with(5, 0, &|_| 90.0);
        assert!(ds.iter().all(|&x| x == d(0, 0)), "{ds:?}");
        assert!(!s.a.congested());
    }

    #[test]
    fn request_pins_level_and_caps_k() {
        let mut s = Script::new(vec![96_000, 32_000]);
        s.second(0, 20.0, 0.0);
        s.a.set_request(Some(QualityReq {
            max_bitrate: 0,
            preferred_format: 1,
        }));
        assert_eq!(s.a.decision(), d(1, 0));
        // random loss still raises k, the level stays pinned
        let ds = s.run(7, 20, 0.0);
        assert_eq!(ds[6], d(1, 2));
        // 70 kbit/s: main (96k) does not fit, k limited to 70/32 - 1 = 1
        s.a.set_request(Some(QualityReq {
            max_bitrate: 70_000,
            preferred_format: 0xff,
        }));
        assert_eq!(s.a.decision(), d(1, 1));
        s.a.set_request(None);
        assert_eq!(s.a.decision(), d(0, 2));
        // a pin to the best level does not override congestion
        s.a.set_request(Some(QualityReq {
            max_bitrate: 0,
            preferred_format: 0,
        }));
        s.run_with(2, 0, &|_| 200.0);
        assert_eq!(s.a.decision(), d(1, 0));
    }

    #[test]
    fn first_report_and_sequence_restart_are_ignored() {
        let mut s = Script::new(vec![96_000]);
        let t = s.t0 + Duration::from_secs(1);
        let r = |high, lost| Report {
            highest_seq: high,
            lost,
            ..Report::default()
        };
        assert_eq!(s.a.on_report(t, &r(1000, 999)), d(0, 0));
        assert_eq!(s.a.on_report(t, &r(5, 999)), d(0, 0)); // wrapped: restart, no loss figure
        assert_eq!(s.a.opus_loss_percent(), 0);
        assert_eq!(s.a.on_report(t, &r(105, 10)), d(0, 0));
        assert_eq!(s.a.opus_loss_percent(), 3); // 10 % * 0.3
    }
}
