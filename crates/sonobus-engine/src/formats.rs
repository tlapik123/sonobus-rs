//! The send-quality table (`initFormats`, P.cpp:1329-1350): index -> codec. The index is what
//! `defsendqual` and the per-peer cache store.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendCodec {
    /// Opus at this many bits/s per channel, complexity 10, signal MUSIC, RESTRICTED_LOWDELAY.
    Opus { bitrate_per_channel: i32 },
    /// PCM with this many bytes per sample: 2 int16, 3 int24, 4 float32.
    Pcm { bytes: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendFormat {
    pub codec: SendCodec,
    /// Codec blocksize is `max(device block, this)` (`formatInfoToAooFormat`, P.cpp:6318).
    pub min_blocksize: i32,
    /// Upstream display name (`computeName`, P.cpp:1308-1327).
    pub name: &'static str,
}

const fn opus(kbps: i32, min_blocksize: i32, name: &'static str) -> SendFormat {
    SendFormat {
        codec: SendCodec::Opus {
            bitrate_per_channel: kbps * 1000,
        },
        min_blocksize,
        name,
    }
}

const fn pcm(bytes: i32, name: &'static str) -> SendFormat {
    SendFormat {
        codec: SendCodec::Pcm { bytes },
        min_blocksize: 16,
        name,
    }
}

pub const FORMATS: [SendFormat; 11] = [
    opus(16, 960, "16 kbps/ch"),
    opus(24, 480, "24 kbps/ch"),
    opus(48, 240, "48 kbps/ch"),
    opus(64, 240, "64 kbps/ch"),
    opus(96, 120, "96 kbps/ch"),
    opus(128, 120, "128 kbps/ch"),
    opus(160, 120, "160 kbps/ch"),
    opus(256, 120, "256 kbps/ch"),
    pcm(2, "PCM 16 bit"),
    pcm(3, "PCM 24 bit"),
    pcm(4, "PCM 32 bit float"),
];

/// 96 kbps/ch Opus; also upstream's "emergency default" for out-of-range indices.
pub const DEFAULT_FORMAT: usize = 4;

/// `defsendqual` allows 0..14, the table has 11 entries: anything else is the default.
pub fn checked(index: i32) -> usize {
    usize::try_from(index)
        .ok()
        .filter(|&i| i < FORMATS.len())
        .unwrap_or(DEFAULT_FORMAT)
}

/// `findFormatIndex` (P.cpp:1352-1369).
pub fn find(codec: SendCodec) -> Option<usize> {
    FORMATS.iter().position(|f| f.codec == codec)
}

/// PR #201 codec UUIDs of [`FORMATS`], index for index, as `/sb/pinfo` `supportedcodecs`
/// carries them (JUCE `Uuid::toString()`: undashed lowercase hex). A peer that sends no list
/// (every stock 1.7.2 client) is taken to decode exactly these: the legacy 8 Opus + 3 PCM.
pub const CODEC_IDS: [&str; FORMATS.len()] = [
    "d30affc3db7640fd85bd6ba4f67d75f0",
    "540af31d3047420d89d7472c07dcbb2e",
    "df40ba8a362142d2a9d7b6fa0ef1be84",
    "e440a4d9a45044628aa89b4af8576514",
    "14aaab94dbd64446bf1b64efb55f16f6",
    "79dbf03bc1124517aa1cfa501fc7d0b6",
    "4333f3e575374b94af9adfd836d59055",
    "46c728a4d8304388a493361cb3f7a220",
    "d7942c5c4802425bac5116e995af03cd",
    "70b9b5d8e2634053a58472a9e97ed97d",
    "9825290ef2cf41be9ff80c25f60ae8ad",
];

/// Whether a peer that advertised `codecs` (`None`: nothing, legacy) decodes table entry `index`.
pub fn supports(codecs: Option<&[String]>, index: usize) -> bool {
    index < FORMATS.len() && codecs.is_none_or(|c| c.iter().any(|id| id == CODEC_IDS[index]))
}

/// `index` if the peer decodes it, else the nearest table entry it does (the lower one on a
/// tie); `None` when it decodes none of ours.
pub fn nearest_supported(codecs: Option<&[String]>, index: usize) -> Option<usize> {
    (0..FORMATS.len())
        .filter(|&i| supports(codecs, i))
        .min_by_key(|&i| i.abs_diff(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonobus_core::sbmsg::codec_id;

    #[test]
    fn codec_ids_and_fallback() {
        // PR #201 writes them dashed in its source
        assert_eq!(codec_id("d30affc3-db76-40fd-85bd-6ba4f67d75f0"), CODEC_IDS[0]);
        assert_eq!(codec_id("9825290e-f2cf-41be-9ff8-0c25f60ae8ad"), CODEC_IDS[10]);
        // legacy: everything
        assert!((0..FORMATS.len()).all(|i| supports(None, i)));
        assert!(!supports(None, FORMATS.len()));
        assert_eq!(nearest_supported(None, 6), Some(6));
        let some: Vec<String> = [1, 4, 9].map(|i| CODEC_IDS[i].to_owned()).into();
        let c = Some(&some[..]);
        assert!(supports(c, 4) && !supports(c, 5));
        assert_eq!(nearest_supported(c, 4), Some(4));
        assert_eq!(nearest_supported(c, 6), Some(4));
        assert_eq!(nearest_supported(c, 7), Some(9));
        assert_eq!(nearest_supported(c, 0), Some(1));
        // a tie goes to the lower (cheaper) one
        let two: Vec<String> = [2, 4].map(|i| CODEC_IDS[i].to_owned()).into();
        assert_eq!(nearest_supported(Some(&two), 3), Some(2));
        assert_eq!(nearest_supported(Some(&[]), 3), None);
        assert_eq!(nearest_supported(Some(&["pcm8".to_owned()]), 3), None);
    }
}
