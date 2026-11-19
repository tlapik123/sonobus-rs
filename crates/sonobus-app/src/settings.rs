//! Settings on disk, in upstream's formats (`sonobus_core::files`), in this app's own directory
//! so a real SonoBus install is never overwritten:
//! - Linux: `$XDG_CONFIG_HOME/sonobus-rs/` (default `~/.config/sonobus-rs/`)
//! - macOS: `~/Library/Application Support/SonoBus-RS/`
//! - Windows: `%APPDATA%\SonoBus-RS\`
//! - elsewhere (Android): the embedding app passes a directory.
//!
//! Files: `SonoBus.settings` (state + `audioSetup`) and `GlobalState.xml` (blocked addresses).
//! The audio setup is stored in upstream's `<DEVICESETUP>` element: JUCE's attributes for what
//! maps (`deviceType`, `audioDeviceRate`, `audioDeviceBufferSize`, device names) plus
//! `sonobusRs*` attributes holding cpal device ids, which JUCE ignores.

use std::io;
use std::path::{Path, PathBuf};

use sonobus_core::files::{self, GlobalState, PropertiesFile, StandaloneSettings};
use sonobus_core::juce_xml::{TextFormat, XmlElement};
use sonobus_core::state::AppState;

use crate::audio::{DeviceChoice, DeviceSetup};
use crate::secure;

pub const SETTINGS_FILE: &str = "SonoBus.settings";
pub const GLOBAL_STATE_FILE: &str = "GlobalState.xml";
const RECENT_FILES_KEY: &str = "sonobusRsRecentAudioFiles";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// This app's settings directory.
pub fn app_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(home()?.join("Library/Application Support/SonoBus-RS"))
    } else if cfg!(windows) {
        Some(PathBuf::from(std::env::var_os("APPDATA")?).join("SonoBus-RS"))
    } else if cfg!(target_os = "android") {
        None
    } else {
        let xdg = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()).map(PathBuf::from);
        Some(xdg.or_else(|| Some(home()?.join(".config")))?.join("sonobus-rs"))
    }
}

/// Where stock SonoBus keeps `SonoBus.settings` (see `sonobus_core::files`).
pub fn upstream_settings_file() -> Option<PathBuf> {
    let dir = if cfg!(target_os = "macos") {
        home()?.join("Library/Application Support/SonoBus")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("SonoBus")
    } else {
        home()?.join(".config/sonobus") // JUCE folderName "~/.config/sonobus", not XDG
    };
    Some(dir.join(SETTINGS_FILE))
}

/// Upstream's default record location: `<music dir>/SonoBus` (P.cpp:733-744); on Android
/// (no home) `None`, the caller picks a folder under the settings directory.
pub fn default_record_dir() -> Option<PathBuf> {
    let home = if cfg!(windows) { std::env::var_os("USERPROFILE").map(PathBuf::from) } else { home() };
    Some(home?.join("Music").join("SonoBus"))
}

/// `file://` URL of a local path (JUCE `URL (File)`: path bytes outside `A-Za-z0-9-_.~/:`
/// percent-escaped).
pub fn file_url(path: &Path) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    let mut s = String::from(if p.starts_with('/') { "file://" } else { "file:///" });
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/:".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// The local path of a `file://` URL (`None` for anything else).
pub fn url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    let p = String::from_utf8_lossy(&out).into_owned();
    // "file:///C:/x" on Windows
    let p = if cfg!(windows) { p.trim_start_matches('/').to_owned() } else { p };
    Some(PathBuf::from(p))
}

pub const SOUNDBOARDS_FILE: &str = "soundboards.xml";

pub fn load_soundboards(path: &Path) -> io::Result<files::Soundboards> {
    files::Soundboards::parse(&read_capped(path)?).map_err(invalid)
}

pub fn save_soundboards(path: &Path, s: &files::Soundboards) -> io::Result<()> {
    write_atomic(path, s.to_xml_string().as_bytes())
}

/// Creates `dir` (and parents); on Unix the ones created here are private (0700): the
/// settings hold passwords in clear text.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

/// Writes via a private temporary file (`.<name>.<pid>.<n>.tmp`, created new, 0600 on Unix),
/// flushed to disk, then renamed over `path`: a crash never leaves a truncated file, and two
/// writers never share a temporary file or touch a user's own `X.tmp`. The result is 0600 on
/// Unix (settings and setups can hold passwords).
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let name = path.file_name().map_or("file".into(), |n| n.to_string_lossy().into_owned());
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = dir.join(format!(".{name}.{}.{}{nanos}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let res = o.open(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    match res.and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Settings that belong to this machine and its user: never read from nor written to setup
/// files, stock-settings imports or a plugin's project (files that travel between people),
/// only our own settings file. OSC remote control (exposure: a listening port, a feedback
/// target), the UDP port, the record folder, the last browse folder, the language and the
/// recent connections (they hold group passwords), and the endpoint ids of the people met in
/// secure sessions (the engine's `ModernPeerNames`). The user name stays in setups: upstream's
/// carry it and users expect it. (Secure-session secrets, the device identity included, are
/// not in the state at all: [`crate::secure`].)
pub fn keep_local(incoming: &mut AppState, local: &AppState) {
    let (e, l) = (&mut incoming.extra, &local.extra);
    e.use_udp_port = l.use_udp_port;
    e.default_record_dir_url.clone_from(&l.default_record_dir_url);
    e.default_record_dir.clone_from(&l.default_record_dir);
    e.last_browse_dir.clone_from(&l.last_browse_dir);
    e.lang_override_code.clone_from(&l.lang_override_code);
    let local_key = |k: &str| k.starts_with("OSC") || k.starts_with("Modern") || k.starts_with("sonobusRsSecure") || k == secure::IDENTITY_KEY;
    e.unknown.retain(|(k, _)| !local_key(k));
    e.unknown.extend(l.unknown.iter().filter(|(k, _)| local_key(k)).cloned());
    incoming.recents.clone_from(&local.recents);
}

/// `state` without what [`keep_local`] keeps on this machine (defaults instead).
pub fn shareable(state: &AppState) -> AppState {
    let mut s = state.clone();
    keep_local(&mut s, &AppState::new(state.is_plugin));
    s
}

/// The settings directory's instance lock (`instance.lock`, an OS file lock: released when
/// the process ends, crash included). Only the holder writes the settings files; a second
/// instance on the same directory reads them and saves nothing.
pub struct InstanceLock(#[allow(dead_code)] std::fs::File);

impl InstanceLock {
    /// `None`: another instance holds the directory (or the lock cannot be taken).
    pub fn acquire(dir: &Path) -> Option<InstanceLock> {
        create_private_dir(dir).ok()?;
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("instance.lock")).ok()?;
        f.try_lock().ok()?;
        Some(InstanceLock(f))
    }
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let len = std::fs::metadata(path)?.len();
    if len > files::MAX_FILE_SIZE as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    std::fs::read(path)
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

// ---- audio setup <-> DEVICESETUP --------------------------------------------------------------

pub fn device_setup_xml(s: &DeviceSetup) -> String {
    let mut e = XmlElement::new("DEVICESETUP");
    if let Some(h) = &s.host {
        e.set_attribute("deviceType", h.clone());
    }
    let name = |c: &DeviceChoice| match c {
        DeviceChoice::Id(id) => id.split_once(':').map_or(id.clone(), |(_, d)| d.to_owned()),
        _ => String::new(),
    };
    e.set_attribute("audioOutputDeviceName", name(&s.output));
    e.set_attribute("audioInputDeviceName", name(&s.input));
    e.set_attribute("audioDeviceRate", s.sample_rate.to_string());
    e.set_attribute("audioDeviceBufferSize", s.block_size.to_string());
    e.set_attribute("sonobusRsEnabled", if s.enabled { "1" } else { "0" });
    e.set_attribute("sonobusRsOutput", s.output.to_setting());
    e.set_attribute("sonobusRsInput", s.input.to_setting());
    if let Some(c) = s.input_channels {
        e.set_attribute("sonobusRsInputChannels", c.to_string());
    }
    if let Some(l) = &s.system_sound {
        e.set_attribute("sonobusRsSystemSound", l.clone());
    }
    if let Some(b) = s.buffer_frames {
        e.set_attribute("sonobusRsBufferFrames", b.to_string());
    }
    e.to_xml_string(TextFormat::SINGLE_LINE_NO_HEADER)
}

/// Applies a stored `<DEVICESETUP>`: everything when we wrote it; from a stock file only the
/// rate and buffer size (JUCE device names do not map to cpal devices).
pub fn apply_device_setup_xml(xml: &str, s: &mut DeviceSetup) {
    let Ok(e) = XmlElement::parse(xml) else { return };
    let num = |k: &str| e.attribute(k).and_then(|v| v.trim().parse::<u32>().ok()).filter(|&v| v > 0);
    if let Some(r) = num("audioDeviceRate") {
        s.sample_rate = r;
    }
    if let Some(b) = num("audioDeviceBufferSize") {
        s.block_size = b.clamp(16, 4096) as usize;
    }
    let Some(enabled) = e.attribute("sonobusRsEnabled") else { return };
    s.enabled = enabled != "0";
    s.host = e.attribute("deviceType").filter(|h| !h.is_empty()).map(str::to_owned);
    s.output = DeviceChoice::from_setting(e.attribute("sonobusRsOutput").unwrap_or_default());
    s.input = DeviceChoice::from_setting(e.attribute("sonobusRsInput").unwrap_or_default());
    s.input_channels = num("sonobusRsInputChannels").map(|c| c.min(64) as u16);
    s.system_sound = e.attribute("sonobusRsSystemSound").filter(|v| !v.is_empty()).map(str::to_owned);
    s.buffer_frames = num("sonobusRsBufferFrames");
}

// ---- the store --------------------------------------------------------------------------------

/// Everything loaded from the settings directory.
pub struct Loaded {
    pub state: AppState,
    pub setup: DeviceSetup,
    pub blocked: Vec<String>,
    /// Problems worth showing (unreadable file, skipped state).
    pub warnings: Vec<String>,
}

/// The settings directory, with the loaded `SonoBus.settings` kept so unknown keys survive.
pub struct Store {
    pub dir: PathBuf,
    /// `SonoBus.settings` as loaded (unknown keys survive); our own keys too ([`crate::secure`]).
    pub props: PropertiesFile,
    pub standalone: StandaloneSettings,
    global: GlobalState,
}

impl Store {
    /// Loads whatever exists in `dir` (nothing is an empty, default state).
    pub fn open(dir: PathBuf) -> (Store, Loaded) {
        let mut loaded = Loaded {
            state: AppState::new(false),
            setup: DeviceSetup::default(),
            blocked: Vec::new(),
            warnings: Vec::new(),
        };
        let mut store = Store {
            props: PropertiesFile::default(),
            standalone: StandaloneSettings::default(),
            global: GlobalState::default(),
            dir,
        };
        let path = store.dir.join(SETTINGS_FILE);
        match read_capped(&path) {
            Ok(data) => match files::load_settings_checked(&data, &mut loaded.state) {
                Ok((p, s, err)) => {
                    if let Some(a) = &s.audio_setup {
                        apply_device_setup_xml(a, &mut loaded.setup);
                    }
                    if let Some(e) = err {
                        loaded.warnings.push(format!("{}: {e}", path.display()));
                    }
                    (store.props, store.standalone) = (p, s);
                }
                Err(e) => loaded.warnings.push(format!("{}: {e}", path.display())),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => loaded.warnings.push(format!("{}: {e}", path.display())),
        }
        if let Ok(data) = read_capped(&store.dir.join(GLOBAL_STATE_FILE)) {
            match GlobalState::parse(&data) {
                Ok(g) => store.global = g,
                Err(e) => loaded.warnings.push(format!("{GLOBAL_STATE_FILE}: {e}")),
            }
        }
        loaded.blocked = store.global.blocked_addresses.clone();
        (store, loaded)
    }

    pub fn save(&mut self, state: &AppState, setup: &DeviceSetup, blocked: Vec<String>) -> io::Result<()> {
        self.files(state, setup, blocked).into_iter().try_for_each(|(p, d)| write_atomic(&p, &d))
    }

    /// What [`Self::save`] writes, for writing elsewhere (an autosave worker).
    pub fn files(&mut self, state: &AppState, setup: &DeviceSetup, blocked: Vec<String>) -> Vec<(PathBuf, Vec<u8>)> {
        self.standalone.audio_setup = Some(device_setup_xml(setup));
        let text = files::save_settings(&mut self.props, &self.standalone, state);
        let mut out = vec![(self.dir.join(SETTINGS_FILE), text.into_bytes())];
        if blocked != self.global.blocked_addresses || !self.dir.join(GLOBAL_STATE_FILE).exists() {
            self.global.blocked_addresses = blocked;
            out.push((self.dir.join(GLOBAL_STATE_FILE), self.global.to_xml_string().into_bytes()));
        }
        out
    }

    /// Recently loaded audio files, newest first (our key; stock has no such list).
    pub fn recent_files(&self) -> Vec<String> {
        self.props.get(RECENT_FILES_KEY).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(str::to_owned).collect()
    }

    pub fn add_recent_file(&mut self, path: &Path) {
        let p = path.to_string_lossy().into_owned();
        let mut list = self.recent_files();
        list.retain(|x| *x != p);
        list.insert(0, p);
        list.truncate(12);
        self.props.set(RECENT_FILES_KEY, list.join("\n"));
    }

    /// Remembers a setup file in the recents list (oldest first, max 12, as upstream).
    pub fn add_recent_setup(&mut self, path: &Path) {
        let p = path.to_string_lossy().into_owned();
        let list = &mut self.standalone.recent_setup_files;
        list.retain(|x| *x != p);
        list.push(p);
        let n = list.len();
        if n > 12 {
            list.drain(..n - 12);
        }
        if let Some(dir) = path.parent() {
            self.standalone.last_recents_setup_folder = dir.to_string_lossy().into_owned();
        }
    }
}

/// Reads a stock `SonoBus.settings` onto a fresh state ("Import settings from SonoBus").
pub fn import_settings(path: &Path) -> io::Result<(AppState, DeviceSetup)> {
    let data = read_capped(path)?;
    let mut state = AppState::new(false);
    let (_, s, err) = files::load_settings_checked(&data, &mut state).map_err(invalid)?;
    if let Some(e) = err {
        return Err(invalid(e));
    }
    let mut setup = DeviceSetup::default();
    if let Some(a) = &s.audio_setup {
        apply_device_setup_xml(a, &mut setup);
    }
    Ok((state, setup))
}

/// Loads a `.sonobus` setup onto `state` (and `setup` when it carries one).
pub fn load_setup_file(path: &Path, state: &mut AppState, setup: &mut DeviceSetup) -> io::Result<()> {
    let data = read_capped(path)?;
    let (audio, err) = files::load_setup_checked(&data, state).map_err(invalid)?;
    if let Some(e) = err {
        return Err(invalid(e));
    }
    if let Some(a) = audio {
        apply_device_setup_xml(&a, setup);
    }
    Ok(())
}

/// Writes a `.sonobus` setup: without what stays on this machine ([`keep_local`]).
pub fn save_setup_file(path: &Path, state: &AppState, setup: &DeviceSetup) -> io::Result<()> {
    write_atomic(path, files::save_setup(&shareable(state), Some(&device_setup_xml(setup))).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sonobus-app-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn device_setup_round_trip() {
        let s = DeviceSetup {
            enabled: true,
            host: Some("alsa".into()),
            output: DeviceChoice::Id("alsa:hw:1,0".into()),
            input: DeviceChoice::None,
            input_channels: Some(4),
            system_sound: Some("alsa:hw:2,0".into()),
            external: None, // not persisted
            sample_rate: 44100,
            block_size: 128,
            buffer_frames: Some(64),
        };
        let mut back = DeviceSetup::default();
        apply_device_setup_xml(&device_setup_xml(&s), &mut back);
        assert_eq!(back, s);
        // a stock DEVICESETUP: rate and buffer only
        let mut d = DeviceSetup::default();
        apply_device_setup_xml(
            r#"<DEVICESETUP deviceType="ALSA" audioOutputDeviceName="x" audioDeviceRate="96000" audioDeviceBufferSize="512"/>"#,
            &mut d,
        );
        assert_eq!(d, DeviceSetup { sample_rate: 96000, block_size: 512, ..DeviceSetup::default() });
    }

    #[test]
    fn settings_round_trip_on_disk() {
        let dir = temp_dir("store");
        let (mut store, loaded) = Store::open(dir.clone());
        assert!(loaded.warnings.is_empty());
        let mut state = loaded.state;
        state.params.wet = 0.5;
        state.params.met_tempo = 133.0;
        state.extra.last_username = "Zoë".into();
        state.extra.unknown.push(("forkKey".into(), sonobus_core::Var::String("kept".into())));
        let setup = DeviceSetup { sample_rate: 44100, enabled: false, ..DeviceSetup::default() };
        store.save(&state, &setup, vec!["10.1.2.3".into()]).unwrap();

        let (_, l) = Store::open(dir.clone());
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!(l.state.params.wet, 0.5);
        assert_eq!(l.state.params.met_tempo, 133.0);
        assert_eq!(l.state.extra.last_username, "Zoë");
        assert_eq!(l.state.extra.unknown, state.extra.unknown);
        assert_eq!(l.setup, setup);
        assert_eq!(l.blocked, vec!["10.1.2.3".to_string()]);

        // setup file round trip, and import of a settings file
        let f = dir.join("my.sonobus");
        save_setup_file(&f, &state, &setup).unwrap();
        let (mut st2, mut su2) = (AppState::new(false), DeviceSetup::default());
        load_setup_file(&f, &mut st2, &mut su2).unwrap();
        assert_eq!((st2.params.wet, su2.sample_rate), (0.5, 44100));
        let (imported, _) = import_settings(&dir.join(SETTINGS_FILE)).unwrap();
        assert_eq!(imported.params.met_tempo, 133.0);
        assert!(import_settings(&dir.join("missing")).is_err());

        // a corrupt file is reported, not fatal
        std::fs::write(dir.join(SETTINGS_FILE), b"garbage").unwrap();
        let (_, l) = Store::open(dir.clone());
        assert_eq!(l.warnings.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_urls_and_soundboards() {
        let p = Path::new("/home/zoë/Music/Sono Bus/a%b.wav");
        let u = file_url(p);
        assert_eq!(u, "file:///home/zo%C3%AB/Music/Sono%20Bus/a%25b.wav");
        assert_eq!(url_path(&u).unwrap(), p);
        assert_eq!(url_path("https://x"), None);
        let dir = temp_dir("boards");
        let mut b = files::Soundboards::default();
        b.boards.push(files::Soundboard { name: "Jingles".into(), samples: vec![files::SoundSample { name: "a".into(), file_url: u, ..Default::default() }] });
        save_soundboards(&dir.join(SOUNDBOARDS_FILE), &b).unwrap();
        assert_eq!(load_soundboards(&dir.join(SOUNDBOARDS_FILE)).unwrap().boards, b.boards);
        let (mut store, _) = Store::open(dir.clone());
        store.add_recent_file(Path::new("/a.wav"));
        store.add_recent_file(Path::new("/b.wav"));
        store.add_recent_file(Path::new("/a.wav"));
        assert_eq!(store.recent_files(), ["/a.wav", "/b.wav"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_setups_capped() {
        let (mut store, _) = Store::open(temp_dir("recents"));
        for i in 0..15 {
            store.add_recent_setup(Path::new(&format!("/x/{i}.sonobus")));
        }
        store.add_recent_setup(Path::new("/x/5.sonobus"));
        let l = &store.standalone.recent_setup_files;
        assert_eq!(l.len(), 12);
        assert_eq!(l.last().unwrap(), "/x/5.sonobus");
        assert_eq!(store.standalone.last_recents_setup_folder, "/x");
    }

    #[test]
    fn setup_files_leave_machine_settings_out() {
        let dir = temp_dir("local");
        let mut mine = AppState::new(false);
        mine.params.wet = 0.5;
        mine.extra.last_username = "zoe".into();
        mine.extra.use_udp_port = 12000;
        mine.extra.lang_override_code = "de".into();
        mine.extra.default_record_dir_url = "file:///home/zoe/rec".into();
        mine.extra.unknown.push(("OSCTargetIPAddress".into(), sonobus_core::Var::String("10.9.9.9".into())));
        mine.extra.unknown.push(("OSCEnabled".into(), sonobus_core::Var::Bool(true)));
        mine.extra.unknown.push(("forkKey".into(), sonobus_core::Var::Bool(true)));
        mine.extra.unknown.push(("ModernPeerNames".into(), sonobus_core::Var::String("ab12\u{1f}bob".into())));
        mine.recents.push(Default::default());
        let f = dir.join("shared.sonobus");
        save_setup_file(&f, &mine, &DeviceSetup::default()).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(!text.contains("10.9.9.9") && !text.contains("OSCEnabled") && !text.contains("12000"), "{text}");
        assert!(!text.contains("home/zoe/rec"));
        assert!(!text.contains("ModernPeerNames") && !text.contains("ab12"), "who we met stays here");
        // loading someone's file keeps ours: their OSC, port, folder, language never apply
        let mut theirs = AppState::new(false);
        theirs.params.wet = 0.25;
        theirs.extra.last_username = "mallory".into();
        theirs.extra.use_udp_port = 1;
        theirs.extra.lang_override_code = "ja".into();
        theirs.extra.unknown.push(("OSCEnabled".into(), sonobus_core::Var::Bool(true)));
        theirs.extra.unknown.push(("OSCAllowNetwork".into(), sonobus_core::Var::Bool(true)));
        let raw = dir.join("theirs.sonobus");
        write_atomic(&raw, files::save_setup(&theirs, None).as_bytes()).unwrap();
        let mut st = mine.clone();
        let mut su = DeviceSetup::default();
        load_setup_file(&raw, &mut st, &mut su).unwrap();
        keep_local(&mut st, &mine);
        assert_eq!((st.params.wet, st.extra.last_username.as_str()), (0.25, "mallory"));
        assert_eq!((st.extra.use_udp_port, st.extra.lang_override_code.as_str()), (12000, "de"));
        assert!(!st.extra.unknown.iter().any(|(k, _)| k == "OSCAllowNetwork"));
        assert!(st.extra.unknown.contains(&("OSCTargetIPAddress".into(), sonobus_core::Var::String("10.9.9.9".into()))));
        assert_eq!(st.recents.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_writes_are_private_and_leave_neighbours() {
        let dir = temp_dir("atomic");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("X.tmp"), b"mine").unwrap();
        write_atomic(&dir.join("X.sonobus"), b"one").unwrap();
        write_atomic(&dir.join("X.sonobus"), b"two").unwrap();
        assert_eq!(std::fs::read(dir.join("X.tmp")).unwrap(), b"mine", "a user's X.tmp survives");
        assert_eq!(std::fs::read(dir.join("X.sonobus")).unwrap(), b"two");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 2, "no temporary files left: {left:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("X.sonobus")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let sub = dir.join("new/inner");
            create_private_dir(&sub).unwrap();
            assert_eq!(std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // the instance lock: one holder at a time, free again when dropped
        let a = InstanceLock::acquire(&dir).expect("first");
        assert!(InstanceLock::acquire(&dir).is_none(), "second instance");
        drop(a);
        assert!(InstanceLock::acquire(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirs_are_distinct_from_upstream() {
        if let (Some(a), Some(u)) = (app_dir(), upstream_settings_file()) {
            assert_ne!(a.join(SETTINGS_FILE), u);
        }
    }
}
