//! The SonoBus desktop application, stage 1.
//!
//! - [`controller::AppController`]: owns the [`sonobus_engine::Engine`], the audio driver
//!   ([`audio::AudioManager`]: real devices or a "null audio" timer), the embedded connection
//!   server, settings I/O and the autosave timer; exposes a plain-data
//!   [`controller::ViewModel`]. It needs no window: tests, `--headless` and later the Android
//!   app drive it directly. In hosted mode ([`controller::AppController::hosted`]) the plugin
//!   owns and processes the engine instead, and persists through [`controller::Host`].
//! - Feature `desktop` (default): the eframe window (`sonobus` binary). Without it the UI is
//!   plain egui, drawn by any host (the plugin).
//! - [`ui`]: the egui front end. It renders the view model and calls controller (or engine)
//!   commands; it never does I/O itself.
//! - [`settings`], [`links`]: `SonoBus.settings` / `.sonobus` files and `sonobus://` links +
//!   command line.
//! - [`secure`]: secure sessions (protocol v2): what is persisted (device identity, relay
//!   choice, remembered sessions; never in shared files), forms, invite previews, QR codes.

pub mod audio;
pub mod controller;
pub mod i18n;
pub mod links;
pub mod osc_control;
pub mod platform;
pub mod secure;
pub mod settings;
pub mod ui;
pub mod waveform;

/// Translation hook: every user-visible string goes through here. The argument is the English
/// source text, identical to upstream's `TRANS("...")` id where the control exists upstream, so
/// upstream's `localization/*.txt` apply ([`i18n`]; English until [`i18n::init`]).
pub fn tr(s: &str) -> &str {
    i18n::tr(s)
}

/// Text from peers and servers (user, group and public group names, chat, server errors), made
/// safe to show and print: control characters and bidirectional overrides removed, line breaks
/// and tabs collapsed to one space, at most `max` characters (an ellipsis marks the cut). The
/// one place this happens: the controller's view model.
pub fn clean(s: &str, max: usize) -> String {
    let bidi = |c: char| matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{061c}');
    let mut out = String::with_capacity(s.len().min(4 * max));
    let mut n = 0;
    let mut space = false;
    for c in s.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if c.is_control() || bidi(c) || (c == ' ' && space) {
            continue;
        }
        space = c == ' ';
        if n == max {
            out.pop();
            out.push('\u{2026}');
            return out;
        }
        out.push(c);
        n += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn clean_text() {
        use super::clean;
        assert_eq!(clean("bob", 10), "bob");
        // terminal escapes, newline forgery, bidi spoofing
        assert_eq!(
            clean("\u{1b}[31mred\u{1b}]0;title\u{7}", 50),
            "[31mred]0;title"
        );
        assert_eq!(clean("group\n\nAccept? [yes]", 50), "group Accept? [yes]");
        assert_eq!(clean("evil\u{202e}gnp.exe", 50), "evilgnp.exe");
        assert_eq!(clean("abcdef", 4), "abc\u{2026}");
        assert_eq!(clean("abcd", 4), "abcd");
        assert_eq!(clean("zoë 日本", 10), "zoë 日本");
    }
}
