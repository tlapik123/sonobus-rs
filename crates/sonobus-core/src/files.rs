//! Loaders and savers for SonoBus 1.7.2's on-disk files, as pure functions over bytes/strings.
//!
//! Where upstream keeps them. The support directory is the parent of a JUCE
//! `PropertiesFile::Options::getDefaultFile()` with `applicationName "SonoBus"`,
//! `osxLibrarySubFolder "Application Support/SonoBus"`, `folderName "~/.config/sonobus"` on Linux
//! and empty elsewhere (SonobusPluginProcessor.cpp:740-751, SonoStandaloneFilterApp.cpp:82-93,
//! juce_PropertiesFile.cpp:56-110):
//! - Linux: `~/.config/sonobus/` (before 1.3.19 the settings were `~/.config/SonoBus.settings`;
//!   moved once at startup, SonoStandaloneFilterApp.cpp:95-105)
//! - macOS and iOS: `~/Library/Application Support/SonoBus/`
//! - Windows: `%APPDATA%\SonoBus\`
//! - Android: `~/.SonoBus/` (`~` = the app's data dir)
//!
//! Files there:
//! - `SonoBus.settings` (standalone only): [`PropertiesFile`] + [`StandaloneSettings`].
//!   A `SENTINEL` file next to it marks a crashed launch; the next launch renames the settings
//!   to `POSSIBLY_BAD_<timestamp>` (SonoStandaloneFilterApp.cpp:147-160).
//! - `PluginDefault.xml`: [`save_plugin_default`] (P.cpp:8805-8835).
//! - `GlobalState.xml`: [`GlobalState`] (P.cpp:8990-9011).
//! - `soundboards.xml`: [`Soundboards`] (SoundboardProcessor.cpp:13, 295-332).
//!
//! `*.sonobus` setup files live wherever the user saves them (default folder: the last one used,
//! `lastRecentsSetupFolder`): [`load_setup`]/[`save_setup`] (SonobusPluginEditor.cpp:2638-2793).
//! Hosts keep the plugin state themselves: it is the binary [`ValueTree`]
//! (`ValueTree::read` + [`AppState::apply_tree`] with [`Scope::FULL`]).

use crate::juce_xml::{
    Error, TextFormat, XmlElement, XmlNode, escape, from_base64, juce_int, to_base64, var_to_string,
};
use crate::state::{AppState, Scope, int_enum};
use crate::valuetree::{ValueTree, Var, set_all};
use std::borrow::Cow;

/// Largest file the loaders accept ([`PropertiesFile::parse`], [`load_plugin_default`],
/// [`GlobalState::parse`], [`Soundboards::parse`]); callers should not read more than this.
/// Stock has no limit. A channel group is about 1.6 KB of XML, so a `SonoBus.settings` holding
/// 64 input groups and 300 cached peers at the stock maximum of 64+64 groups each stays under
/// it; real files are well below 1 MB. The cap bounds parse time and memory (a parsed tree
/// takes several times its text size).
pub const MAX_FILE_SIZE: usize = 64 << 20;

/// File bytes as text (invalid UTF-8 replaced), refusing files above [`MAX_FILE_SIZE`].
fn file_text(data: &[u8]) -> Result<Cow<'_, str>, Error> {
    if data.len() > MAX_FILE_SIZE {
        return Err(Error::Xml("file larger than MAX_FILE_SIZE"));
    }
    Ok(String::from_utf8_lossy(data))
}

/// A JUCE `PropertiesFile` stored as XML (`storeAsXML`, the default every SonoBus file uses):
/// `<PROPERTIES><VALUE name=".." val=".."/>...` with values that parse as XML stored as a child
/// element instead (juce_PropertiesFile.cpp:20-28, 170-215). Keys keep their file order; unknown
/// keys survive a load/save unchanged.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PropertiesFile {
    pub entries: Vec<(String, String)>,
}

impl PropertiesFile {
    /// `PropertiesFile::loadAsXml`. JUCE's binary `PROP`/`CPRP` storage formats are not
    /// supported (SonoBus never uses them) and give an error, like any non-`PROPERTIES` root.
    /// A repeated key keeps its first position and takes the last value (`StringPairArray::set`).
    // ponytail: binary PROP/CPRP formats skipped; add if a fork ever switches storageFormat.
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let doc = XmlElement::parse(&file_text(data)?)?;
        if doc.name != "PROPERTIES" {
            return Err(Error::WrongRoot(doc.name));
        }
        let mut pairs = Vec::new();
        for e in doc.child_elements().filter(|e| e.name == "VALUE") {
            let name = e.attribute("name").unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let value = match e.children.first() {
                Some(XmlNode::Element(x)) => x.to_xml_string(TextFormat::SINGLE_LINE_NO_HEADER),
                Some(XmlNode::Text(t)) => {
                    let mut s = String::new();
                    escape(&mut s, t, false); // a text element's toString()
                    s
                }
                None => e.attribute("val").unwrap_or_default().to_string(),
            };
            pairs.push((name.to_string(), value));
        }
        Ok(PropertiesFile {
            entries: set_all(pairs),
        })
    }

    /// `PropertiesFile::saveAsXml` + `XmlElement::writeTo(file, {})`.
    pub fn to_xml_string(&self) -> String {
        let mut doc = XmlElement::new("PROPERTIES");
        for (k, v) in &self.entries {
            let mut e = XmlElement::new("VALUE");
            e.set_attribute("name", k.clone());
            match XmlElement::parse(v) {
                Ok(x) => e.children.push(XmlNode::Element(x)),
                Err(_) => e.set_attribute("val", v.clone()),
            }
            doc.children.push(XmlNode::Element(e));
        }
        doc.to_xml_string(TextFormat::DEFAULT)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// `PropertySet::setValue`: replaces in place or appends (linear in the key count).
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value,
            None => self.entries.push((key.into(), value)),
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.retain(|(k, _)| k != key);
    }

    /// `PropertySet::getBoolValue`: `getIntValue() != 0`.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).map(|v| juce_int(v) as i32 != 0)
    }

    /// `PropertySet::getIntValue`.
    pub fn get_int(&self, key: &str) -> Option<i32> {
        self.get(key).map(|v| juce_int(v) as i32)
    }

    /// `PropertySet::setValue(key, const XmlElement*)`: single line, no header; `None` stores "".
    pub fn set_xml(&mut self, key: &str, xml: Option<&XmlElement>) {
        let v = xml.map(|x| x.to_xml_string(TextFormat::SINGLE_LINE_NO_HEADER));
        self.set(key, v.unwrap_or_default());
    }

    /// The processor state tree: `filterStateXML` (XML) if present and non-empty, else the legacy
    /// `filterState` (JUCE base64 of the binary tree) (SonoStandaloneFilterWindow.h:489-506).
    /// `Ok(None)` when neither key holds anything. The loaders skip a state that fails here, as
    /// stock does; this is where to find out why.
    pub fn processor_state(&self) -> Result<Option<ValueTree>, Error> {
        if let Some(xml) = self.get("filterStateXML").filter(|s| !s.is_empty()) {
            return ValueTree::from_xml_str(xml).map(Some);
        }
        match self.get("filterState").filter(|s| !s.is_empty()) {
            Some(b64) => Ok(Some(ValueTree::read(
                &from_base64(b64).ok_or(Error::Base64)?,
            )?)),
            None => Ok(None),
        }
    }

    /// Stores the state as `filterStateXML` and drops the legacy `filterState`, as
    /// `savePluginState` does (SonoStandaloneFilterWindow.h:461-476).
    pub fn set_processor_state(&mut self, tree: &ValueTree) {
        self.set_xml("filterStateXML", tree.to_xml().as_ref());
        self.remove("filterState");
    }

    /// Stores the state the legacy way: `filterState` = JUCE base64 of the binary tree
    /// (what stock writes only when its XML round trip fails).
    pub fn set_processor_state_binary(&mut self, tree: &ValueTree) {
        let mut bytes = Vec::new();
        tree.write(&mut bytes);
        self.set("filterState", to_base64(&bytes));
    }
}

/// The keys the standalone app keeps in `SonoBus.settings` besides the processor state
/// (SonoStandaloneFilterWindow.h:200-220, 346-415, 878-921). Read with [`Self::load`], write back
/// into the same [`PropertiesFile`] with [`Self::store`] so keys not listed here (and the key
/// order) survive.
#[derive(Debug, Clone, PartialEq)]
pub struct StandaloneSettings {
    /// `audioSetup`: JUCE `AudioDeviceManager::createStateXml()` (`<DEVICESETUP .../>`), kept as
    /// the single-line XML string, verbatim. Stock drops `audioDeviceRate` from it on load unless
    /// `should_override_sample_rate`.
    pub audio_setup: Option<String>,
    /// `shouldOverrideSampleRate` (default true)
    pub should_override_sample_rate: bool,
    /// `shouldCheckForNewVersion` (default true)
    pub should_check_for_new_version: bool,
    /// `allowBluetoothInput` (default false; iOS)
    pub allow_bluetooth_input: bool,
    /// `recentSetupFiles`: `<PATHLIST><PATH>file</PATH>...`, oldest first, max 12
    /// (SonobusPluginEditor.cpp:2722-2740).
    pub recent_setup_files: Vec<String>,
    /// `lastRecentsSetupFolder`
    pub last_recents_setup_folder: String,
    /// `lastStateFile`: last file chosen in the stock load/save state dialogs.
    pub last_state_file: Option<String>,
    /// `windowX`, `windowY`, `windowW`, `windowH` (desktop only; -100 = unset in upstream)
    pub window: [Option<i32>; 4],
    /// `shouldMuteInput`: read by stock but never written by 1.7.2 (the writer is commented out);
    /// [`Self::store`] leaves the key alone.
    pub should_mute_input: Option<bool>,
}

impl Default for StandaloneSettings {
    fn default() -> Self {
        StandaloneSettings {
            audio_setup: None,
            should_override_sample_rate: true,
            should_check_for_new_version: true,
            allow_bluetooth_input: false,
            recent_setup_files: Vec::new(),
            last_recents_setup_folder: String::new(),
            last_state_file: None,
            window: [None; 4],
            should_mute_input: None,
        }
    }
}

const WINDOW_KEYS: [&str; 4] = ["windowX", "windowY", "windowW", "windowH"];

impl StandaloneSettings {
    pub fn load(p: &PropertiesFile) -> Self {
        let d = StandaloneSettings::default();
        let recent_setup_files = p
            .get("recentSetupFiles")
            .and_then(|x| XmlElement::parse(x).ok())
            .map(|list| {
                list.child_elements()
                    .filter_map(|c| match c.children.first() {
                        Some(XmlNode::Text(t)) => Some(t.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        StandaloneSettings {
            audio_setup: p.get("audioSetup").map(str::to_string),
            should_override_sample_rate: p
                .get_bool("shouldOverrideSampleRate")
                .unwrap_or(d.should_override_sample_rate),
            should_check_for_new_version: p
                .get_bool("shouldCheckForNewVersion")
                .unwrap_or(d.should_check_for_new_version),
            allow_bluetooth_input: p
                .get_bool("allowBluetoothInput")
                .unwrap_or(d.allow_bluetooth_input),
            recent_setup_files,
            last_recents_setup_folder: p
                .get("lastRecentsSetupFolder")
                .unwrap_or_default()
                .to_string(),
            last_state_file: p.get("lastStateFile").map(str::to_string),
            window: WINDOW_KEYS.map(|k| p.get_int(k)),
            should_mute_input: p.get_bool("shouldMuteInput"),
        }
    }

    /// Writes the keys in the order stock sets them on a fresh file (`saveAudioDeviceState`
    /// then the window position); existing keys are updated in place.
    pub fn store(&self, p: &mut PropertiesFile) {
        p.set("audioSetup", self.audio_setup.clone().unwrap_or_default());
        let b = |v: bool| if v { "1" } else { "0" };
        p.set(
            "shouldOverrideSampleRate",
            b(self.should_override_sample_rate),
        );
        p.set(
            "shouldCheckForNewVersion",
            b(self.should_check_for_new_version),
        );
        p.set("allowBluetoothInput", b(self.allow_bluetooth_input));
        let mut list = XmlElement::new("PATHLIST");
        for f in &self.recent_setup_files {
            let mut path = XmlElement::new("PATH");
            path.children.push(XmlNode::Text(f.clone()));
            list.children.push(XmlNode::Element(path));
        }
        p.set_xml("recentSetupFiles", Some(&list));
        p.set(
            "lastRecentsSetupFolder",
            self.last_recents_setup_folder.clone(),
        );
        if let Some(f) = &self.last_state_file {
            p.set("lastStateFile", f.clone());
        }
        for (k, v) in WINDOW_KEYS.iter().zip(self.window) {
            if let Some(v) = v {
                p.set(k, v.to_string());
            }
        }
    }
}

/// Reads a whole `SonoBus.settings`: the keyed settings, and the processor state applied with
/// [`Scope::FULL`] onto `state` (when the file has one). Keep the returned [`PropertiesFile`] to
/// write back with [`save_settings`]. Fails only when the file itself is unreadable: like stock
/// (SonoStandaloneFilterWindow.h:489-506), an unreadable embedded state leaves `state` as it was
/// and everything else still loads; [`load_settings_checked`] also returns why.
pub fn load_settings(
    data: &[u8],
    state: &mut AppState,
) -> Result<(PropertiesFile, StandaloneSettings), Error> {
    load_settings_checked(data, state).map(|(p, s, _)| (p, s))
}

/// [`load_settings`], plus the error of an embedded processor state that was skipped.
pub fn load_settings_checked(
    data: &[u8],
    state: &mut AppState,
) -> Result<(PropertiesFile, StandaloneSettings, Option<Error>), Error> {
    let p = PropertiesFile::parse(data)?;
    let err = apply_processor_state(&p, state, Scope::FULL);
    let s = StandaloneSettings::load(&p);
    Ok((p, s, err))
}

fn apply_processor_state(p: &PropertiesFile, state: &mut AppState, scope: Scope) -> Option<Error> {
    match p.processor_state() {
        Ok(Some(tree)) => state.apply_tree(&tree, scope),
        Ok(None) => {}
        Err(e) => return Some(e),
    }
    None
}

/// Updates `props` (from [`load_settings`], or empty for a new file) and serializes it, setting
/// keys in stock order: device/options keys, `filterStateXML`, window position.
pub fn save_settings(
    props: &mut PropertiesFile,
    settings: &StandaloneSettings,
    state: &AppState,
) -> String {
    let window = settings.window;
    StandaloneSettings {
        window: [None; 4],
        ..settings.clone()
    }
    .store(props);
    props.set_processor_state(&state.to_tree());
    for (k, v) in WINDOW_KEYS.iter().zip(window) {
        if let Some(v) = v {
            props.set(k, v.to_string());
        }
    }
    props.to_xml_string()
}

/// Loads a `.sonobus` setup file onto `state` the way the stock "Load Setup" does
/// (SonobusPluginEditor.cpp:2638-2690): input groups and everything else, but not recents or the
/// peer cache. Returns the `audioSetup` device XML string, if any. An unreadable embedded state
/// is skipped and the device setup still returned, as stock does; [`load_setup_checked`] also
/// returns why.
pub fn load_setup(data: &[u8], state: &mut AppState) -> Result<Option<String>, Error> {
    load_setup_checked(data, state).map(|(audio, _)| audio)
}

/// [`load_setup`], plus the error of an embedded processor state that was skipped.
pub fn load_setup_checked(
    data: &[u8],
    state: &mut AppState,
) -> Result<(Option<String>, Option<Error>), Error> {
    let p = PropertiesFile::parse(data)?;
    let err = apply_processor_state(&p, state, Scope::SETUP_FILE);
    Ok((p.get("audioSetup").map(str::to_string), err))
}

/// Writes a `.sonobus` setup file as the stock "Save Setup" does (SonobusPluginEditor.cpp:
/// 2758-2793): `audioSetup` then `filterStateXML` (no recents, no peer cache).
pub fn save_setup(state: &AppState, audio_setup: Option<&str>) -> String {
    let mut p = PropertiesFile::default();
    p.set("audioSetup", audio_setup.unwrap_or_default());
    p.set_xml(
        "filterStateXML",
        state.to_tree_scoped(Scope::SETUP_FILE).to_xml().as_ref(),
    );
    p.to_xml_string()
}

/// Loads `PluginDefault.xml` onto `state` (P.cpp:8815-8825): XML, no cache, no input groups.
pub fn load_plugin_default(data: &[u8], state: &mut AppState) -> Result<(), Error> {
    let tree = ValueTree::from_xml_str(&file_text(data)?)?;
    state.apply_tree(&tree, Scope::PLUGIN_DEFAULT);
    Ok(())
}

/// `PluginDefault.xml` bytes (P.cpp:8805-8812): `toXmlString()` written with
/// `OutputStream::writeString`, which appends a NUL.
pub fn save_plugin_default(state: &AppState) -> Vec<u8> {
    let mut b = state
        .to_tree_scoped(Scope::PLUGIN_DEFAULT)
        .to_xml_string()
        .into_bytes();
    b.push(0);
    b
}

/// `GlobalState.xml`: the `SonobusGlobalState` tree (P.cpp:8990-9085).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GlobalState {
    /// `BlockedAddresses/Address@value`: blocked peer IP addresses, in insertion order.
    pub blocked_addresses: Vec<String>,
    /// Other root children, kept as loaded.
    pub other: Vec<ValueTree>,
}

impl GlobalState {
    /// Stock dereferences a null document on a malformed file (P.cpp:9000); we return the error.
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let tree = ValueTree::from_xml_str(&file_text(data)?)?;
        let blocked = tree.child_with_name("BlockedAddresses");
        Ok(GlobalState {
            blocked_addresses: blocked
                .map(|b| {
                    b.children
                        .iter()
                        .filter_map(|c| match c.get("value") {
                            Some(Var::String(s)) if !s.is_empty() => Some(s.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            other: tree
                .children
                .iter()
                .filter(|c| c.type_name != "BlockedAddresses")
                .cloned()
                .collect(),
        })
    }

    pub fn to_xml_string(&self) -> String {
        let mut root = ValueTree::new("SonobusGlobalState");
        let mut list = ValueTree::new("BlockedAddresses");
        for a in &self.blocked_addresses {
            let mut c = ValueTree::new("Address");
            c.set("value", Var::String(a.clone()));
            list.children.push(c);
        }
        root.children.push(list);
        root.children.extend(self.other.iter().cloned());
        root.to_xml_string()
    }
}

/// `soundboards.xml` (SoundboardProcessor.cpp:295-332).
#[derive(Debug, Clone, PartialEq)]
pub struct Soundboards {
    /// `selected`: index of the shown board; -1 (none) maps to `None`. Missing reads as 0. An
    /// index that is not a board of [`Self::boards`] loads as `None` (stock would index past its
    /// list).
    pub selected: Option<usize>,
    /// `hotkeysMuted` (default false)
    pub hotkeys_muted: bool,
    /// `hotkeysAllowNumeric` (default true)
    pub hotkeys_allow_numeric: bool,
    /// The root's children, any type (stock sorts them by name after loading).
    pub boards: Vec<Soundboard>,
}

impl Default for Soundboards {
    fn default() -> Self {
        Soundboards {
            selected: None,
            hotkeys_muted: false,
            hotkeys_allow_numeric: true,
            boards: Vec::new(),
        }
    }
}

/// `soundboard` (Soundboard.cpp:228-246).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Soundboard {
    /// `name`
    pub name: String,
    /// `samples/soundSample`
    pub samples: Vec<SoundSample>,
}

int_enum! {
    /// `SoundSample::EndPlaybackBehaviour` (Soundboard.h:51-55).
    EndPlaybackBehaviour { Stop = 0, Loop = 1, Next = 2 }
}
int_enum! {
    /// `SoundSample::PlaybackBehaviour` (Soundboard.h:33-49).
    PlaybackBehaviour { Simultaneous = 0, BackToBack = 1, Background = 2 }
}
int_enum! {
    /// `SoundSample::ButtonBehaviour` (Soundboard.h:57-61).
    ButtonBehaviour { Toggle = 0, Hold = 1, OneShot = 2 }
}
int_enum! {
    /// `SoundSample::ReplayBehaviour` (Soundboard.h:63-66).
    ReplayBehaviour { FromStart = 0, ContinueFromLastPosition = 1 }
}

/// `soundSample` (Soundboard.h:60-255, Soundboard.cpp:138-222). The behaviour fields keep the
/// stored number (values from newer versions round-trip); read them through the `*_checked`
/// accessors, which give `None` for values 1.7.2 does not define.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundSample {
    /// `name`
    pub name: String,
    /// `filePath`: legacy local path, written only for local files; used when `fileURL` is empty.
    pub file_path: Option<String>,
    /// `fileURL`
    pub file_url: String,
    /// `base64:fileURLBM`: iOS security-scoped bookmark (binary var).
    pub file_url_bookmark: Option<Vec<u8>>,
    /// `endPlaybackBehavior` (sic): 0 stop, 1 loop, 2 next. Missing: from the legacy `loop`
    /// bool. `loop` is always written as `end == 1`.
    pub end_playback_behaviour: i32,
    /// `buttonColour`: RGB, written as int64 (default 0x252525).
    pub button_colour: u32,
    /// `hotkey`: JUCE key code, -1 = none.
    pub hotkey: i32,
    /// `playbackBehaviour`: 0 simultaneous, 1 back to back, 2 background.
    pub playback_behaviour: i32,
    /// `buttonBehaviour`: 0 toggle, 1 hold, 2 one shot.
    pub button_behaviour: i32,
    /// `replayBehaviour`: 0 from start, 1 continue from last position.
    pub replay_behaviour: i32,
    /// `gain` (linear, default 1.0)
    pub gain: f32,
}

impl Default for SoundSample {
    fn default() -> Self {
        SoundSample {
            name: String::new(),
            file_path: None,
            file_url: String::new(),
            file_url_bookmark: None,
            end_playback_behaviour: 0,
            button_colour: 0x252525,
            hotkey: -1,
            playback_behaviour: 0,
            button_behaviour: 0,
            replay_behaviour: 0,
            gain: 1.0,
        }
    }
}

fn prop_i64(t: &ValueTree, key: &str) -> Option<i64> {
    t.get(key).map(Var::to_i64)
}

fn prop_str(t: &ValueTree, key: &str) -> Option<String> {
    t.get(key).map(var_to_string)
}

impl Soundboards {
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let tree = ValueTree::from_xml_str(&file_text(data)?)?;
        let d = Soundboards::default();
        let selected = tree.get("selected").map_or(0, Var::to_i32);
        let flag = |k, def| tree.get(k).map_or(def, Var::to_bool);
        Ok(Soundboards {
            selected: usize::try_from(selected)
                .ok()
                .filter(|&i| i < tree.children.len()),
            hotkeys_muted: flag("hotkeysMuted", d.hotkeys_muted),
            hotkeys_allow_numeric: flag("hotkeysAllowNumeric", d.hotkeys_allow_numeric),
            boards: tree.children.iter().map(Soundboard::from_tree).collect(),
        })
    }

    /// `tree.createXml()->writeTo(file)`.
    pub fn to_xml_string(&self) -> String {
        let mut t = ValueTree::new("soundboards");
        t.set("selected", Var::Int(self.selected.map_or(-1, |s| s as i32)));
        t.set("hotkeysMuted", Var::Bool(self.hotkeys_muted));
        t.set("hotkeysAllowNumeric", Var::Bool(self.hotkeys_allow_numeric));
        t.children = self.boards.iter().map(Soundboard::to_tree).collect();
        t.to_xml_string()
    }
}

impl Soundboard {
    fn from_tree(t: &ValueTree) -> Self {
        Soundboard {
            name: prop_str(t, "name").unwrap_or_default(),
            samples: t
                .child_with_name("samples")
                .map(|s| s.children.iter().map(SoundSample::from_tree).collect())
                .unwrap_or_default(),
        }
    }

    fn to_tree(&self) -> ValueTree {
        let mut t = ValueTree::new("soundboard");
        t.set("name", Var::String(self.name.clone()));
        let mut s = ValueTree::new("samples");
        s.children = self.samples.iter().map(SoundSample::to_tree).collect();
        t.children.push(s);
        t
    }
}

impl SoundSample {
    pub fn end_playback_behaviour_checked(&self) -> Option<EndPlaybackBehaviour> {
        EndPlaybackBehaviour::from_i32(self.end_playback_behaviour)
    }
    pub fn playback_behaviour_checked(&self) -> Option<PlaybackBehaviour> {
        PlaybackBehaviour::from_i32(self.playback_behaviour)
    }
    pub fn button_behaviour_checked(&self) -> Option<ButtonBehaviour> {
        ButtonBehaviour::from_i32(self.button_behaviour)
    }
    pub fn replay_behaviour_checked(&self) -> Option<ReplayBehaviour> {
        ReplayBehaviour::from_i32(self.replay_behaviour)
    }

    fn from_tree(t: &ValueTree) -> Self {
        let d = SoundSample::default();
        let int = |k, def: i64| prop_i64(t, k).unwrap_or(def);
        let old_loop = t.get("loop").is_some_and(Var::to_bool);
        SoundSample {
            name: prop_str(t, "name").unwrap_or_default(),
            file_path: prop_str(t, "filePath"),
            file_url: prop_str(t, "fileURL").unwrap_or_default(),
            file_url_bookmark: match t.get("fileURLBM") {
                Some(Var::Binary(b)) => Some(b.clone()),
                _ => None,
            },
            end_playback_behaviour: int("endPlaybackBehavior", i64::from(old_loop)) as i32,
            button_colour: int("buttonColour", i64::from(d.button_colour)) as u32,
            hotkey: int("hotkey", -1) as i32,
            playback_behaviour: int("playbackBehaviour", 0) as i32,
            button_behaviour: int("buttonBehaviour", 0) as i32,
            replay_behaviour: int("replayBehaviour", 0) as i32,
            gain: t.get("gain").map_or(d.gain, |v| v.to_f64() as f32),
        }
    }

    fn to_tree(&self) -> ValueTree {
        let mut t = ValueTree::new("soundSample");
        t.set("name", Var::String(self.name.clone()));
        if let Some(p) = &self.file_path {
            t.set("filePath", Var::String(p.clone()));
        }
        t.set("fileURL", Var::String(self.file_url.clone()));
        if let Some(b) = &self.file_url_bookmark {
            t.set("fileURLBM", Var::Binary(b.clone()));
        }
        t.set("endPlaybackBehavior", Var::Int(self.end_playback_behaviour));
        t.set("loop", Var::Bool(self.end_playback_behaviour == 1));
        t.set("buttonColour", Var::Int64(i64::from(self.button_colour)));
        t.set("hotkey", Var::Int(self.hotkey));
        t.set("playbackBehaviour", Var::Int(self.playback_behaviour));
        t.set("buttonBehaviour", Var::Int(self.button_behaviour));
        t.set("replayBehaviour", Var::Int(self.replay_behaviour));
        t.set("gain", Var::Double(f64::from(self.gain)));
        t
    }
}
