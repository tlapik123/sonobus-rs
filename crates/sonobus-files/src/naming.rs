//! Recording file names as upstream builds them.
//!
//! Base name: `SonobusPluginEditor.cpp:2216-2218`
//! (`(group.isEmpty() ? "SonoBusSession" : group) + "_" + "%Y-%m-%d_%H.%M.%S"`, made legal).
//! Per-track names inside the recording folder: `SonobusPluginProcessor.cpp:9337` (`-MIXMINUS`),
//! `:9371` (`-SELF` / `-SELF-<group>`), `:9401` (`-MIX`), `:9454` (`-<username>`), each made legal
//! together with its extension.

use std::path::{Path, PathBuf};

/// Wall-clock time of the recording start, supplied by the caller (local time, as upstream's
/// `Time::getCurrentTime().formatted(...)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// What one recorder captures; picks the file-name suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind<'a> {
    /// Full mix (`-MIX`). A recording that is only the mix uses the bare base name instead.
    Mix,
    /// Mix minus own input (`-MIXMINUS`).
    MixMinus,
    /// Own input group; `Some(name)` for a named group (`-SELF-<name>`), `None` for `-SELF`.
    SelfInput(Option<&'a str>),
    /// A remote user's raw audio (`-<username>`).
    User(&'a str),
}

/// Base name of a recording: `<group or "SonoBusSession">_YYYY-MM-DD_HH.MM.SS`, legalised.
pub fn session_base_name(group: &str, t: LocalTime) -> String {
    let prefix = if group.is_empty() {
        "SonoBusSession"
    } else {
        group
    };
    legal_file_name(&format!(
        "{prefix}_{:04}-{:02}-{:02}_{:02}.{:02}.{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    ))
}

/// File name of one track inside the recording folder, `<base><suffix>.<ext>`, legalised.
pub fn track_file_name(base: &str, kind: TrackKind<'_>, extension: &str) -> String {
    let suffix = match kind {
        TrackKind::Mix => "-MIX".to_owned(),
        TrackKind::MixMinus => "-MIXMINUS".to_owned(),
        TrackKind::SelfInput(None) | TrackKind::SelfInput(Some("")) => "-SELF".to_owned(),
        TrackKind::SelfInput(Some(group)) => format!("-SELF-{group}"),
        TrackKind::User(name) => format!("-{name}"),
    };
    legal_file_name(&format!("{base}{suffix}.{extension}"))
}

/// Longest legal name in bytes: file systems allow 255; [`nonexistent_path`] may add
/// `(NNNNNNN)`.
pub const MAX_NAME_BYTES: usize = 240;

/// JUCE `File::createLegalFileName`: drops `"#@,;:<>*^|?\/` and caps the name at 128
/// characters, keeping a short (< 12 char) extension. Not in JUCE: control characters are
/// dropped too, and the name is also cut to [`MAX_NAME_BYTES`] bytes (128 four-byte characters
/// would exceed the file system's 255-byte limit), on a character boundary, extension kept.
pub fn legal_file_name(original: &str) -> String {
    const MAX: usize = 128;
    let s: Vec<char> = original
        .chars()
        .filter(|c| !"\"#@,;:<>*^|?\\/".contains(*c) && !c.is_control())
        .collect();
    let len = s.len();
    // JUCE keeps an extension that starts within the last 12 characters
    let (stem, ext) = match s.iter().rposition(|&c| c == '.') {
        Some(dot) if dot > len.saturating_sub(12) => s.split_at(dot),
        _ => (&s[..], &[][..]),
    };
    let stem = &stem[..stem.len().min(MAX.saturating_sub(ext.len()))];
    let ext: String = ext.iter().collect();
    let mut out = String::new();
    let mut bytes = ext.len();
    for &c in stem {
        bytes += c.len_utf8();
        if bytes > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    out.push_str(&ext);
    out
}

/// JUCE `File::getNonexistentSibling` (numbers in brackets): `name.ext`, else `name(2).ext`,
/// `name(3).ext`, ...; an existing `name(N)` suffix continues from N.
pub fn nonexistent_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_owned();
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // JUCE's extension is everything from the last dot, even for dotfiles.
    let (stem, ext) = match name.rfind('.') {
        Some(i) => name.split_at(i),
        None => (name.as_str(), ""),
    };
    // Not in JUCE: the number is capped and so is the search, so a name like `a(1844...)`
    // chosen by a peer cannot overflow it or loop for ever; past the cap the last candidate
    // is returned, and creating it fails (the caller reports that).
    const MAX_START: u64 = 1_000_000;
    const MAX_TRIES: u32 = 10_000;
    let mut prefix = stem;
    let mut number = 1u64;
    if stem.trim().ends_with(')')
        && let (Some(open), Some(close)) = (stem.rfind('('), stem.rfind(')'))
        && open > 0
        && close > open
        && stem[open + 1..close].chars().all(|c| c.is_ascii_digit())
    {
        // JUCE getIntValue(): empty -> 0.
        number = stem[open + 1..close]
            .parse::<u64>()
            .map_or(if open + 1 < close { MAX_START } else { 0 }, |n| n.min(MAX_START));
        prefix = &stem[..open];
    }
    let mut candidate = path.to_owned();
    for _ in 0..MAX_TRIES {
        number += 1;
        candidate = parent.join(format!("{prefix}({number}){ext}"));
        if !candidate.exists() {
            break;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: LocalTime = LocalTime {
        year: 2024,
        month: 3,
        day: 7,
        hour: 9,
        minute: 5,
        second: 2,
    };

    #[test]
    fn base_name_matches_upstream_format() {
        assert_eq!(
            session_base_name("", T),
            "SonoBusSession_2024-03-07_09.05.02"
        );
        // Group names go through createLegalFileName: ':' and '/' are dropped.
        assert_eq!(
            session_base_name("Jam: A/B", T),
            "Jam AB_2024-03-07_09.05.02"
        );
    }

    #[test]
    fn track_names_match_upstream() {
        let b = "SonoBusSession_2024-03-07_09.05.02";
        assert_eq!(
            track_file_name(b, TrackKind::Mix, "flac"),
            format!("{b}-MIX.flac")
        );
        assert_eq!(
            track_file_name(b, TrackKind::MixMinus, "wav"),
            format!("{b}-MIXMINUS.wav")
        );
        assert_eq!(
            track_file_name(b, TrackKind::SelfInput(None), "ogg"),
            format!("{b}-SELF.ogg")
        );
        assert_eq!(
            track_file_name(b, TrackKind::SelfInput(Some("Guitar")), "flac"),
            format!("{b}-SELF-Guitar.flac")
        );
        assert_eq!(
            track_file_name(b, TrackKind::User("bob@home?"), "flac"),
            format!("{b}-bobhome.flac")
        );
    }

    #[test]
    fn legal_name_truncates_like_juce() {
        let long = format!("{}.flac", "a".repeat(200));
        let legal = legal_file_name(&long);
        assert_eq!(legal.chars().count(), 128);
        assert!(legal.ends_with("a.flac"));
        // No usable extension (dot too far from the end): plain cut.
        let long = format!("x.{}", "b".repeat(200));
        assert_eq!(legal_file_name(&long), format!("x.{}", "b".repeat(126)));
    }

    #[test]
    fn legal_name_fits_the_file_system() {
        // 128 four-byte characters: 512 bytes, cut to MAX_NAME_BYTES, extension kept
        let name = format!("{}.flac", "\u{1F3B8}".repeat(127));
        let legal = legal_file_name(&name);
        assert!(legal.len() <= MAX_NAME_BYTES, "{}", legal.len());
        assert!(legal.ends_with("\u{1F3B8}.flac"));
        let long = legal_file_name(&format!("{}.wav", "\u{e9}".repeat(300)));
        assert!(long.len() <= MAX_NAME_BYTES && long.ends_with(".wav"));
        // control characters go
        assert_eq!(legal_file_name("a\tb\nc\x1e.wav"), "abc.wav");
        // short names are unchanged
        assert_eq!(legal_file_name("take-bob.flac"), "take-bob.flac");
    }

    #[test]
    fn nonexistent_sibling_with_a_huge_number() {
        let dir = std::env::temp_dir().join(format!("sbfiles-naming-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a(18446744073709551615).wav", "a(99999999999999999999999).wav"] {
            let p = dir.join(n);
            std::fs::write(&p, b"").unwrap();
            let got = nonexistent_path(&p);
            assert_eq!(got, dir.join("a(1000001).wav"), "{n}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nonexistent_sibling_numbering() {
        let dir = std::env::temp_dir().join(format!("sbfiles-naming-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("take.flac");
        assert_eq!(nonexistent_path(&p), p);
        std::fs::write(&p, b"").unwrap();
        assert_eq!(nonexistent_path(&p), dir.join("take(2).flac"));
        std::fs::write(dir.join("take(2).flac"), b"").unwrap();
        assert_eq!(nonexistent_path(&p), dir.join("take(3).flac"));
        // Continues from an existing bracketed number.
        assert_eq!(
            nonexistent_path(&dir.join("take(2).flac")),
            dir.join("take(3).flac")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
