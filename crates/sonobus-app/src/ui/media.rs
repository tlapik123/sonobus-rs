//! Recording, file playback and the soundboard (SonobusPluginEditor's transport,
//! SoundboardView, SampleEditView, OptionsView's RECORDING tab).

use std::path::Path;

use egui::{Color32, RichText, Ui};
use sonobus_core::files::{SoundSample, Soundboard};

use super::{FileAction, Icon, Page, SonobusApp, gain_slider, hms, icon_button, paint_icon, row, text_field, toggle};
use crate::controller::open_in_os;
use crate::settings::{file_url, url_path};
use crate::tr;

/// The sample being added or edited.
#[derive(Debug, Clone)]
pub struct SampleEdit {
    pub board: usize,
    /// `None`: a new sample.
    pub index: Option<usize>,
    pub sample: SoundSample,
    /// The file field (a path; stored as a `file://` URL).
    pub path: String,
    /// "Press a key...": the next key becomes the hotkey.
    pub capturing_hotkey: bool,
}

// ---- recording and file transport (main page) -------------------------------------------------

pub fn transport(app: &mut SonobusApp, ui: &mut Ui) {
    let rec = app.ctrl.vm().recording.clone();
    row(ui, |ui| {
        let on = rec.is_some();
        let (text, fill) = match &rec {
            // blinks once a second while recording
            Some(r) => (format!("       {}", hms(r.elapsed_secs)), Some(if (r.elapsed_secs as u64).is_multiple_of(2) { Color32::from_rgb(200, 40, 40) } else { Color32::from_rgb(140, 30, 30) })),
            None => (format!("       {}", tr("Record")), None),
        };
        let mut b = egui::Button::new(RichText::new(text).color(if on { Color32::WHITE } else { Color32::from_rgb(220, 70, 70) })).selected(on);
        if let Some(f) = fill {
            b = b.fill(f);
        }
        let r = ui.add(b.min_size(egui::vec2(0.0, 26.0)));
        paint_icon(ui.painter(), r.rect.left_center() + egui::vec2(15.0, 0.0), Icon::Record, if on { Color32::WHITE } else { Color32::from_rgb(220, 60, 60) });
        r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, on, tr("Record")));
        let hover = match &rec {
            Some(r) if r.files.len() > 1 => format!("{}{}", tr("Recording multi-track audio to: "), r.path.display()),
            Some(r) => format!("{}{}", tr("Recording audio to: "), r.path.display()),
            None => tr("Start/Stop recording audio to file").to_owned(),
        };
        if r.on_hover_text(hover).clicked() {
            app.ctrl.toggle_recording();
        }
        if let Some(r) = &rec
            && r.dropped_frames > 0
        {
            ui.colored_label(Color32::LIGHT_RED, format!("{} {}", r.dropped_frames, tr("frames lost")));
        }
        if ui.button(tr("Load File")).on_hover_text(tr("Load audio file for playback")).clicked() {
            app.open_page(Page::File(FileAction::OpenAudio));
        }
    });
    let last = app.ctrl.vm().last_recording.clone();
    if rec.is_none() && !last.is_empty() {
        egui::CollapsingHeader::new(format!("{} ({})", tr("Last recording"), last.len())).id_salt("last_rec").show(ui, |ui| {
            for f in &last {
                row(ui, |ui| {
                    ui.label(RichText::new(f.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())).monospace());
                    if ui.small_button(tr("Play")).on_hover_text(f.display().to_string()).clicked()
                        && let Err(e) = app.ctrl.load_file(f)
                    {
                        app.ctrl.set_status(format!("{}: {e}", tr("Cannot play file")), true);
                    }
                });
            }
            if ui.button(tr("Reveal File")).clicked()
                && let Some(dir) = last[0].parent()
                && let Err(e) = open_in_os(dir)
            {
                app.ctrl.set_status(e.to_string(), true);
            }
        });
    }
    file_panel(app, ui);
}

/// The file overview: peaks, loop range, play position; click or drag to seek. Returns the
/// position asked for, in seconds.
fn waveform(ui: &mut Ui, peaks: Option<&[[f32; 2]]>, f: &sonobus_engine::FileStatus, len: f64) -> Option<f64> {
    let size = egui::vec2(ui.available_width().max(60.0), 56.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::slider(true, f.position_secs, tr("Waveform")));
    let p = ui.painter_at(rect);
    let v = ui.visuals();
    p.rect_filled(rect, 3.0, v.extreme_bg_color);
    let x_of = |secs: f64| rect.left() + (secs / len).clamp(0.0, 1.0) as f32 * rect.width();
    if f.looping {
        let (start, l) = f.loop_range_secs;
        let end = if l > 0.0 { start + l } else { len };
        let r = egui::Rect::from_x_y_ranges(x_of(start)..=x_of(end), rect.y_range());
        p.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(60, 130, 200, 50));
    }
    match peaks {
        Some(pk) if !pk.is_empty() => {
            let colour = v.widgets.inactive.fg_stroke.color;
            let mid = rect.center().y;
            let half = rect.height() * 0.48;
            let cols = rect.width().max(1.0) as usize;
            for x in 0..cols {
                let a = x * pk.len() / cols;
                let b = ((x + 1) * pk.len() / cols).max(a + 1).min(pk.len());
                let (lo, hi) = pk[a..b].iter().fold((f32::MAX, f32::MIN), |(lo, hi), q| (lo.min(q[0]), hi.max(q[1])));
                let px = rect.left() + x as f32 + 0.5;
                p.line_segment(
                    [egui::pos2(px, mid - hi.clamp(-1.0, 1.0) * half), egui::pos2(px, mid - lo.clamp(-1.0, 1.0) * half + 0.5)],
                    (1.0, colour),
                );
            }
        }
        _ => {
            p.text(rect.center(), egui::Align2::CENTER_CENTER, tr("Loading waveform..."), egui::FontId::proportional(12.0), v.weak_text_color());
        }
    }
    let x = x_of(f.position_secs);
    p.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], (1.5, Color32::from_rgb(230, 170, 60)));
    if resp.clicked() || resp.dragged() {
        let px = resp.interact_pointer_pos()?.x;
        return Some(f64::from((px - rect.left()) / rect.width()).clamp(0.0, 1.0) * len);
    }
    None
}

fn file_panel(app: &mut SonobusApp, ui: &mut Ui) {
    if let Some(rx) = &app.trim
        && let Ok(r) = rx.try_recv()
    {
        app.trim = None;
        match r.and_then(|p| app.ctrl.load_file(&p).map(|()| p)) {
            Ok(p) => app.ctrl.set_status(format!("{} {}", tr("Trimmed to"), p.display()), false),
            Err(e) => app.ctrl.set_status(format!("{}: {e}", tr("Trim to New")), true),
        }
    }
    let Some(f) = app.ctrl.vm().file.clone() else { return };
    egui::Frame::group(ui.style()).show(ui, |ui| {
        let e = app.ctrl.engine();
        let name = f.path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
        let len = f.length_secs.unwrap_or(f.position_secs.max(1.0)).max(0.001);
        row(ui, |ui| {
            ui.label(RichText::new(name).strong());
            ui.label(RichText::new(format!("{}  {} ch  {} Hz", hms(len), f.channels, f.sample_rate)).weak());
        });
        let peaks = app.waveforms.get(&f.path, f.sample_rate).cloned();
        if let Some(secs) = waveform(ui, peaks.as_deref(), &f, len) {
            e.seek_file(secs);
        }
        row(ui, |ui| {
            ui.horizontal(|ui| {
                if icon_button(ui, Icon::ToStart, tr("Return to start of file"), false).clicked() {
                    e.seek_file(0.0);
                }
                if icon_button(ui, if f.playing { Icon::Pause } else { Icon::Play }, tr("Play"), f.playing).clicked() {
                    e.set_file_playing(!f.playing);
                }
                if icon_button(ui, Icon::Loop, tr("Loop"), f.looping).on_hover_text(tr("Toggle loop range")).clicked() {
                    e.set_file_looping(!f.looping);
                }
            });
            let mut pos = f.position_secs;
            let r = ui.add(egui::Slider::new(&mut pos, 0.0..=len).text(tr("Position")).custom_formatter(|v, _| hms(v)));
            if r.changed() {
                e.seek_file(pos);
            }
            ui.label(format!("{} / {}", hms(f.position_secs), hms(len)));
        });
        if f.looping {
            row(ui, |ui| {
                let (mut start, mut length) = f.loop_range_secs;
                if length <= 0.0 {
                    length = len - start;
                }
                let a = ui.add(egui::Slider::new(&mut start, 0.0..=len).text(tr("Loop Start")).custom_formatter(|v, _| hms(v))).changed();
                let b = ui.add(egui::Slider::new(&mut length, 0.0..=len).text(tr("Loop Length")).custom_formatter(|v, _| hms(v))).changed();
                if a || b {
                    e.set_file_loop_range(start, length.min(len - start));
                }
            });
        }
        let sends = app.sends();
        let st = &app.ctrl.vm().state;
        let (mut g, mut send) = (st.file_playback_group.clone(), st.params.send_file_audio);
        let (old_g, old_send) = (g.clone(), send);
        row(ui, |ui| {
            gain_slider(ui, &mut g.gain, 2.0, tr("Playback Level"));
            gain_slider(ui, &mut g.monitor, 2.0, tr("Monitor"));
            if sends {
                ui.checkbox(&mut send, tr("Send File Playback to All"))
                    .on_hover_text(if send { tr("Sending file playback to all users") } else { tr("Now only you will hear the file playback") });
            }
        });
        if g != old_g || send != old_send {
            app.ctrl.edit(move |s| {
                s.file_playback_group = g.clone();
                s.params.send_file_audio = send;
            });
        }
        row(ui, |ui| {
            if ui.button(tr("Reveal File")).clicked()
                && let Err(e) = open_in_os(f.path.parent().unwrap_or(Path::new(".")))
            {
                app.ctrl.set_status(e.to_string(), true);
            }
            let (start, loop_len) = f.loop_range_secs;
            let trimmable = f.looping && loop_len > 0.0 && loop_len < len && app.trim.is_none();
            if ui
                .add_enabled(trimmable, egui::Button::new(tr("Trim to New")))
                .on_hover_text(tr("Saves the loop range as a new file and loads it"))
                .clicked()
            {
                let (tx, rx) = std::sync::mpsc::channel();
                let (path, rate) = (f.path.clone(), f.sample_rate);
                std::thread::spawn(move || tx.send(crate::waveform::trim_to_new(&path, rate, start, loop_len)));
                app.trim = Some(rx);
            }
            if ui.button(tr("Close Audio File")).on_hover_text(tr("Dismiss File Playback")).clicked() {
                app.ctrl.close_file();
            }
        });
    });
}

// ---- recording options (options page) ---------------------------------------------------------

pub fn recording_options(app: &mut SonobusApp, ui: &mut Ui) {
    let st = app.ctrl.vm().state.clone();
    let mask = st.extra.default_recording_options;
    ui.label(tr("Record feature creates the following files:"));
    let mut new_mask = mask;
    row(ui, |ui| {
        for (bit, name) in [(1, tr("Full Mix")), (4, tr("Full Mix without yourself")), (2, tr("Yourself")), (8, tr("Each Connected User"))] {
            let mut on = mask & bit != 0;
            if ui.checkbox(&mut on, name).changed() {
                new_mask = if on { new_mask | bit } else { new_mask & !bit };
            }
        }
    });
    if new_mask != mask {
        app.ctrl.set_recording_options(new_mask); // the engine fills in the mix (PR #78)
    }
    let (mut x, mut met) = (st.extra.clone(), st.params.met_is_recorded);
    let mut post_fx = !x.record_self_pre_fx;
    ui.checkbox(&mut post_fx, tr("Record yourself including input FX"));
    x.record_self_pre_fx = !post_fx;
    ui.checkbox(&mut x.record_self_silence_when_muted, tr("Silence self recording when input is muted"));
    ui.checkbox(&mut met, tr("Metronome output recorded in full mix"));
    ui.checkbox(&mut x.record_finish_open, tr("Open finished recording for playback"));
    row(ui, |ui| {
        let formats = [(2, tr("FLAC")), (3, tr("WAV")), (4, tr("OGG"))];
        let cur = formats.iter().find(|f| f.0 == x.default_recording_format).map_or(tr("FLAC"), |f| f.1);
        egui::ComboBox::from_label(tr("Audio File Format:")).selected_text(cur).show_ui(ui, |ui| {
            for (v, n) in formats {
                ui.selectable_value(&mut x.default_recording_format, v, n);
            }
        });
        ui.add_enabled_ui(x.default_recording_format != 4, |ui| {
            egui::ComboBox::from_label(tr("Bit depth")).selected_text(if x.default_recording_bits_per_sample == 24 { tr("24 bit") } else { tr("16 bit") }).show_ui(ui, |ui| {
                ui.selectable_value(&mut x.default_recording_bits_per_sample, 16, tr("16 bit"));
                ui.selectable_value(&mut x.default_recording_bits_per_sample, 24, tr("24 bit"));
            });
        });
    });
    let dir = app.ctrl.record_dir();
    row(ui, |ui| {
        ui.label(format!("{} {}", tr("Record Location:"), dir.display()));
        if ui.button(tr("Choose Folder")).on_hover_text(tr("Choose the folder for new recordings")).clicked() {
            app.open_page(Page::File(FileAction::RecordDir));
        }
        if ui.button(tr("Reveal")).clicked() {
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = open_in_os(&dir) {
                app.ctrl.set_status(e.to_string(), true);
            }
        }
    });
    if x != st.extra || met != st.params.met_is_recorded {
        app.ctrl.edit(move |s| {
            s.extra = x.clone();
            s.params.met_is_recorded = met;
        });
    }
}

// ---- soundboard -------------------------------------------------------------------------------

/// The board shown: the selected one, else the first.
fn selected(app: &SonobusApp) -> Option<usize> {
    let b = &app.ctrl.vm().soundboards;
    b.selected.filter(|&i| i < b.boards.len()).or((!b.boards.is_empty()).then_some(0))
}

/// Adds a sample for `path` to the shown board (creating one if there is none).
pub fn add_sample_file(app: &mut SonobusApp, path: &Path) {
    let sel = selected(app);
    let name = path.file_stem().map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let url = file_url(path);
    app.ctrl.edit_soundboards(move |b| {
        let i = match sel {
            Some(i) => i,
            None => {
                b.boards.push(Soundboard { name: format!("{} 1", tr("Soundboard")), samples: Vec::new() });
                0
            }
        };
        b.selected = Some(i);
        b.boards[i].samples.push(SoundSample { name, file_url: url, ..SoundSample::default() });
    });
}

fn colour(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn key_text(code: i32) -> String {
    match u8::try_from(code) {
        Ok(c) if c.is_ascii_graphic() => (c as char).to_ascii_uppercase().to_string(),
        _ => String::new(),
    }
}

/// The JUCE key code of a key with a character (letters as upper case, digits).
fn juce_key(k: egui::Key) -> Option<i32> {
    let mut chars = k.name().chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_uppercase() as i32),
        _ => None,
    }
}

fn back(app: &mut SonobusApp, ui: &mut Ui, to: Page) {
    if ui.button(format!("< {}", tr("Back"))).clicked() {
        app.page = to;
    }
}

pub fn soundboard(app: &mut SonobusApp, ui: &mut Ui) {
    if app.page == Page::Soundboard {
        back(app, ui, Page::Main);
    }
    ui.heading(tr("Soundboard"));
    let boards = app.ctrl.vm().soundboards.clone();
    let sel = selected(app);
    if let Some(i) = sel {
        let mut pick = i;
        egui::ComboBox::from_label(tr("Select Soundboard")).selected_text(boards.boards[i].name.clone()).show_ui(ui, |ui| {
            for (j, b) in boards.boards.iter().enumerate() {
                ui.selectable_value(&mut pick, j, &b.name);
            }
        });
        if pick != i {
            app.ctrl.edit_soundboards(move |b| b.selected = Some(pick));
        }
    }
    egui::CollapsingHeader::new(tr("Soundboard Menu")).id_salt("sb_menu").default_open(sel.is_none()).show(ui, |ui| board_menu(app, ui, sel));
    let Some(sel) = sel else { return };

    // controls
    let sends = app.sends();
    let st = &app.ctrl.vm().state;
    let (mut g, mut send) = (st.soundboard_group.clone(), st.params.send_soundboard_audio);
    let (old_g, old_send) = (g.clone(), send);
    row(ui, |ui| {
        if ui.button(tr("Stop all playback")).on_hover_text(tr("Stops all playing samples.")).clicked() {
            app.ctrl.engine().stop_all_samples();
        }
        let (mut keys, mut numeric) = (!boards.hotkeys_muted, boards.hotkeys_allow_numeric);
        if toggle(ui, &mut keys, tr("Toggle hotkeys")).on_hover_text(tr("Toggles whether sound samples can be played using hotkeys.")).changed() {
            app.ctrl.edit_soundboards(move |b| b.hotkeys_muted = !keys);
        }
        if toggle(ui, &mut numeric, tr("Toggle numeric hotkeys")).on_hover_text(tr("Toggles whether sound samples can be played using default numeric hotkeys.")).changed() {
            app.ctrl.edit_soundboards(move |b| b.hotkeys_allow_numeric = numeric);
        }
    });
    row(ui, |ui| {
        if sends {
            ui.checkbox(&mut send, tr("Send Soundboard Playback to All"));
        }
        gain_slider(ui, &mut g.gain, 2.0, tr("Soundboard volume"));
        gain_slider(ui, &mut g.monitor, 2.0, tr("Monitor"));
    });
    if g != old_g || send != old_send {
        app.ctrl.edit(move |s| {
            s.soundboard_group = g.clone();
            s.params.send_soundboard_audio = send;
        });
    }

    // the sample buttons
    let voices = app.ctrl.vm().voices.clone();
    let samples = boards.boards[sel].samples.clone();
    // a grid as wide as the panel (a wrapping row cannot measure the button groups first)
    let cols = ((ui.available_width() + 8.0) / 118.0).floor().max(1.0) as usize;
    for (r, chunk) in samples.chunks(cols).enumerate() {
        ui.horizontal(|ui| {
            for (c, s) in chunk.iter().enumerate() {
                let i = r * cols + c;
                ui.push_id(("sample", i), |ui| sample_button(app, ui, (sel, i), s, voices.iter().find(|v| (v.board, v.sample) == (sel, i))));
            }
        });
    }
    if samples.is_empty() {
        ui.label(RichText::new(tr("Drop audio files here, or add a sample.")).weak());
    }
    if ui.button(format!("+ {}", tr("Add Sample"))).clicked() {
        app.sample_edit = Some(SampleEdit { board: sel, index: None, sample: SoundSample::default(), path: String::new(), capturing_hotkey: false });
        app.page = Page::SampleEdit;
    }
}

fn sample_button(app: &mut SonobusApp, ui: &mut Ui, key: (usize, usize), s: &SoundSample, voice: Option<&sonobus_engine::VoiceStatus>) {
    ui.vertical(|ui| {
        ui.set_width(110.0);
        let fill = colour(s.button_colour);
        let text_colour = if fill.intensity() > 0.5 { Color32::BLACK } else { Color32::WHITE };
        let hotkey = key_text(s.hotkey);
        let label = if hotkey.is_empty() { s.name.clone() } else { format!("{}  [{hotkey}]", s.name) };
        let r = ui.add(
            egui::Button::new(RichText::new(&label).color(text_colour))
                .fill(fill)
                .selected(voice.is_some())
                .stroke(if voice.is_some() { egui::Stroke::new(2.0, Color32::WHITE) } else { egui::Stroke::NONE })
                .min_size(egui::vec2(110.0, 44.0))
                .truncate(),
        );
        r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, voice.is_some(), &s.name));
        let hold = s.button_behaviour_checked() == Some(sonobus_core::files::ButtonBehaviour::Hold);
        let e = app.ctrl.engine();
        let mut press = false;
        if hold {
            let down = r.is_pointer_button_down_on();
            if down && app.held_sample != Some(key) {
                app.held_sample = Some(key);
                press = true;
            } else if !down && app.held_sample == Some(key) {
                app.held_sample = None;
                e.soundboard_release(key);
            }
        } else {
            press = r.clicked();
        }
        if press && let Err(err) = e.soundboard_press(key) {
            app.ctrl.set_status(format!("{}: {err}", tr("Cannot play file")), true);
        }
        let progress = voice.map_or(0.0, |v| v.length_secs.map_or(0.0, |l| (v.position_secs / l.max(0.001)) as f32));
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(progress).desired_width(70.0).desired_height(4.0));
            if ui.small_button(tr("Edit")).on_hover_text(&s.name).clicked() {
                let path = url_path(&s.file_url).or_else(|| s.file_path.clone().map(Into::into)).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                app.sample_edit = Some(SampleEdit { board: key.0, index: Some(key.1), sample: s.clone(), path, capturing_hotkey: false });
                app.page = Page::SampleEdit;
            }
        });
    });
}

fn board_menu(app: &mut SonobusApp, ui: &mut Ui, sel: Option<usize>) {
    text_field(ui, tr("Name of the soundboard:"), &mut app.board_name, tr("Soundboard"), false);
    let name = app.board_name.trim().to_owned();
    row(ui, |ui| {
        if ui.button(tr("New soundboard...")).clicked() {
            let n = app.ctrl.vm().soundboards.boards.len();
            let name = if name.is_empty() { format!("{} {}", tr("Soundboard"), n + 1) } else { name.clone() };
            app.ctrl.edit_soundboards(move |b| {
                b.boards.push(Soundboard { name: name.clone(), samples: Vec::new() });
                b.selected = Some(b.boards.len() - 1);
            });
            app.board_name.clear();
        }
        let Some(i) = sel else { return };
        if ui.add_enabled(!name.is_empty(), egui::Button::new(tr("Rename soundboard..."))).clicked() {
            app.ctrl.edit_soundboards(move |b| b.boards[i].name = name.clone());
            app.board_name.clear();
        }
        if ui.button(tr("Duplicate soundboard...")).clicked() {
            app.ctrl.edit_soundboards(move |b| {
                let mut d = b.boards[i].clone();
                d.name = format!("{} 2", d.name);
                b.boards.push(d);
                b.selected = Some(b.boards.len() - 1);
            });
        }
        if ui.button(tr("Delete soundboard")).clicked() {
            app.confirm_delete_board = true;
        }
    });
    if app.confirm_delete_board
        && let Some(i) = sel
    {
        ui.label(tr("Delete soundboard?"));
        row(ui, |ui| {
            if ui.button(tr("Yes, delete soundboard")).clicked() {
                app.ctrl.engine().stop_all_samples();
                app.ctrl.edit_soundboards(move |b| {
                    b.boards.remove(i);
                    b.selected = (!b.boards.is_empty()).then_some(i.min(b.boards.len().saturating_sub(1)));
                });
                app.confirm_delete_board = false;
            }
            if ui.button(tr("No, keep soundboard")).clicked() {
                app.confirm_delete_board = false;
            }
        });
    }
}

/// Soundboard hotkeys (SoundboardView::processKeystroke): a sample's own key, else 1-9 for the
/// first nine samples when numeric hotkeys are on. Only unmodified keys; the key is consumed.
pub fn hotkeys(app: &mut SonobusApp, ctx: &egui::Context) {
    let boards = &app.ctrl.vm().soundboards;
    let Some(sel) = selected(app) else { return };
    if boards.hotkeys_muted || app.sample_edit.as_ref().is_some_and(|e| e.capturing_hotkey) {
        return;
    }
    let samples = boards.boards[sel].samples.clone();
    let numeric = boards.hotkeys_allow_numeric;
    let keys: Vec<egui::Key> = ctx.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } if modifiers.is_none() => Some(*key),
                _ => None,
            })
            .collect()
    });
    for k in keys {
        let code = juce_key(k);
        let mut hits: Vec<usize> = samples.iter().enumerate().filter(|(_, s)| code.is_some_and(|c| s.hotkey == c || s.hotkey == c + 32)).map(|(i, _)| i).collect();
        if hits.is_empty() && numeric && let Some(d) = code.and_then(|c| u8::try_from(c).ok()).filter(|c| (b'1'..=b'9').contains(c)) {
            hits.extend(Some(usize::from(d - b'1')).filter(|&i| i < samples.len()));
        }
        for i in &hits {
            if let Err(e) = app.ctrl.engine().soundboard_press((sel, *i)) {
                app.ctrl.set_status(format!("{}: {e}", tr("Cannot play file")), true);
            }
        }
        if !hits.is_empty() {
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k));
        }
    }
}

pub fn sample_edit(app: &mut SonobusApp, ui: &mut Ui) {
    let Some(mut e) = app.sample_edit.take() else {
        app.page = Page::Soundboard;
        return;
    };
    back(app, ui, if app.show_soundboard { Page::Main } else { Page::Soundboard });
    if app.page != Page::SampleEdit {
        return; // back: discard
    }
    ui.heading(format!("{}: {}", tr("Soundboard"), if e.index.is_some() { e.sample.name.as_str() } else { tr("Add Sample") }));
    text_field(ui, tr("Name"), &mut e.sample.name, "", false);
    row(ui, |ui| {
        text_field(ui, tr("File"), &mut e.path, "/path/to/file.wav", false);
        if ui.button(tr("Browse")).on_hover_text(tr("Select an audio file...")).clicked() {
            app.sample_edit = Some(e.clone());
            app.open_page(Page::File(FileAction::SampleFile));
        }
    });
    if app.page != Page::SampleEdit {
        return;
    }
    let s = &mut e.sample;
    row(ui, |ui| {
        let l = ui.label(tr("Button colour"));
        let mut rgb = [(s.button_colour >> 16) as u8, (s.button_colour >> 8) as u8, s.button_colour as u8];
        if ui.color_edit_button_srgb(&mut rgb).labelled_by(l.id).changed() {
            s.button_colour = u32::from(rgb[0]) << 16 | u32::from(rgb[1]) << 8 | u32::from(rgb[2]);
        }
        gain_slider(ui, &mut s.gain, 2.0, tr("Gain")).on_hover_text(tr("Playback gain level"));
    });
    ui.label(RichText::new(tr("Playback options")).strong());
    let choice = |ui: &mut Ui, v: &mut i32, names: &[&str]| {
        ui.horizontal_wrapped(|ui| {
            for (i, n) in names.iter().enumerate() {
                ui.selectable_value(v, i as i32, *n);
            }
        });
    };
    choice(ui, &mut s.end_playback_behaviour, &[tr("Stop at End"), tr("Loop at End"), tr("Play Next")]);
    choice(ui, &mut s.replay_behaviour, &[tr("Replay"), tr("Continue")]);
    choice(ui, &mut s.playback_behaviour, &[tr("Simultaneous"), tr("Back to Back"), tr("Background")]);
    choice(ui, &mut s.button_behaviour, &[tr("Toggle"), tr("Hold"), tr("1-shot")]);
    let apply_others = ui.button(tr("Apply to others")).on_hover_text(tr("Apply these options to all samples in the selected soundboard")).clicked();
    row(ui, |ui| {
        ui.label(tr("Hotkey"));
        let text = if e.capturing_hotkey {
            tr("Press a key...").to_owned()
        } else if e.sample.hotkey < 0 {
            tr("Click to change...").to_owned()
        } else {
            key_text(e.sample.hotkey)
        };
        if ui.button(text).clicked() {
            e.capturing_hotkey = true;
        }
        if e.sample.hotkey >= 0 && ui.button(tr("Remove hotkey")).clicked() {
            e.sample.hotkey = -1;
        }
    });
    if e.capturing_hotkey {
        let code = ui.input(|i| {
            i.events.iter().find_map(|ev| match ev {
                egui::Event::Key { key, pressed: true, modifiers, .. } if modifiers.is_none() => juce_key(*key),
                _ => None,
            })
        });
        if let Some(c) = code {
            (e.sample.hotkey, e.capturing_hotkey) = (c, false);
        }
    }
    if apply_others {
        let (b, s) = (e.board, e.sample.clone());
        app.ctrl.edit_soundboards(move |bs| {
            for x in bs.boards.get_mut(b).into_iter().flat_map(|b| b.samples.iter_mut()) {
                x.end_playback_behaviour = s.end_playback_behaviour;
                x.replay_behaviour = s.replay_behaviour;
                x.playback_behaviour = s.playback_behaviour;
                x.button_behaviour = s.button_behaviour;
            }
        });
    }
    let mut done = false;
    row(ui, |ui| {
        let save = if e.index.is_some() { tr("Save") } else { tr("Add Sample") };
        let path = e.path.trim().to_owned();
        if ui.add_enabled(!path.is_empty(), egui::Button::new(RichText::new(save).strong())).clicked() {
            let mut s = e.sample.clone();
            if url_path(&s.file_url).is_none_or(|p| p.to_string_lossy() != path) {
                (s.file_url, s.file_path) = (file_url(Path::new(&path)), None);
            }
            if s.name.trim().is_empty() {
                s.name = Path::new(&path).file_stem().map_or(String::new(), |n| n.to_string_lossy().into_owned());
            }
            let (b, idx) = (e.board, e.index);
            app.ctrl.edit_soundboards(move |bs| {
                if let Some(board) = bs.boards.get_mut(b) {
                    match idx {
                        Some(i) if i < board.samples.len() => board.samples[i] = s.clone(),
                        _ => board.samples.push(s.clone()),
                    }
                }
            });
            done = true;
        }
        if let Some(i) = e.index {
            if ui.button(tr("Delete")).clicked() {
                let b = e.board;
                app.ctrl.engine().stop_sample((b, i));
                app.ctrl.edit_soundboards(move |bs| {
                    if let Some(board) = bs.boards.get_mut(b).filter(|x| i < x.samples.len()) {
                        board.samples.remove(i);
                    }
                });
                done = true;
            }
        } else if ui.button(tr("Cancel")).clicked() {
            done = true;
        }
    });
    if done {
        app.page = if app.show_soundboard && ui.ctx().content_rect().width() >= super::NARROW { Page::Main } else { Page::Soundboard };
    } else {
        app.sample_edit = Some(e);
    }
}
