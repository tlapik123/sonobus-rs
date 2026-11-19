//! The host-automatable parameters: upstream's `AudioProcessorValueTreeState` list
//! (SonobusPluginProcessor.cpp:604-710, all `ParameterID` version 1), in its order, with its
//! ids, names, ranges, skews, steps and plugin-mode defaults, so host automation lanes map 1:1.
//!
//! - Ranges and defaults come from [`sonobus_core::state::Params::plugin`] and its range table;
//!   the skews (which the core model does not need) are written here, from the same upstream
//!   lines.
//! - JUCE's `NormalisableRange` skew maps a value `v` to `((v - start) / (end - start))^skew`;
//!   nice-plug's [`FloatRange::Skewed`] uses the same formula, so normalised values (what hosts
//!   automate) agree. Choices and ints are `index / (count - 1)` in both.
//! - VST3 parameter ids: JUCE uses `String::hashCode()` (`h = 31 * h + char`, as `uint32`) with
//!   the sign bit cleared (`JUCE_USE_STUDIO_ONE_COMPATIBLE_PARAMETERS`); nice-plug's
//!   `wrapper::util::hash_param_id` is the same formula over the id's bytes. For these ASCII ids
//!   the numeric ids are therefore identical to the JUCE plugin's ([`juce_vst3_param_id`]).
//!
//! On top of upstream's list there is a host bypass parameter (JUCE's VST3 wrapper adds one too).

use std::sync::Arc;

use nice_plug::params::InternalParamMut;
use nice_plug::prelude::*;
use sonobus_core::state;

use crate::session::StateField;

/// How a JUCE `AudioParameterFloat` with range `(start, end, interval, skew)` is built here.
fn float(name: &str, default: f32, (min, max, step, skew): (f32, f32, f32, f32)) -> FloatParam {
    let range = if skew == 1.0 {
        FloatRange::Linear { min, max }
    } else {
        FloatRange::Skewed { min, max, factor: skew }
    };
    let p = FloatParam::new(name, default, range);
    if step > 0.0 { p.with_step_size(step) } else { p }
}

/// Gain-type floats show dB, as upstream's `Decibels::toString`.
fn gain(name: &str, default: f32, range: (f32, f32, f32, f32)) -> FloatParam {
    float(name, default, range)
        .with_value_to_string(formatters::v2s_f32_gain_to_db(1))
        .with_string_to_value(formatters::s2v_f32_gain_to_db())
}

fn pan(name: &str, default: f32) -> FloatParam {
    float(name, default, (-1.0, 1.0, 0.0, 1.0))
        .with_value_to_string(formatters::v2s_f32_panning())
        .with_string_to_value(formatters::s2v_f32_panning())
}

fn unit(name: &str, default: f32, range: (f32, f32, f32, f32), unit: &'static str) -> FloatParam {
    float(name, default, range).with_unit(unit)
}

fn percent(name: &str, default: f32) -> FloatParam {
    float(name, default, (0.0, 1.0, 0.0, 1.0))
        .with_value_to_string(formatters::v2s_f32_percentage(0))
        .with_string_to_value(formatters::s2v_f32_percentage())
        .with_unit(" %")
}

/// `defnetbuf` is stored in seconds and shown in ms (upstream: `v*1000 + " ms"`).
fn netbuf(name: &str, default: f32) -> FloatParam {
    float(name, default, (0.0, 1.0, 0.001, 0.5))
        .with_value_to_string(Arc::new(|v| format!("{:.0} ms", v * 1000.0)))
        .with_string_to_value(Arc::new(|s| {
            s.trim_end_matches("ms").trim().parse::<f32>().ok().map(|v| v * 1e-3)
        }))
}

fn choice(name: &str, default: i32, names: &'static [&'static str]) -> IntParam {
    IntParam::new(name, default, IntRange::Linear { min: 0, max: names.len() as i32 - 1 })
        .with_value_to_string(Arc::new(move |v| names.get(v as usize).copied().unwrap_or("").to_owned()))
        .with_string_to_value(Arc::new(move |s| names.iter().position(|n| *n == s).map(|i| i as i32)))
}

fn int(name: &str, default: i32, min: i32, max: i32) -> IntParam {
    IntParam::new(name, default, IntRange::Linear { min, max })
}

fn boolean(name: &str, default: bool) -> BoolParam {
    BoolParam::new(name, default)
}

macro_rules! sb_params {
    ($($field:ident: $ty:ident = $id:literal, $ctor:ident($name:literal $(, $arg:expr)*);)*) => {
        #[derive(Params)]
        pub struct SbParams {
            /// The whole processor state in upstream's format (see [`StateField`]).
            #[persist = "sonobus-state"]
            pub state: StateField,
            $(#[id = $id] pub $field: $ty,)*
            #[id = "bypass"]
            pub bypass: BoolParam,
        }

        impl SbParams {
            /// Upstream's plugin-mode defaults; `state` holds the session.
            pub fn new(state: StateField) -> Self {
                let d = state::Params::plugin();
                SbParams {
                    state,
                    $($field: $ctor($name, d.$field $(, $arg)*),)*
                    bypass: BoolParam::new("Bypass", false)
                        .with_value_to_string(formatters::v2s_bool_bypass())
                        .with_string_to_value(formatters::s2v_bool_bypass())
                        .make_bypass(),
                }
            }

            /// The current values in the core model.
            pub fn to_core(&self) -> state::Params {
                let mut p = state::Params::plugin();
                $(p.$field = self.$field.value();)*
                p
            }

            /// Sets every parameter to `p` without telling the host. Only for before the host
            /// has seen the values (instance creation, `PluginDefault.xml`) and for state loads
            /// (the wrapper makes the host rescan values afterwards).
            pub fn set_from_core(&self, p: &state::Params) {
                // SAFETY: plain atomic stores; nice-plug's wrappers do the same during a state
                // load. No smoother is used, so nothing else needs updating.
                unsafe {
                    $(self.$field._internal_set_plain_value(p.$field);)*
                }
            }

            /// An edit from the UI (or OSC) changed `old` to `new`: sets each changed parameter,
            /// as a host gesture through `setter` (the editor's thread), else silently (no editor
            /// thread: the host rescans values on its own).
            pub fn apply_edit(&self, setter: Option<&ParamSetter>, old: &state::Params, new: &state::Params) {
                $(if old.$field != new.$field {
                    match setter {
                        Some(s) => {
                            s.begin_set_parameter(&self.$field);
                            s.set_parameter(&self.$field, new.$field);
                            s.end_set_parameter(&self.$field);
                        }
                        // SAFETY: a plain atomic store, as in `set_from_core`
                        None => unsafe {
                            self.$field._internal_set_plain_value(new.$field);
                        },
                    }
                })*
            }

            /// `(id, name)` in declaration (host) order.
            pub const IDS: &[(&'static str, &'static str)] = &[$(($id, $name),)*];
        }
    };
}

// Order, ids and names: P.cpp:604-710. Ranges: (start, end, interval, skew).
sb_params! {
    in_gain: FloatParam = "ingain", gain("In Gain", (0.0, 4.0, 0.0, 0.33));
    in_mon_mono_pan: FloatParam = "inmonmonopan", pan("In Pan");
    in_mon_pan1: FloatParam = "inmonpan1", pan("In Pan 1");
    in_mon_pan2: FloatParam = "inmonpan2", pan("In Pan 2");
    dry: FloatParam = "dry", gain("Dry Level", (0.0, 1.0, 0.0, 0.5));
    wet: FloatParam = "wet", gain("Output Level", (0.0, 2.0, 0.0, 0.5));
    def_net_buf: FloatParam = "defnetbuf", netbuf("Default Jitter Buffer Time");
    send_channels: IntParam = "sendchannels", choice("Send Channels", &["Match # Inputs", "Send Mono", "Send Stereo"]);
    met_enabled: BoolParam = "metenabled", boolean("Metronome Enabled");
    send_met_audio: BoolParam = "sendmetaudio", boolean("Send Metronome Audio");
    met_gain: FloatParam = "metgain", gain("Metronome Gain", (0.0, 1.0, 0.0, 0.5));
    met_tempo: FloatParam = "mettempo", unit("Metronome Tempo", (10.0, 400.0, 1.0, 0.5), " bpm");
    send_file_audio: BoolParam = "sendfileaudio", boolean("Send Playback Audio");
    send_soundboard_audio: BoolParam = "sendsoundboardaudio", boolean("Send Soundboard Audio");
    hear_latency_test: BoolParam = "hearlatencytest", boolean("Hear Latency Test");
    met_is_recorded: BoolParam = "metisrecorded", boolean("Record Metronome to File");
    main_reverb_enabled: BoolParam = "mainreverbenabled", boolean("Main Reverb Enabled");
    main_reverb_level: FloatParam = "nmainreverblevel", gain("Main Reverb Level", (0.0, 1.0, 0.0, 0.4));
    main_reverb_size: FloatParam = "mainreverbsize", percent("Main Reverb Size");
    main_reverb_damp: FloatParam = "mainreverbdamp", percent("Main Reverb Damping");
    main_reverb_predelay: FloatParam = "mainreverbpredelay", unit("Pre-Delay Time", (0.0, 100.0, 1.0, 1.0), " ms");
    main_reverb_model: IntParam = "mainreverbmodel", choice("Main Reverb Model", &["Freeverb", "MVerb", "Zita"]);
    main_send_mute: BoolParam = "mastsendmute", boolean("Main Send Mute");
    main_recv_mute: BoolParam = "mastrecvmute", boolean("Main Receive Mute");
    main_in_mute: BoolParam = "mastinmute", boolean("Main In Mute");
    main_monitor_solo: BoolParam = "mastmonsolo", boolean("Main Monitor Solo");
    def_net_auto: IntParam = "defnetauto", choice("Def Auto Net Buffer Mode", &["Off", "Auto-Increase", "Auto-Full", "Initial-Auto"]);
    def_send_qual: IntParam = "defsendqual", int("Def Send Format", 0, 14);
    dynamic_resampling: BoolParam = "dynamicresampling", boolean("Dynamic Resampling");
    reconnect_last: BoolParam = "reconnectlast", boolean("Reconnect Last");
    def_peer_level: FloatParam = "defPeerLevel", gain("Default User Level", (0.0, 1.0, 0.0, 0.5));
    sync_met_host: BoolParam = "syncMetHost", boolean("Sync to Host");
    in_reverb_level: FloatParam = "inreverblevel", gain("Input Reverb Level", (0.0, 1.0, 0.0, 0.4));
    in_reverb_size: FloatParam = "inreverbsize", percent("Input Reverb Size");
    in_reverb_damp: FloatParam = "inreverbdamp", percent("Input Reverb Damping");
    in_reverb_predelay: FloatParam = "inreverbpredelay", unit("Input Reverb Pre-Delay Time", (0.0, 100.0, 1.0, 1.0), " ms");
    sync_met_file: BoolParam = "syncMetFile", boolean("Sync Met to File Playback");
}

/// The VST3 `ParamID` JUCE gives a parameter id (juce_VST3_Wrapper.cpp
/// `generateVSTParamIDForParam`): `String::hashCode()` with the top bit cleared. nice-plug
/// computes the same value (`wrapper::util::hash_param_id`).
pub fn juce_vst3_param_id(id: &str) -> u32 {
    id.chars().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32)) & 0x7fff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    fn params() -> SbParams {
        SbParams::new(StateField(Arc::new(Session::new(None))))
    }

    #[test]
    fn ids_cover_the_core_parameter_list_in_order() {
        let core: Vec<&str> = state::Params::plugin().values().iter().map(|(id, _)| *id).collect();
        let ours: Vec<&str> = SbParams::IDS.iter().map(|(id, _)| *id).collect();
        assert_eq!(ours, core);
        assert_eq!(ours.len(), 37);
        // host order = declaration order (P.cpp:604-710)
        assert_eq!(&ours[..8], &["ingain", "inmonmonopan", "inmonpan1", "inmonpan2", "dry", "wet", "defnetbuf", "sendchannels"]);
        let map = params().param_map();
        let ids: Vec<&str> = map.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(&ids[..37], &ours[..]);
        assert_eq!(ids[37], "bypass");
    }

    #[test]
    fn defaults_and_round_trip_match_the_core_model() {
        let p = params();
        assert_eq!(p.to_core(), state::Params::plugin());
        // plugin-mode defaults (P.cpp:614, 622, 677, 780-783)
        assert_eq!(p.dry.value(), 1.0);
        assert_eq!(p.send_channels.value(), 0);
        assert!(p.sync_met_host.value());
        let mut q = state::Params::plugin();
        q.dry = 0.25;
        q.met_tempo = 133.0;
        q.def_send_qual = 9;
        q.main_reverb_model = 2;
        q.main_send_mute = true;
        p.set_from_core(&q);
        assert_eq!(p.to_core(), q);
        // an edit sets only what it changed (a concurrent automation of others stays)
        let mut r = q.clone();
        r.wet = 1.5;
        unsafe { p.met_tempo._internal_set_plain_value(90.0) };
        p.apply_edit(None, &q, &r);
        assert_eq!((p.wet.value(), p.met_tempo.value()), (1.5, 90.0));
    }

    #[test]
    fn normalisation_matches_juce_skew() {
        let p = params();
        // JUCE convertTo0to1 with skew s: ((v - start) / (end - start))^s
        let juce = |v: f32, start: f32, end: f32, s: f32| ((v - start) / (end - start)).powf(s);
        let cases: [(&FloatParam, f32, f32, f32, f32); 6] = [
            (&p.in_gain, 1.0, 0.0, 4.0, 0.33),
            (&p.dry, 0.5, 0.0, 1.0, 0.5),
            (&p.wet, 1.0, 0.0, 2.0, 0.5),
            (&p.met_tempo, 120.0, 10.0, 400.0, 0.5),
            (&p.main_reverb_level, 0.3, 0.0, 1.0, 0.4),
            (&p.main_reverb_predelay, 20.0, 0.0, 100.0, 1.0),
        ];
        for (param, v, start, end, s) in cases {
            let n = param.preview_normalized(v);
            assert!((n - juce(v, start, end, s)).abs() < 1e-6, "{}: {n}", param.name());
            assert!((param.preview_plain(n) - v).abs() < 1e-3, "{}", param.name());
        }
        // steps snap like JUCE's interval
        assert_eq!(p.met_tempo.preview_plain(p.met_tempo.preview_normalized(120.4)), 120.0);
        assert!((p.def_net_buf.preview_plain(0.3) - 0.09).abs() < 1e-6); // 0.3^2 = 0.09 s
        // choices: index / (count - 1)
        assert_eq!(p.def_net_auto.preview_normalized(2), 2.0 / 3.0);
        assert_eq!(p.def_send_qual.preview_normalized(7), 0.5);
        assert_eq!(p.send_channels.to_string(), "Match # Inputs");
    }

    #[test]
    fn vst3_ids_are_juces_string_hash() {
        // hand-derived: "dry" = ((100*31)+114)*31+121 = 99755
        assert_eq!(juce_vst3_param_id("dry"), 99_755);
        assert_eq!(juce_vst3_param_id("wet"), ((119 * 31 + 101) * 31) + 116);
        // longer ids overflow 32 bits and lose the sign bit; nice-plug's hash_param_id is the
        // same fold over bytes (identical for ASCII)
        let bytes = |id: &str| id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b))) & !(1 << 31);
        for (id, _) in SbParams::IDS {
            assert_eq!(juce_vst3_param_id(id), bytes(id));
            assert!(juce_vst3_param_id(id) < 1 << 31);
        }
    }
}
