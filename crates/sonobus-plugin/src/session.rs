//! One plugin instance's session: the app's controller in hosted mode (with the app's UI state,
//! [`SonobusApp`]) over the engine the plugin builds and processes. The engine is rebuilt off
//! the audio thread when the host changes the sample rate, block size or channel layout; the
//! controller keeps the session and rejoins the group, as the app does. Also the persisted
//! state in upstream's format and `PluginDefault.xml`.
//!
//! Nothing here is global: every instance owns its own [`Session`], controller, engine and UDP
//! socket. The controller and its UI state live here, with the instance, not with the editor:
//! closing and reopening the editor keeps them.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError, Weak};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use nice_plug::context::gui::GuiContext;
use nice_plug::params::persist::PersistentField;
use sonobus_app::controller::{AppController, Host};
use sonobus_app::settings::{self, SOUNDBOARDS_FILE};
use sonobus_app::ui::SonobusApp;
use sonobus_core::files;
use sonobus_core::juce_xml::{from_base64, to_base64};
use sonobus_core::state::{self, AppState, ChannelGroup, Scope};
use sonobus_core::valuetree::ValueTree;
use sonobus_engine::{AudioConfig, Engine, EngineAudio, EngineConfig, Soundboards};

use crate::params::SbParams;

/// A server login (and group) to connect to: the app's connect form.
pub use sonobus_app::controller::ConnectRequest as Connect;

/// Upstream's plugin defaults file (P.cpp:8805-8835), in the settings directory.
pub const PLUGIN_DEFAULT_FILE: &str = "PluginDefault.xml";

/// The app's settings directory (`~/.config/sonobus-rs/` etc.): `PluginDefault.xml` and the
/// soundboards, shared with the app and every instance (as upstream's soundboards file).
pub fn settings_dir() -> Option<PathBuf> {
    settings::app_dir()
}

/// Without an editor frame for this long, the parameter thread ticks the controller (events,
/// chat, OSC keep flowing with the editor closed).
const IDLE_TICK: Duration = Duration::from_millis(200);

struct Inner {
    /// The controller and UI state, once the host first activated the plugin.
    app: Option<SonobusApp>,
    /// The state while there is no controller (before the first activation, or after a failed
    /// one).
    state: AppState,
    last_tick: Instant,
}

/// The editor's host context, and the thread its frames run on: UI parameter edits become host
/// gestures through it, only from that thread.
type Gui = Arc<Mutex<Option<(GuiContext, ThreadId)>>>;

pub struct Session {
    defaults_dir: Option<PathBuf>,
    /// The controller's engine, for the parameter thread and the host's state saves (no wait
    /// for an editor frame).
    engine: Mutex<Option<Arc<Engine>>>,
    inner: Mutex<Inner>,
    /// A state load happened: the parameters must be pushed to the engine again.
    resync: AtomicBool,
    gui: Gui,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the hosted controller asks of the plugin.
struct PluginHost {
    /// Weak: the parameters own the session (their persist field), which owns this.
    params: Weak<SbParams>,
    gui: Gui,
    dir: Option<PathBuf>,
}

impl Host for PluginHost {
    fn params(&self) -> state::Params {
        self.params
            .upgrade()
            .map_or_else(state::Params::plugin, |p| p.to_core())
    }

    fn params_edited(&mut self, old: &state::Params, new: &state::Params) {
        let Some(p) = self.params.upgrade() else {
            return;
        };
        let gui = lock(&self.gui)
            .as_ref()
            .filter(|g| g.1 == std::thread::current().id())
            .map(|g| g.0.clone());
        p.apply_edit(
            gui.as_ref().map(GuiContext::param_setter).as_ref(),
            old,
            new,
        );
    }

    fn soundboards_changed(&mut self, boards: &Soundboards) -> io::Result<()> {
        match &self.dir {
            Some(d) => {
                std::fs::create_dir_all(d)?;
                settings::save_soundboards(&d.join(SOUNDBOARDS_FILE), boards)
            }
            None => Ok(()),
        }
    }

    fn save_default(&mut self, state: &AppState) -> io::Result<PathBuf> {
        save_plugin_default(self.dir.as_deref(), state)
    }

    fn reset_default(&mut self) -> io::Result<()> {
        reset_plugin_default(self.dir.as_deref())
    }
}

/// `saveCurrentAsDefaultPluginSettings` (P.cpp:8803-8811): no cache, no input groups, XML.
fn save_plugin_default(dir: Option<&Path>, state: &AppState) -> io::Result<PathBuf> {
    let dir =
        dir.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no settings directory"))?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(PLUGIN_DEFAULT_FILE);
    std::fs::write(&path, files::save_plugin_default(state))?;
    Ok(path)
}

/// `resetDefaultPluginSettings` (P.cpp:8826-8831).
fn reset_plugin_default(dir: Option<&Path>) -> io::Result<()> {
    match dir.map(|d| std::fs::remove_file(d.join(PLUGIN_DEFAULT_FILE))) {
        Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

impl Session {
    /// Plugin defaults, then `PluginDefault.xml` from `defaults_dir` if there is one (upstream
    /// loads it in the processor constructor when running as a plugin, P.cpp:840-843).
    pub fn new(defaults_dir: Option<PathBuf>) -> Self {
        let mut state = AppState::new(true);
        if let Some(d) = &defaults_dir
            && let Ok(data) = std::fs::read(d.join(PLUGIN_DEFAULT_FILE))
            && let Err(e) = files::load_plugin_default(&data, &mut state)
        {
            nice_plug::nice_log!("sonobus: ignoring {PLUGIN_DEFAULT_FILE}: {e}");
        }
        Session {
            defaults_dir,
            engine: Mutex::new(None),
            inner: Mutex::new(Inner {
                app: None,
                state,
                last_tick: Instant::now(),
            }),
            resync: AtomicBool::new(false),
            gui: Gui::default(),
        }
    }

    pub fn engine(&self) -> Option<Arc<Engine>> {
        lock(&self.engine).clone()
    }

    /// The current state (the engine's, with every peer's settings in the peer cache).
    pub fn state(&self) -> AppState {
        if let Some(e) = self.engine() {
            return e.state();
        }
        lock(&self.inner).state.clone()
    }

    /// `getStateInformation` (P.cpp:8602-8606): the `SonoBusAoO` tree in JUCE's binary
    /// `ValueTree::writeToStream` format, with the cache and input groups. Without what stays
    /// on this machine (`settings::keep_local`: OSC, UDP port, record folder, language,
    /// recents): projects travel between people.
    pub fn state_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        settings::shareable(&self.state())
            .to_tree_scoped(Scope::FULL)
            .write(&mut out);
        out
    }

    /// `setStateInformation` (P.cpp:8797-8800) from those bytes. An invalid tree changes nothing.
    pub fn load_state_bytes(&self, data: &[u8]) -> io::Result<()> {
        let tree = ValueTree::read(data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let local = self.state();
        let mut s = local.clone();
        s.apply_tree(&tree, Scope::FULL);
        // a project cannot set this machine's OSC exposure, ports, folders or language
        settings::keep_local(&mut s, &local);
        self.restore(s);
        Ok(())
    }

    fn restore(&self, mut s: AppState) {
        // upstream never restores these (P.cpp:8778-8786)
        s.params.met_enabled = false;
        s.params.main_monitor_solo = false;
        let mut g = lock(&self.inner);
        match &mut g.app {
            Some(app) => {
                app.ctrl.engine().update_state(move |x| *x = s);
                app.ctrl.refresh_state();
            }
            None => g.state = s,
        }
        self.resync.store(true, Ordering::Release);
    }

    /// True once after a state load.
    pub fn take_resync(&self) -> bool {
        self.resync.swap(false, Ordering::AcqRel)
    }

    /// Writes `PluginDefault.xml` from the current state.
    pub fn save_plugin_default(&self) -> io::Result<PathBuf> {
        save_plugin_default(self.defaults_dir.as_deref(), &self.state())
    }

    pub fn reset_plugin_default(&self) -> io::Result<()> {
        reset_plugin_default(self.defaults_dir.as_deref())
    }

    pub fn defaults_dir(&self) -> Option<&Path> {
        self.defaults_dir.as_deref()
    }

    /// (Re)builds the engine for `cfg` with the parameters' values; the caller installs the
    /// returned audio half. Called from the host's activate (never the audio thread). The
    /// first activation creates the controller (which reconnects to the most recent group when
    /// `reconnectlast` is on, as upstream's first state restore, P.cpp:8788-8795); later ones
    /// hand it the new engine, and it rejoins a joined group.
    pub fn start(
        &self,
        cfg: AudioConfig,
        main_inputs: usize,
        params: &Arc<SbParams>,
    ) -> io::Result<EngineAudio> {
        let p = params.to_core();
        let mut audio = None;
        let mut build = |mut state: AppState| -> io::Result<Arc<Engine>> {
            state.params = p.clone();
            if state.input_groups.is_empty() {
                // one group over the main input only, not the aux inputs (as the app's device
                // inputs; upstream: P.cpp:6991-6998)
                let mut grp = ChannelGroup::input_default(0, true);
                grp.num_channels = main_inputs.max(1) as i32;
                grp.pans = vec![0.0; grp.num_channels as usize];
                grp.mon_dest_channels = cfg.output_channels.min(2) as i32;
                state.input_groups.push(grp);
            }
            let port = u16::try_from(state.extra.use_udp_port).unwrap_or(0);
            let (engine, rt) = Engine::new(EngineConfig {
                audio: cfg,
                udp_port: port,
                state,
            })?;
            audio = Some(rt);
            Ok(Arc::new(engine))
        };
        let mut g = lock(&self.inner);
        // the controller drops the old engine before building: its UDP port must be free
        *lock(&self.engine) = None;
        match &mut g.app {
            Some(app) => {
                let mut kept = None;
                let res = app.ctrl.swap_engine(|s| {
                    kept = Some(s.clone());
                    build(s)
                });
                if let Err(e) = res {
                    // the controller has no engine now: keep the settings for the next try
                    g.app = None;
                    g.state = kept.unwrap_or_else(|| AppState::new(true));
                    return Err(e);
                }
            }
            None => {
                let engine = build(g.state.clone())?;
                let boards = self
                    .defaults_dir
                    .as_ref()
                    .and_then(|d| settings::load_soundboards(&d.join(SOUNDBOARDS_FILE)).ok());
                let host = PluginHost {
                    params: Arc::downgrade(params),
                    gui: self.gui.clone(),
                    dir: self.defaults_dir.clone(),
                };
                let ctrl =
                    AppController::hosted(engine, Box::new(host), boards.unwrap_or_default());
                let form = ctrl.default_request();
                g.app = Some(SonobusApp::new(ctrl, form));
            }
        }
        if let Some(app) = &g.app {
            *lock(&self.engine) = Some(app.ctrl.shared_engine());
        }
        audio.ok_or_else(|| io::Error::other("engine not built"))
    }

    /// Runs `f` on the controller and UI state, if the plugin was activated.
    pub fn with_app<R>(&self, f: impl FnOnce(&mut SonobusApp) -> R) -> Option<R> {
        let mut g = lock(&self.inner);
        g.last_tick = Instant::now();
        g.app.as_mut().map(f)
    }

    /// One editor frame: the app's full UI. `false` before the first activation.
    pub fn frame(&self, gui: &GuiContext, ui: &mut egui::Ui) -> bool {
        *lock(&self.gui) = Some((gui.clone(), std::thread::current().id()));
        self.with_app(|app| app.show(ui)).is_some()
    }

    /// Called by the parameter thread: ticks the controller while no editor frame does.
    pub fn idle_tick(&self) {
        let mut g = match self.inner.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return, // an editor frame (which ticks) or the host
        };
        if g.last_tick.elapsed() >= IDLE_TICK {
            g.last_tick = Instant::now();
            if let Some(app) = &mut g.app {
                app.tick();
            }
        }
    }

    /// Logs in (DNS on a helper thread) and joins `c.group`. No-op before the first activation.
    pub fn connect(&self, c: Connect) {
        self.with_app(|app| app.ctrl.connect(c));
    }

    pub fn disconnect(&self) {
        self.with_app(|app| app.ctrl.disconnect());
    }

    /// Remembers the editor size (`lastWindowWidth/Height`, saved with the state, as upstream
    /// E.cpp:1297-1301).
    pub fn set_window_size(&self, w: i32, h: i32) {
        let mut g = lock(&self.inner);
        match &mut g.app {
            Some(app) => app
                .ctrl
                .edit(|s| (s.extra.last_window_width, s.extra.last_window_height) = (w, h)),
            None => {
                (
                    g.state.extra.last_window_width,
                    g.state.extra.last_window_height,
                ) = (w, h)
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // OSC "closing" messages, as the app on exit
        if let Some(app) = &mut lock(&self.inner).app {
            app.ctrl.shutdown();
        }
    }
}

/// The `#[persist]` field holding the session. Hosts store nice-plug's JSON state; this field's
/// entry is upstream's `getStateInformation` bytes (the binary `SonoBusAoO` tree) in JUCE's
/// `MemoryBlock::toBase64Encoding`, read and written through [`Session`].
pub struct StateField(pub Arc<Session>);

impl<'a> PersistentField<'a, String> for StateField {
    fn set(&self, new_value: String) {
        match from_base64(&new_value) {
            Some(b) => {
                if let Err(e) = self.0.load_state_bytes(&b) {
                    nice_plug::nice_log!("sonobus: ignoring saved state: {e}");
                }
            }
            None => nice_plug::nice_log!("sonobus: ignoring saved state: not base64"),
        }
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&String) -> R,
    {
        f(&to_base64(&self.0.state_bytes()))
    }
}
