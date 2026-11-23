//! Gain ramps and pan law as upstream applies them: JUCE `AudioBuffer` gain helpers
//! (`deps/juce/modules/juce_audio_basics/buffers/juce_AudioSampleBuffer.h`) and the pan
//! computation repeated in `ChannelGroup::processPan/processMonitor/processReverbSend`.
//!
//! JUCE's ramp starts at `start` on the first sample and steps by `(end - start) / n`, so
//! the last sample gets `end - step`: the target is only reached on the next block.
//! Its "clear buffer" fast path is not modelled; it is numerically identical.

/// Upstream `centerPanLaw` default: 0.596 (about -4.5 dB at center).
pub const DEFAULT_CENTER_PAN_LAW: f32 = 0.596;

/// JUCE `approximatelyEqual` for floats (absolute `f32::MIN_POSITIVE`, relative epsilon).
pub fn approximately_equal(a: f32, b: f32) -> bool {
    if !(a.is_finite() && b.is_finite()) {
        return a == b;
    }
    let diff = (a - b).abs();
    diff <= f32::MIN_POSITIVE || diff <= f32::EPSILON * a.abs().max(b.abs())
}

/// `AudioBuffer::applyGain` on one channel.
pub fn apply_gain(buf: &mut [f32], gain: f32) {
    if approximately_equal(gain, 1.0) {
        return;
    }
    if approximately_equal(gain, 0.0) {
        buf.fill(0.0);
    } else {
        buf.iter_mut().for_each(|x| *x *= gain);
    }
}

/// `AudioBuffer::applyGainRamp` on one channel.
pub fn apply_gain_ramp(buf: &mut [f32], start: f32, end: f32) {
    if approximately_equal(start, end) {
        return apply_gain(buf, start);
    }
    let step = (end - start) / buf.len() as f32;
    let mut g = start;
    for x in buf {
        *x *= g;
        g += step;
    }
}

/// `AudioBuffer::addFrom` with gain: `dst += src * gain`.
pub fn add_from(dst: &mut [f32], src: &[f32], gain: f32) {
    if approximately_equal(gain, 0.0) {
        return;
    }
    if approximately_equal(gain, 1.0) {
        dst.iter_mut().zip(src).for_each(|(d, s)| *d += *s);
    } else {
        dst.iter_mut().zip(src).for_each(|(d, s)| *d += *s * gain);
    }
}

/// `AudioBuffer::addFromWithRamp`: `dst += src * ramp(start -> end)`.
pub fn add_from_with_ramp(dst: &mut [f32], src: &[f32], start: f32, end: f32) {
    if approximately_equal(start, end) {
        return add_from(dst, src, start);
    }
    let step = (end - start) / dst.len().min(src.len()) as f32;
    let mut g = start;
    for (d, s) in dst.iter_mut().zip(src) {
        *d += g * *s;
        g += step;
    }
}

/// Upstream pan law for a stereo destination: `[left, right]` gains for `pan` in -1..=1
/// (-1 left), before the caller's level factor (upstream multiplies that in last).
pub fn pan_gains(pan: f32, center_pan_law: f32) -> [f32; 2] {
    let law = center_pan_law + (pan.abs() * (1.0 - center_pan_law));
    let (l, r) = if pan >= 0.0 {
        (1.0 - pan, 1.0)
    } else {
        (1.0, 1.0 + pan)
    };
    [l * law, r * law]
}

/// Upstream only ramps a pan change when it moved by more than this.
pub const PAN_CHANGE_EPSILON: f32 = 0.00001;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_stops_one_step_short_of_target() {
        let mut b = [1.0f32; 4];
        apply_gain_ramp(&mut b, 0.0, 1.0);
        assert_eq!(b, [0.0, 0.25, 0.5, 0.75]);
        let mut d = [1.0f32; 4];
        add_from_with_ramp(&mut d, &[2.0; 4], 1.0, 0.0);
        assert_eq!(d, [3.0, 2.5, 2.0, 1.5]);
    }

    #[test]
    fn equal_gains_take_the_flat_path() {
        let mut b = [2.0f32; 3];
        apply_gain_ramp(&mut b, 0.5, 0.5);
        assert_eq!(b, [1.0; 3]);
        apply_gain_ramp(&mut b, 0.0, 0.0);
        assert_eq!(b, [0.0; 3]);
        let mut d = [1.0f32; 2];
        add_from(&mut d, &[f32::NAN; 2], 0.0); // JUCE skips zero gain entirely
        assert_eq!(d, [1.0; 2]);
    }

    #[test]
    fn pan_law() {
        assert_eq!(pan_gains(0.0, DEFAULT_CENTER_PAN_LAW), [0.596, 0.596]);
        assert_eq!(pan_gains(-1.0, DEFAULT_CENTER_PAN_LAW), [1.0, 0.0]);
        assert_eq!(pan_gains(1.0, DEFAULT_CENTER_PAN_LAW), [0.0, 1.0]);
        let [l, r] = pan_gains(0.5, DEFAULT_CENTER_PAN_LAW);
        let law = 0.596 + 0.5 * (1.0 - 0.596);
        assert_eq!([l, r], [0.5 * law, law]);
    }
}
