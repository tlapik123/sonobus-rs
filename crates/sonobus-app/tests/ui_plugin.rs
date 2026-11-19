//! The UI with the plugin's capabilities (a hosted controller): what does not apply in a plugin
//! is absent, the plugin-only actions are present, at the plugin's small and default editor
//! sizes; and two instances in one process, each in its own language, render independently.

use std::sync::Arc;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use sonobus_app::controller::{AppController, Host};
use sonobus_app::ui::{Capabilities, ConnectTab, FileAction, Page, SonobusApp};
use sonobus_core::state::{AppState, Params};
use sonobus_engine::{AudioConfig, Engine, EngineConfig, Soundboards};

struct NoHost;

impl Host for NoHost {
    fn params(&self) -> Params {
        Params::plugin()
    }
    fn params_edited(&mut self, _: &Params, _: &Params) {}
    fn soundboards_changed(&mut self, _: &Soundboards) -> std::io::Result<()> {
        Ok(())
    }
    fn save_default(&mut self, _: &AppState) -> std::io::Result<std::path::PathBuf> {
        Ok("PluginDefault.xml".into())
    }
    fn reset_default(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A plugin instance's UI in `lang` ("" = the OS locale); the engine is never processed.
fn plugin_app(lang: &str) -> SonobusApp {
    let mut state = AppState::new(true);
    state.extra.lang_override_code = lang.into();
    let audio = AudioConfig { sample_rate: 48_000, block_size: 256, input_channels: 2, output_channels: 2 };
    let (engine, _rt) = Engine::new(EngineConfig { audio, udp_port: 0, state }).unwrap();
    let ctrl = AppController::hosted(Arc::new(engine), Box::new(NoHost), Soundboards::default());
    let form = ctrl.default_request();
    SonobusApp::new(ctrl, form)
}

fn harness(app: SonobusApp, size: (f32, f32)) -> Harness<'static, SonobusApp> {
    Harness::builder().with_size(size).build_ui_state(|ui, app: &mut SonobusApp| app.show(ui), app)
}

fn absent(h: &Harness<'_, SonobusApp>, label: &str) {
    assert!(h.query_by_label(label).is_none(), "{label:?} is shown in the plugin");
}

#[test]
fn plugin_capabilities_hide_what_does_not_apply() {
    for size in [(400.0, 600.0), (900.0, 700.0)] {
        let mut h = harness(plugin_app("en"), size);
        assert_eq!(h.state().caps, Capabilities::PLUGIN);
        h.state_mut().open_page(Page::Main);
        h.run_steps(1);
        h.run();
        absent(&h, "Setup Audio");
        h.get_by_label("Mute All Input");
        h.get_by_label("Record");
        // the metronome has "Sync to Host"
        h.get_by_label("Metronome").click_accesskit();
        h.run_steps(1);
        h.run();
        h.get_by_label("Sync to Host");
        // the File menu: audio files only, no setups or stock import
        h.get_by_label("File").click_accesskit();
        h.run_steps(1);
        h.run();
        h.get_by_label("Open Audio File...");
        absent(&h, "Load Setup...");
        absent(&h, "Save Setup...");
        absent(&h, "Import settings from SonoBus...");
        h.key_press(egui::Key::Escape);
        // pages that do not exist here cannot be opened
        for p in [Page::AudioSetup, Page::File(FileAction::LoadSetup), Page::File(FileAction::SaveSetup), Page::File(FileAction::ImportSonoBus)] {
            h.state_mut().open_page(p);
            h.run_steps(1);
        h.run();
            assert_ne!(h.state().page, p);
        }
        h.state_mut().page = Page::AudioSetup; // set directly: falls back to the main page
        h.run_steps(1);
        h.run();
        assert_eq!(h.state().page, Page::Main);
        // connect: no hosting a server
        h.state_mut().open_page(Page::Connect);
        h.run_steps(1);
        h.run();
        h.get_by_label("PRIVATE GROUP");
        absent(&h, "HOST");
        h.state_mut().tab = ConnectTab::Host;
        h.run_steps(1);
        h.run();
        assert_eq!(h.state().tab, ConnectTab::Group);
        // options: the plugin default actions, no settings files
        h.state_mut().open_page(Page::Options);
        h.run_steps(1);
        h.run();
        h.get_by_label("Save as default plugin options");
        h.get_by_label("Reset default plugin options");
        absent(&h, "Save now");
        h.get_by_label("Language");
        h.get_by_label("Enable OSC control");
        assert!(!h.state().ctrl.vm().osc.config.enabled, "OSC off by default");
        h.get_by_label("Save as default plugin options").click_accesskit();
        h.run_steps(1);
        h.run();
        assert!(!h.state().ctrl.vm().status_is_error);
    }
}

#[test]
fn standalone_keeps_its_pages() {
    assert!(Capabilities::STANDALONE.has(Page::AudioSetup));
    assert!(Capabilities::STANDALONE.has(Page::File(FileAction::LoadSetup)));
    assert!(!Capabilities::PLUGIN.has(Page::AudioSetup));
    assert!(Capabilities::PLUGIN.has(Page::File(FileAction::OpenAudio)));
}

#[test]
fn two_instances_in_their_own_languages() {
    let mut de = harness(plugin_app("de"), (400.0, 600.0));
    let mut fr = harness(plugin_app("fr"), (900.0, 700.0));
    for h in [&mut de, &mut fr] {
        h.state_mut().open_page(Page::Main);
    }
    // interleaved frames, as two editors of one process
    for _ in 0..3 {
        de.run();
        fr.run();
        de.get_by_label("Alle Eingangssignale stumm schalten");
        fr.get_by_label("Toutes entrées muettes");
        assert!(de.query_by_label("Toutes entrées muettes").is_none());
        assert!(fr.query_by_label("Mute All Input").is_none());
    }
    assert_eq!(de.state().ctrl.language(), Some("de"));
    assert_eq!(fr.state().ctrl.language(), Some("fr"));
    // the process language is untouched (English here)
    assert_eq!(sonobus_app::i18n::current(), None);
    assert_eq!(sonobus_app::tr("Mute All Input"), "Mute All Input");
    // a language change applies when the host reopens the view
    de.state_mut().ctrl.edit(|s| s.extra.lang_override_code = "fr".into());
    de.state_mut().ctrl.reload_language();
    de.run();
    de.get_by_label("Toutes entrées muettes");
}

/// Secure sessions are in the plugin too; advertising one on the LAN is opt-in there.
#[test]
fn plugin_hosts_secure_sessions_without_lan_advertising_by_default() {
    let mut h = harness(plugin_app("en"), (900.0, 700.0));
    h.state_mut().open_page(Page::Connect);
    h.run_steps(2);
    h.get_by_label("Secure (this app only)").click();
    h.run_steps(2);
    h.check_accessibility();
    h.get_by_label("Host Secure Session");
    assert!(!h.state().host_form.lan, "LAN advertising starts unticked in a plugin");
}
