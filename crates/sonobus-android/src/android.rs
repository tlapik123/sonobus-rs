//! `android_main`, the egui surface wrapper (insets, soft keyboard text) and the JNI glue with
//! `io.github.sonobusrs.NativeBridge`.

use std::ffi::{CString, c_char, c_int};
use std::sync::atomic::{AtomicI32, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use android_activity::AndroidApp;
use android_activity::input::{ImeOptions, InputType, TextInputAction, TextInputState, TextSpan};
use eframe::egui;
use jni::JNIEnv;
use jni::objects::{GlobalRef, JByteBuffer, JClass, JString, JValue};
use jni::sys::{jboolean, jfloat, jint};
use jni::JavaVM;
use sonobus_app::audio::{DeviceChoice, ExternalSlot};
use sonobus_app::controller::{AppController, ControllerConfig};
use sonobus_app::platform::{AppInfo, CaptureStatus, CaptureTarget, Platform};
use sonobus_app::ui::SonobusApp;
use sonobus_engine::Connection;

use crate::capture::{CaptureEvent, CaptureModel, push_pcm_f32};

// ---- logcat -----------------------------------------------------------------------------------

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

fn log(msg: &str) {
    let text = CString::new(msg.replace('\0', " ")).unwrap_or_default();
    // SAFETY: both are valid NUL-terminated strings; 4 = ANDROID_LOG_INFO.
    unsafe { __android_log_write(4, c"sonobus".as_ptr(), text.as_ptr()) };
}

// ---- process-wide state -----------------------------------------------------------------------

static APP: OnceLock<Arc<Mutex<SonobusApp>>> = OnceLock::new();
/// An invite link that arrived before `android_main` built the app (the launch intent).
static PENDING_URL: Mutex<Option<String>> = Mutex::new(None);
static EXTERNAL: OnceLock<ExternalSlot> = OnceLock::new();
static MODEL: LazyLock<Arc<Mutex<CaptureModel>>> = LazyLock::new(Default::default);
/// Safe-area insets in physical pixels: left, top, right, bottom (bottom includes the keyboard).
static INSETS: [AtomicI32; 4] = [const { AtomicI32::new(0) }; 4];

struct Bridge {
    vm: JavaVM,
    class: GlobalRef,
}

static BRIDGE: OnceLock<Bridge> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Calls a static `NativeBridge` method returning void; Java exceptions are logged and cleared.
fn call(name: &str, sig: &str, args: &[JValue<'_, '_>]) {
    let Some(b) = BRIDGE.get() else {
        log(&format!("NativeBridge.{name}: bridge not initialised"));
        return;
    };
    let Ok(mut env) = b.vm.attach_current_thread_permanently() else { return };
    let class: &JClass<'_> = b.class.as_obj().into();
    if let Err(e) = env.call_static_method(class, name, sig, args) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
        log(&format!("NativeBridge.{name} failed: {e}"));
    }
}

// ---- Platform over JNI ------------------------------------------------------------------------

struct AndroidPlatform;

impl Platform for AndroidPlatform {
    fn capture(&self) -> CaptureStatus {
        lock(&MODEL).status.clone()
    }

    fn apps(&self) -> Vec<AppInfo> {
        lock(&MODEL).apps.clone()
    }

    fn load_apps(&self) {
        call("loadApps", "()V", &[]);
    }

    fn start_capture(&self, target: CaptureTarget) {
        let uid = match &target {
            CaptureTarget::System => -1,
            CaptureTarget::App { uid, .. } => *uid,
        };
        lock(&MODEL).on_event(CaptureEvent::Requested(target));
        call("startCapture", "(I)V", &[JValue::Int(uid)]);
    }

    fn stop_capture(&self) {
        lock(&MODEL).on_event(CaptureEvent::Off);
        call("stopCapture", "()V", &[]);
    }
}

// ---- android_main -----------------------------------------------------------------------------

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    std::panic::set_hook(Box::new(|p| log(&format!("panic: {p}"))));
    if APP.get().is_some() {
        // winit allows one event loop per process; MainActivity kills the process instead of
        // recreating the activity, so this should not happen.
        log("android_main called twice in one process; not starting a second UI");
        return;
    }
    let settings_dir = app.internal_data_path().map(|p| p.join("settings"));
    let ctrl = match AppController::new(ControllerConfig { settings_dir, use_devices: true }) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("cannot start the engine: {e}"));
            return;
        }
    };
    let _ = EXTERNAL.set(ctrl.external_input());
    let form = ctrl.default_request();
    let mut ui = SonobusApp::new(ctrl, form);
    ui.platform = Some(Box::new(AndroidPlatform));
    if let Some(url) = lock(&PENDING_URL).take() {
        ui.open_link(&url);
    }
    let shared = Arc::new(Mutex::new(ui));
    let _ = APP.set(shared.clone());
    keep_alive(shared.clone());

    let options = eframe::NativeOptions { android_app: Some(app.clone()), ..Default::default() };
    let shell = Shell { app: shared, android: app, ime_set: false };
    if let Err(e) = eframe::run_native("SonoBus", options, Box::new(|_cc| Ok(Box::new(shell)))) {
        log(&format!("UI stopped: {e}"));
    }
    log("android_main returned; engine keeps running in the service");
}

/// Ticks the controller while no frame is drawn (activity in the background, screen off), and
/// tells Kotlin when a session starts or ends (Wi-Fi/wake locks).
fn keep_alive(app: Arc<Mutex<SonobusApp>>) {
    std::thread::Builder::new()
        .name("sonobus-keepalive".into())
        .spawn(move || {
            let mut active = false;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let now = {
                    let mut a = lock(&app);
                    a.tick();
                    let vm = a.ctrl.vm();
                    vm.snapshot.connection != Connection::Disconnected || vm.connecting || vm.hosting.is_some()
                };
                if now != active {
                    active = now;
                    call("setSessionActive", "(Z)V", &[JValue::Bool(u8::from(active))]);
                }
            }
        })
        .expect("spawn keep-alive thread");
}

/// The egui surface: applies the safe area and feeds soft-keyboard text to egui.
struct Shell {
    app: Arc<Mutex<SonobusApp>>,
    android: AndroidApp,
    ime_set: bool,
}

impl Shell {
    /// GameActivity delivers soft-keyboard text as an editable buffer, not key events. Commit
    /// it as `Event::Text` once no composition is pending and clear the buffer; with suggestions
    /// off (visible-password input type) keyboards commit every character. Backspace on the
    /// empty buffer arrives as a key event, which winit already forwards.
    fn soft_keyboard_text(&mut self, ctx: &egui::Context) {
        if !self.ime_set {
            self.ime_set = true;
            self.android.set_ime_editor_info(
                InputType::TYPE_CLASS_TEXT
                    | InputType::TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                    | InputType::TYPE_TEXT_FLAG_NO_SUGGESTIONS,
                TextInputAction::Done,
                ImeOptions::IMG_FLAG_NO_EXTRACT_UI,
            );
        }
        let st = self.android.text_input_state();
        if st.text.is_empty() || st.compose_region.is_some() {
            return;
        }
        ctx.input_mut(|i| i.events.push(egui::Event::Text(st.text.clone())));
        self.android.set_text_input_state(TextInputState {
            text: String::new(),
            selection: TextSpan { start: 0, end: 0 },
            compose_region: None,
        });
    }
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.soft_keyboard_text(ui.ctx());
        let ppp = ui.ctx().pixels_per_point();
        let [l, t, r, b] = std::array::from_fn::<f32, 4, _>(|i| INSETS[i].load(Relaxed) as f32 / ppp);
        let full = ui.max_rect();
        let safe = egui::Rect::from_min_max(full.min + egui::vec2(l, t), full.max - egui::vec2(r, b));
        ui.scope_builder(egui::UiBuilder::new().max_rect(safe), |ui| lock(&self.app).show(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        lock(&self.app).ctrl.shutdown();
    }
}

// ---- JNI: Kotlin -> Rust ----------------------------------------------------------------------
// Static methods of `io.github.sonobusrs.NativeBridge`.

/// `nativeInit()`: remembers the VM and the bridge class (FindClass from native threads only
/// sees system classes).
#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_nativeInit(env: JNIEnv<'_>, class: JClass<'_>) {
    if BRIDGE.get().is_some() {
        return;
    }
    if let (Ok(vm), Ok(class)) = (env.get_java_vm(), env.new_global_ref(class)) {
        let _ = BRIDGE.set(Bridge { vm, class });
    }
}

/// `onCaptureData(buf: ByteBuffer, bytes: Int)`: float PCM, interleaved, native order, from
/// the capture thread. No allocation, no copy beyond the engine's input ring.
#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_onCaptureData(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    buf: JByteBuffer<'_>,
    bytes: jint,
) {
    let (Some(slot), Ok(ptr)) = (EXTERNAL.get(), env.get_direct_buffer_address(&buf)) else { return };
    let cap = env.get_direct_buffer_capacity(&buf).unwrap_or(0);
    let n = usize::try_from(bytes).unwrap_or(0).min(cap);
    if ptr.is_null() || n == 0 {
        return;
    }
    // SAFETY: a direct buffer of at least `n` bytes that the capture thread does not touch
    // until this call returns.
    let data = unsafe { std::slice::from_raw_parts(ptr, n) };
    push_pcm_f32(slot, data);
}

fn jstring(env: &mut JNIEnv<'_>, s: &JString<'_>) -> String {
    if s.is_null() {
        return String::new();
    }
    env.get_string(s).map(Into::into).unwrap_or_default()
}

/// `onCaptureState(code, sampleRate, channels, frames, message)`: 0 off, 2 started, 3 stopped.
#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_onCaptureState(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    code: jint,
    rate: jint,
    channels: jint,
    frames: jint,
    message: JString<'_>,
) {
    let msg = jstring(&mut env, &message);
    let ev = match code {
        2 => CaptureEvent::Started {
            sample_rate: rate.max(0) as u32,
            channels: channels.clamp(0, 8) as u16,
            buffer_frames: frames.max(0) as u32,
        },
        3 => CaptureEvent::Stopped(msg.clone()),
        _ => CaptureEvent::Off,
    };
    log(&format!("capture: {ev:?}"));
    lock(&MODEL).on_event(ev);
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_onCaptureLatency(_env: JNIEnv<'_>, _class: JClass<'_>, ms: jfloat) {
    if let Ok(mut m) = MODEL.try_lock() {
        m.on_event(CaptureEvent::Latency(ms));
    }
}

/// `onApps(list)`: one app per line, `uid \t package \t label`.
#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_onApps(mut env: JNIEnv<'_>, _class: JClass<'_>, list: JString<'_>) {
    let list = jstring(&mut env, &list);
    let apps = list
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let uid = f.next()?.parse().ok()?;
            Some(AppInfo { uid, package: f.next()?.into(), label: f.next()?.into() })
        })
        .collect();
    lock(&MODEL).apps = apps;
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_onInsets(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    left: jint,
    top: jint,
    right: jint,
    bottom: jint,
) {
    for (a, v) in INSETS.iter().zip([left, top, right, bottom]) {
        a.store(v.max(0), Relaxed);
    }
}

/// `onPermission(recordAudio)`: the microphone can be opened now; without it, drop the
/// device input so the output (what peers send) still plays.
#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_onPermission(_env: JNIEnv<'_>, _class: JClass<'_>, granted: jboolean) {
    let Some(app) = APP.get() else { return };
    let mut a = lock(app);
    if granted != 0 {
        a.ctrl.retry_audio();
    } else if a.ctrl.audio_setup().input != DeviceChoice::None {
        let setup = sonobus_app::audio::DeviceSetup { input: DeviceChoice::None, ..a.ctrl.audio_setup().clone() };
        let _ = a.ctrl.set_audio_setup(setup);
        a.ctrl.set_status("Microphone permission denied: input off (System sound still works once allowed)", true);
    }
}

/// `onNetworkChanged(available)`: logged for now; the future iroh transport rebinds here.
#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_onNetworkChanged(_env: JNIEnv<'_>, _class: JClass<'_>, available: jboolean) {
    log(&format!("network changed (available: {})", available != 0));
}

/// `onOpenUrl(url)`: a `sonobus2://join/...` link (QR code scanned with the camera app, a
/// tapped link). The UI shows what it names and asks before joining.
#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_onOpenUrl(mut env: JNIEnv<'_>, _class: JClass<'_>, url: JString<'_>) {
    let url = jstring(&mut env, &url);
    match APP.get() {
        Some(app) => lock(app).open_link(&url),
        None => *lock(&PENDING_URL) = Some(url),
    }
}

/// `nativeSave()`: writes the settings (activity stopped, app quitting).
#[unsafe(no_mangle)]
extern "system" fn Java_io_github_sonobusrs_NativeBridge_nativeSave(_env: JNIEnv<'_>, _class: JClass<'_>) {
    if let Some(app) = APP.get() {
        lock(app).ctrl.shutdown();
    }
}
