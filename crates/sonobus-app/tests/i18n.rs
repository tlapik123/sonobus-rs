//! Translations: how much of our UI upstream's files cover (report under `.locki/tmp/i18n/`),
//! and a full render in Japanese. This binary runs in Japanese (the language is per process).

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use sonobus_app::controller::{AppController, ConnectRequest, ControllerConfig};
use sonobus_app::i18n;
use sonobus_app::ui::{FileAction, FxTarget, Page, SampleEdit, SonobusApp};

/// Every `tr("...")` literal in the crate's sources.
fn ui_strings() -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let src = std::fs::read_to_string(&p).unwrap();
                for (i, _) in src.match_indices("tr(\"") {
                    let rest = &src[i + 4..];
                    let mut s = String::new();
                    let mut chars = rest.chars();
                    while let Some(c) = chars.next() {
                        match c {
                            '"' => break,
                            '\\' => match chars.next() {
                                Some('n') => s.push('\n'),
                                Some(c) => s.push(c),
                                None => break,
                            },
                            c => s.push(c),
                        }
                    }
                    if !s.is_empty() && !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
fn coverage_report() {
    let strings = ui_strings();
    assert!(strings.len() > 300, "{}", strings.len());
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.locki/tmp/i18n");
    std::fs::create_dir_all(&dir).unwrap();
    let mut summary = format!("{} UI strings\n", strings.len());
    for (code, name, _) in i18n::LANGUAGES.iter().skip(2) {
        let t = i18n::parse(i18n::file(code).unwrap());
        let missing: Vec<&String> = strings.iter().filter(|s| t.get(s).is_none()).collect();
        let done = strings.len() - missing.len();
        summary += &format!(
            "{code:8} {name:24} {done:4}/{} ({:.0}%)\n",
            strings.len(),
            100.0 * done as f64 / strings.len() as f64
        );
        let list: Vec<String> = missing.iter().map(|s| format!("{s:?}")).collect();
        std::fs::write(
            dir.join(format!("untranslated_{code}.txt")),
            list.join("\n") + "\n",
        )
        .unwrap();
        // most of what we share with upstream is translated
        assert!(done * 2 > strings.len(), "{code}: {done}");
    }
    std::fs::write(dir.join("coverage.txt"), &summary).unwrap();
    eprintln!("{summary}");
}

/// The texts shown when a Secure session ends are translated in every shipped language.
#[test]
fn session_ended_texts_are_translated() {
    use sonobus_app::secure::ended_text;
    use sonobus_engine::modern::{END_BLOCKED, END_FULL, END_HOST_LEFT, END_REMOVED};
    let reasons = [
        END_REMOVED,
        END_BLOCKED,
        END_HOST_LEFT,
        END_FULL,
        "some other reason",
    ];
    let english: Vec<String> = {
        let _g = i18n::scope(None);
        reasons.map(ended_text).into()
    };
    for (code, _, _) in i18n::LANGUAGES.iter().skip(2) {
        let _g = i18n::scope(Some(code));
        for (r, en) in reasons.iter().zip(&english) {
            let t = ended_text(r);
            assert!(
                !t.is_empty() && t != *en,
                "{code}: {en:?} is not translated"
            );
        }
    }
}

#[test]
fn renders_in_japanese_without_missing_glyphs() {
    assert_eq!(i18n::init("ja"), Some("ja"));
    if !i18n::CJK_FONTS
        .iter()
        .any(|f| std::path::Path::new(f).exists())
    {
        eprintln!("no CJK system font on this machine: Japanese glyph coverage NOT verified");
        return;
    }
    let mute = i18n::tr("Mute All Input");
    assert_ne!(mute, "Mute All Input");
    for size in [(400.0, 800.0), (1200.0, 800.0)] {
        let ctrl = AppController::new(ControllerConfig {
            settings_dir: None,
            use_devices: false,
        })
        .unwrap();
        let app = SonobusApp::new(
            ctrl,
            ConnectRequest {
                user: "テスト".into(),
                ..ConnectRequest::default()
            },
        );
        // kittest panics on any character no installed font has
        let mut h = Harness::builder()
            .with_size(size)
            .build_ui_state(|ui, app: &mut SonobusApp| app.show(ui), app);
        let pages = [
            Page::Connect,
            Page::Main,
            Page::AudioSetup,
            Page::Options,
            Page::Effects(FxTarget::Input(0)),
            Page::Chat,
            Page::File(FileAction::LoadSetup),
            Page::File(FileAction::OpenAudio),
            Page::Soundboard,
            Page::SampleEdit,
            Page::LatencyMatch,
            Page::SuggestGroup,
        ];
        for page in pages {
            if page == Page::SampleEdit {
                h.state_mut().sample_edit = Some(SampleEdit {
                    board: 0,
                    index: None,
                    sample: Default::default(),
                    path: String::new(),
                    capturing_hotkey: false,
                });
            }
            h.state_mut().open_page(page);
            h.run_steps(3);
        }
        h.state_mut().open_page(Page::Main);
        h.run_steps(2);
        h.get_by_label(mute);
    }
}
