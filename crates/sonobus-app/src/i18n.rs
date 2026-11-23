//! Translations: upstream's `localization/localized_*.txt` (JUCE `LocalisedStrings` text
//! format), embedded, picked once at startup from the language override (`langOverrideCode`)
//! or the OS locale, and applied by [`crate::tr`]. A language change needs a restart, as
//! upstream.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Upstream's language menu (OptionsView.cpp:40-76): code, English name, native name ("" =
/// same). CJK native names are only shown when a CJK font is loaded (upstream: universal font).
pub const LANGUAGES: [(&str, &str, &str); 13] = [
    ("", "System Default Language", ""),
    ("en", "English", ""),
    ("es", "Spanish", "español"),
    ("fr", "French", "français"),
    ("it", "Italian", "italiano"),
    ("de", "German", "Deutsch"),
    ("pt-pt", "Portuguese (Portugal)", "Português (Portugal)"),
    ("pt-br", "Portuguese (Brazil)", "Português (Brasil)"),
    ("nl", "Dutch", "Nederlands"),
    ("ja", "Japanese", "日本語"),
    ("ko", "Korean", "한국인"),
    ("zh-hans", "Chinese (Simplified)", "中文（简体）"),
    ("ru", "Russian", "русский"),
];

/// The shipped translation files.
pub fn file(code: &str) -> Option<&'static str> {
    Some(match code {
        "de" => include_str!("../assets/localization/localized_de.txt"),
        "es" => include_str!("../assets/localization/localized_es.txt"),
        "fr" => include_str!("../assets/localization/localized_fr.txt"),
        "it" => include_str!("../assets/localization/localized_it.txt"),
        "ja" => include_str!("../assets/localization/localized_ja.txt"),
        "ko" => include_str!("../assets/localization/localized_ko.txt"),
        "nl" => include_str!("../assets/localization/localized_nl.txt"),
        "pt-br" => include_str!("../assets/localization/localized_pt-br.txt"),
        "pt-pt" => include_str!("../assets/localization/localized_pt-pt.txt"),
        "ru" => include_str!("../assets/localization/localized_ru.txt"),
        "zh-hans" => include_str!("../assets/localization/localized_zh-hans.txt"),
        _ => return None,
    })
}

/// One parsed translation file.
#[derive(Debug, Default)]
pub struct Strings {
    pub language: String,
    pub countries: Vec<String>,
    /// Keyed by the lowercased English text: upstream loads with `ignoreCase = true` (a later
    /// pair replaces an earlier one that differs only in case).
    map: HashMap<String, String>,
}

impl Strings {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, english: &str) -> Option<&str> {
        self.map.get(&english.to_lowercase()).map(String::as_str)
    }
}

/// JUCE `findCloseQuote`: the byte index of the next `"` not preceded by `\`, or the end.
fn close_quote(s: &str, start: usize) -> usize {
    let start = start.min(s.len());
    let mut last = '\0';
    for (i, c) in s[start..].char_indices() {
        if c == '"' && last != '\\' {
            return start + i;
        }
        last = c;
    }
    s.len()
}

/// JUCE `unescapeString`: replaced one after the other, as JUCE does.
fn unescape(s: &str) -> String {
    s.replace("\\\"", "\"")
        .replace("\\'", "'")
        .replace("\\t", "\t")
        .replace("\\r", "\r")
        .replace("\\n", "\n")
}

/// `LocalisedStrings::loadFromText`: `language:` / `countries:` header lines, then
/// `"English" = "Translated"` lines. Anything else, and pairs with an empty side, is ignored.
pub fn parse(text: &str) -> Strings {
    let mut s = Strings::default();
    for line in text.lines() {
        let line = line.trim();
        // JUCE `substring` clamps; indices are char boundaries (after a `"` or the end)
        let sub = |a: usize, b: usize| line.get(a..b.max(a)).unwrap_or("");
        if line.starts_with('"') {
            let close = close_quote(line, 1);
            let original = unescape(sub(1, close));
            if original.is_empty() {
                continue;
            }
            let open = close_quote(line, close + 1);
            let close = close_quote(line, open + 1);
            let translated = unescape(sub(open + 1, close));
            if !translated.is_empty() {
                s.map.insert(original.to_lowercase(), translated);
            }
        } else if line.len() >= 9 && line[..9].eq_ignore_ascii_case("language:") {
            s.language = line[9..].trim().to_owned();
        } else if line.len() >= 10 && line[..10].eq_ignore_ascii_case("countries:") {
            s.countries
                .extend(line[10..].split_whitespace().map(str::to_owned));
        }
    }
    s
}

/// The OS locale (e.g. `de_DE.UTF-8` -> `de-de`): `LC_ALL` / `LC_MESSAGES` / `LANG` first (an
/// explicit override), else what the OS reports (`sys-locale`: the user locale on Windows and
/// macOS, Android's `persist.sys.locale`); "" when unknown or `C`/`POSIX`.
pub fn system_locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .or_else(sys_locale::get_locale)
        .map(|v| normalize_locale(&v))
        .unwrap_or_default()
}

/// `de_DE.UTF-8` / `de-DE` -> `de-de`; `C` / `POSIX` -> "".
pub fn normalize_locale(v: &str) -> String {
    let v = v
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .replace('_', "-");
    if v == "c" || v == "posix" {
        String::new()
    } else {
        v
    }
}

/// The shipped language for an override code or a locale (`setupLocalisation`,
/// SonobusPluginEditor.cpp:5300-5375): the full code first (`pt-br`), then the base language.
/// Unlike upstream, a Chinese locale outside TW/HK/MO maps to `zh-hans` and plain `pt` to
/// `pt-pt`. For the system locale, upstream's unvetted `nl` and `ja` stay English.
pub fn resolve(code: &str, from_system: bool) -> Option<&'static str> {
    let code = code.to_lowercase().replace('_', "-");
    let (base, region) = code.split_once('-').unwrap_or((&code, ""));
    if from_system && (base == "nl" || base == "ja") {
        return None;
    }
    let known = |c: &str| {
        LANGUAGES
            .iter()
            .map(|l| l.0)
            .find(|k| *k == c && file(k).is_some())
    };
    known(&code).or_else(|| match (base, region) {
        ("zh", "tw" | "hk" | "mo" | "hant") => None,
        ("zh", _) => known("zh-hans"),
        ("pt", _) => known("pt-pt"),
        (b, _) => known(b),
    })
}

/// Parsed translation tables, one per [`LANGUAGES`] entry, filled on first use. Immutable once
/// parsed, so plugin instances in other languages share them safely.
static TABLES: [OnceLock<Strings>; LANGUAGES.len()] = [const { OnceLock::new() }; LANGUAGES.len()];

fn strings(code: Option<&str>) -> Option<&'static Strings> {
    let code = code?;
    let i = LANGUAGES.iter().position(|l| l.0 == code)?;
    let text = file(code)?;
    Some(TABLES[i].get_or_init(|| parse(text)))
}

/// The process language of the standalone app (`None` inside = English).
static CURRENT: OnceLock<Option<&'static str>> = OnceLock::new();

thread_local! {
    /// A language in force on this thread ([`scope`]): a plugin instance's, while it draws or
    /// ticks. Overrides [`CURRENT`].
    static SCOPED: Cell<Option<Option<&'static str>>> = const { Cell::new(None) };
}

/// The language for an override code (upstream's `langOverrideCode`; "" = the OS locale),
/// without making it the process language.
pub fn pick(override_code: &str) -> Option<&'static str> {
    if override_code.is_empty() {
        resolve(&system_locale(), true)
    } else {
        resolve(override_code, false)
    }
}

/// Picks the process language once: `override_code` as in [`pick`]. Later calls change
/// nothing. Returns the language in use (`None` = English).
pub fn init(override_code: &str) -> Option<&'static str> {
    *CURRENT.get_or_init(|| pick(override_code))
}

/// Restores the previous thread language when dropped.
pub struct Scope(Option<Option<&'static str>>);

impl Drop for Scope {
    fn drop(&mut self) {
        SCOPED.with(|s| s.set(self.0));
    }
}

/// Makes `lang` this thread's language until the guard drops (several plugin instances in
/// one process, each in its own language).
pub fn scope(lang: Option<&'static str>) -> Scope {
    Scope(SCOPED.with(|s| s.replace(Some(lang))))
}

/// The language in use on this thread (`None` = English or not initialised yet).
pub fn current() -> Option<&'static str> {
    SCOPED
        .with(Cell::get)
        .unwrap_or_else(|| CURRENT.get().copied().flatten())
}

/// The translation of `s` in the current language, or `s`.
pub fn tr(s: &str) -> &str {
    match strings(current()) {
        Some(m) => m.get(s).unwrap_or(s),
        None => s,
    }
}

/// Languages whose script egui's bundled fonts lack (they need a system CJK font).
pub fn needs_cjk_font(code: Option<&str>) -> bool {
    matches!(code, Some("ja" | "ko" | "zh-hans"))
}

/// System fonts with CJK glyphs, best first, per platform. All found ones are loaded as
/// fallbacks (a Korean font after a Chinese one, and so on).
pub const CJK_FONTS: &[&str] = &[
    // Linux (Fedora, Debian/Ubuntu, Arch), Android
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-droid-sans-fonts/DroidSansFallbackFull.ttf",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/system/fonts/NotoSansCJK-Regular.ttc",
    "/system/fonts/DroidSansFallback.ttf",
    // macOS
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/AppleSDGothicNeo.ttc",
    "/Library/Fonts/Arial Unicode.ttf",
    // Windows
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\YuGothM.ttc",
    "C:\\Windows\\Fonts\\malgun.ttf",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_names() {
        assert_eq!(normalize_locale("de_DE.UTF-8"), "de-de");
        assert_eq!(normalize_locale("pt-BR"), "pt-br");
        assert_eq!(normalize_locale("sr_RS@latin"), "sr-rs");
        assert_eq!(normalize_locale("C"), "");
        assert_eq!(normalize_locale("POSIX"), "");
        assert_eq!(resolve(&normalize_locale("fr-FR"), true), Some("fr"));
    }

    #[test]
    fn grammar_as_juce() {
        let s = parse(
            "language: German\ncountries: de at\n\n\"Send\" = \"Senden\"\n  \"Say \\\"hi\\\"\" = \"Sag \\\"hallo\\\"\"  \n\"A\\nB\" = \"C\\tD\"\n\"empty\" = \"\"\n\"unterminated = \"x\"\nnot a pair\n\"SEND\" = \"SENDEN\"\n\"\" = \"ignored\"\n\"Level: \" = \"Pegel: \"",
        );
        assert_eq!(s.language, "German");
        assert_eq!(s.countries, ["de", "at"]);
        assert_eq!(s.get("Say \"hi\""), Some("Sag \"hallo\""));
        assert_eq!(s.get("A\nB"), Some("C\tD"));
        assert_eq!(s.get("empty"), None);
        // ignore case: the later pair wins for both spellings
        assert_eq!(s.get("Send"), Some("SENDEN"));
        assert_eq!(s.get("Level: "), Some("Pegel: "));
        assert_eq!(s.get("Level:"), None);
        // the unterminated line: its "English" runs to the next quote, its translation is empty
        assert_eq!(s.get("unterminated = "), None);
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn real_files_parse() {
        for (code, _, _) in LANGUAGES.iter().skip(2) {
            let s = parse(file(code).unwrap());
            assert!(s.len() > 500, "{code}: {} strings", s.len());
            assert!(!s.language.is_empty(), "{code}");
        }
        let de = parse(file("de").unwrap());
        assert_eq!(de.get("Output Level"), Some("Ausgabelautstärke"));
        assert_eq!(
            de.get("Filled in Group from link! Press 'Connect to Group' to join..."),
            Some(
                "Gruppe aus Link übernommen! Drücken Sie 'Mit Gruppe verbinden', um sich anzumelden ..."
            )
        );
        let ja = parse(file("ja").unwrap());
        assert_eq!(ja.language, "Japanese");
        assert!(
            ja.get("Mute All Input")
                .is_some_and(|t| t.chars().any(|c| ('\u{3040}'..='\u{9fff}').contains(&c)))
        );
    }

    #[test]
    fn language_resolution() {
        assert_eq!(resolve("de-de", true), Some("de"));
        assert_eq!(resolve("pt-br", true), Some("pt-br"));
        assert_eq!(resolve("pt", true), Some("pt-pt"));
        assert_eq!(resolve("zh-cn", true), Some("zh-hans"));
        assert_eq!(resolve("zh-tw", true), None);
        assert_eq!(resolve("ja-jp", true), None); // unvetted as a system default
        assert_eq!(resolve("ja", false), Some("ja"));
        assert_eq!(resolve("en-us", false), None);
        assert_eq!(resolve("xx", false), None);
    }

    #[test]
    fn scoped_language_per_thread() {
        let de = std::thread::spawn(|| {
            let _g = scope(Some("de"));
            let inner = {
                let _f = scope(Some("fr"));
                tr("Output Level").to_owned()
            };
            (tr("Output Level").to_owned(), inner, current())
        });
        let (de, fr, cur) = de.join().unwrap();
        assert_eq!(de, "Ausgabelautstärke");
        assert_ne!(fr, de);
        assert_ne!(fr, "Output Level");
        assert_eq!(cur, Some("de"));
        // English scope wins over any process language
        let _e = scope(None);
        assert_eq!(tr("Output Level"), "Output Level");
    }
}
