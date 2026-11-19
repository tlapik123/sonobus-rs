//! File playback overview and "Trim to New" on a generated WAV.

use sonobus_app::waveform::{BUCKETS, compute_peaks, trim_to_new};

/// 16-bit mono 48 kHz: one second of silence, then one second of a half-scale square wave.
fn wav(path: &std::path::Path) {
    let n = 96_000u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + 2 * n).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [16u32.to_le_bytes(), [1, 0, 1, 0], 48000u32.to_le_bytes(), 96000u32.to_le_bytes(), [2, 0, 16, 0]] {
        b.extend_from_slice(&v);
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(2 * n).to_le_bytes());
    for i in 0..n {
        let v: i16 = if i < 48_000 { 0 } else if (i / 50) % 2 == 0 { 16384 } else { -16384 };
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

#[test]
fn peaks_and_trim() {
    let dir = std::env::temp_dir().join(format!("sonobus-waveform-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("take.wav");
    wav(&f);
    let p = compute_peaks(&f, 48000).unwrap();
    assert_eq!(p.len(), BUCKETS);
    // first half silent, second half +-0.5
    assert!(p[..BUCKETS / 2 - 1].iter().all(|q| q[0] == 0.0 && q[1] == 0.0));
    assert!(p[BUCKETS / 2 + 1..].iter().all(|q| (q[1] - 0.5).abs() < 0.01 && (q[0] + 0.5).abs() < 0.01));

    // the last 0.75 s to a new file next to it, same format
    let t = trim_to_new(&f, 48000, 1.25, 0.75).unwrap();
    assert_eq!(t, dir.join("take-trim.wav"));
    let p = compute_peaks(&t, 48000).unwrap();
    let (player, _) = sonobus_files::open_player(&t, 48000).unwrap();
    assert_eq!(player.info().frames, Some(36_000));
    assert!(p.iter().all(|q| (q[1].max(-q[0]) - 0.5).abs() < 0.01));
    // again: a fresh name
    assert_eq!(trim_to_new(&f, 48000, 0.0, 0.5).unwrap(), dir.join("take-trim(2).wav"));
    let _ = std::fs::remove_dir_all(&dir);
}
