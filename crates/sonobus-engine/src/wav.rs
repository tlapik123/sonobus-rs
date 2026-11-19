//! Just enough RIFF/WAVE reading for the embedded metronome clicks (16-bit PCM).

/// First channel of a 16-bit PCM WAV as floats, scaled like JUCE (`1 / 32768`). Empty if the
/// data is not 16-bit PCM.
pub(crate) fn decode_pcm16(b: &[u8]) -> Vec<f32> {
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Vec::new();
    }
    let mut channels = 0usize;
    let mut pos = 12;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &b[pos + 8..(pos + 8).saturating_add(len).min(b.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                let format = u16::from_le_bytes([body[0], body[1]]);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                if format != 1 || bits != 16 {
                    return Vec::new();
                }
                channels = u16::from_le_bytes([body[2], body[3]]) as usize;
            }
            b"data" if channels > 0 => {
                return body
                    .chunks_exact(2 * channels)
                    .map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0)
                    .collect();
            }
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    Vec::new()
}

pub(crate) fn beat_click() -> Vec<f32> {
    decode_pcm16(include_bytes!("../assets/beat_click.wav"))
}

pub(crate) fn bar_click() -> Vec<f32> {
    decode_pcm16(include_bytes!("../assets/bar_click.wav"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_decode() {
        let beat = beat_click();
        // data chunk 0x0a68 bytes = 1332 frames; first sample 0xf91c
        assert_eq!(beat.len(), 0x0a68 / 2);
        assert_eq!(beat[0], -1764.0 / 32768.0);
        assert_eq!(beat[1], 0x063a as f32 / 32768.0);
        assert!(!bar_click().is_empty());
        assert!(decode_pcm16(b"RIFF\0\0\0\0WAVEjunk").is_empty());
    }
}
