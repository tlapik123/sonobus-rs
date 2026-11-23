//! The plugin editor: the app's own UI ([`sonobus_app::ui::SonobusApp`], plugin capabilities)
//! drawn through nice-plug-egui (egui 0.37, the app's egui). The controller and UI state live in
//! the instance's [`Session`]; the editor only draws them, so closing and reopening it keeps
//! everything. Automatable parameters edited in the UI reach the host as gestures (the hosted
//! controller's [`sonobus_app::controller::Host`]); host automation shows in the UI on the next
//! frame. The window is resizable and its size is kept in the state (`ExtraState`
//! `lastWindowWidth`/`lastWindowHeight`, as upstream, E.cpp:1297-1301).

use std::sync::Arc;

use nice_plug::context::gui::GuiContext;
use nice_plug::editor::dpi::LogicalSize;
use nice_plug::prelude::*;
use nice_plug_egui::resizable_window::ResizableWindow;
use nice_plug_egui::{
    EguiEditor, EguiEditorState, EguiNiceSettings, NiceEguiApp, RepaintNotifier, create_egui_editor,
};

use crate::session::Session;

/// Upstream's minimum editor size (E.cpp:1297).
pub const MIN_SIZE: (f32, f32) = (320.0, 340.0);
/// The editor size when none is saved (upstream: 800x600, SonobusPluginProcessor.h:1063).
const DEFAULT_SIZE: (f32, f32) = (800.0, 600.0);

fn saved_size(session: &Session) -> egui::Vec2 {
    let e = session.state().extra;
    let pick = |v: i32, min: f32, def: f32| if v as f32 >= min { v as f32 } else { def };
    egui::vec2(
        pick(e.last_window_width, MIN_SIZE.0, DEFAULT_SIZE.0),
        pick(e.last_window_height, MIN_SIZE.1, DEFAULT_SIZE.1),
    )
}

pub fn create(session: Arc<Session>) -> Option<EguiEditor<SbEditor>> {
    let size = saved_size(&session);
    let state = EguiEditorState::from_size(LogicalSize::new(size.x, size.y), 1.0);
    let hint =
        ResizeHint::resizable().with_min_logical_size(LogicalSize::new(MIN_SIZE.0, MIN_SIZE.1));
    let settings = EguiNiceSettings::new()
        .with_tile("SonoBus RS")
        .with_resize_hint(hint);
    create_egui_editor(
        state,
        RepaintNotifier::new(),
        settings,
        SbEditor {
            session,
            gui: None,
            wanted_size: None,
        },
    )
}

pub struct SbEditor {
    session: Arc<Session>,
    gui: Option<GuiContext>,
    /// The size to ask the host for on the first frame (a state load may have changed it).
    wanted_size: Option<egui::Vec2>,
}

impl NiceEguiApp for SbEditor {
    fn build(
        &mut self,
        _egui_ctx: egui::Context,
        gui: GuiContext,
        _frame: &mut nice_plug_egui::Frame,
    ) -> Result<(), nice_plug_egui::baseview::HandlerError> {
        self.gui = Some(gui);
        self.wanted_size = Some(saved_size(&self.session));
        // upstream applies a language change when the host reopens the view
        self.session.with_app(|app| app.ctrl.reload_language());
        Ok(())
    }

    fn resized(&mut self, size: nice_plug_egui::baseview::WindowSize) {
        let (w, h) = (
            size.logical.width.round() as i32,
            size.logical.height.round() as i32,
        );
        self.session.set_window_size(w, h);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut nice_plug_egui::Frame) {
        if let Some(s) = self.wanted_size.take()
            && (ui.ctx().content_rect().size() - s).length() > 1.0
        {
            ui.send_viewport_cmd(egui::ViewportCommand::InnerSize(s));
        }
        let Some(gui) = self.gui.clone() else { return };
        let session = self.session.clone();
        ResizableWindow::new("sonobus")
            .min_size(egui::vec2(MIN_SIZE.0, MIN_SIZE.1))
            .show(ui, |ui| {
                if !session.frame(&gui, ui) {
                    ui.label("Not active: the host has not started audio processing yet.");
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(200));
                }
            });
    }
}
