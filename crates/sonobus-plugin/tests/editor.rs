//! The editor's UI (the app's, plugin capabilities) through the instance's session: two
//! instances in one process with different languages draw independently, and the UI state
//! outlives the editor (closed and reopened = a new egui context).

mod common;

use std::sync::Arc;

use common::*;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use sonobus_app::ui::{Capabilities, Page};
use sonobus_plugin::session::Session;

/// An editor window on `s` (what `SbEditor::ui` draws, without the host's window).
fn editor(s: Arc<Session>) -> Harness<'static, Arc<Session>> {
    Harness::builder().with_size((400.0, 600.0)).build_ui_state(
        |ui, s: &mut Arc<Session>| {
            s.with_app(|app| app.show(ui)).expect("activated");
        },
        s,
    )
}

/// An instance whose language comes from its own `PluginDefault.xml` (the language is this
/// machine's: a project does not carry it).
fn instance(lang: &str) -> Host {
    let dir = tmp_dir(&format!("editor-{lang}"));
    let mut st = sonobus_core::state::AppState::new(true);
    st.extra.lang_override_code = lang.into();
    std::fs::write(
        dir.join(sonobus_plugin::session::PLUGIN_DEFAULT_FILE),
        sonobus_core::files::save_plugin_default(&st),
    )
    .unwrap();
    let mut h = Host::with_dir(440.0, Some(dir));
    // a project's language does not apply
    let mut theirs = h.plugin.session().state();
    theirs.extra.lang_override_code = "ja".into();
    let mut bytes = Vec::new();
    theirs
        .to_tree_scoped(sonobus_core::state::Scope::FULL)
        .write(&mut bytes);
    h.plugin.session().load_state_bytes(&bytes).unwrap();
    assert_eq!(h.plugin.session().state().extra.lang_override_code, lang);
    assert!(h.activate(SR, BLOCK as u32, 0));
    h
}

#[test]
fn two_instances_in_two_languages_and_reopening() {
    let (a, b) = (instance("de"), instance("fr"));
    let (sa, sb) = (a.plugin.session().clone(), b.plugin.session().clone());
    assert_eq!(sa.with_app(|app| app.caps), Some(Capabilities::PLUGIN));
    for s in [&sa, &sb] {
        s.with_app(|app| app.open_page(Page::Main));
    }
    let (mut ea, mut eb) = (editor(sa.clone()), editor(sb.clone()));
    for _ in 0..3 {
        ea.run();
        eb.run();
        ea.get_by_label("Alle Eingangssignale stumm schalten");
        eb.get_by_label("Toutes entrées muettes");
    }
    // UI state lives with the instance: close the editor, reopen it
    sa.with_app(|app| {
        app.open_page(Page::Chat);
        app.chat_text = "draft".into();
    });
    drop(ea);
    let mut ea = editor(sa.clone());
    ea.run();
    assert_eq!(
        sa.with_app(|app| (app.page, app.chat_text.clone())),
        Some((Page::Chat, "draft".into()))
    );
    sa.with_app(|app| app.open_page(Page::Main));
    ea.run();
    ea.get_by_label("Alle Eingangssignale stumm schalten")
        .click_accesskit();
    ea.run();
    assert!(sa.state().params.main_in_mute, "UI edit reached the engine");
    assert!(
        a.plugin.sb_params().main_in_mute.value(),
        "and the host parameter"
    );
}
