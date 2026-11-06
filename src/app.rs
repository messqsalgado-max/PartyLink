//! The app window and the backend for its UI.
//!
//! `ui.html` runs in a WebView2 view. It reads state from a small built-in protocol (`/state`, and
//! `/frame` for the stream) and sends commands as JSON over IPC. Nothing here listens on the network;
//! the protocol exists only inside this window.

use crate::audio::{self, Buffer};
use crate::host::{self, HostState};
use crate::proto::{self, Input};
use crate::setup::{self, Item, Role};
use crate::view::{self, Session};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::windows::{IconExtWindows, WindowExtWindows};
use tao::window::{Icon, WindowBuilder};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use wry::http::{Request, Response};
use wry::{Rect, WebContext, WebViewBuilder, WebViewBuilderExtWindows};

const DEFAULT_WINDOW: &str = "LDPlayer";
const UI_ORIGIN: &str = "http://pl.localhost/";
/// The title bar colour, as 0x00BBGGRR. Matches the app's darkest surface.
const CAPTION_COLOR: u32 = 0x001C_1817;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    role: Option<String>,
    setup_done: bool,
    last_ip: String,
    window: Option<String>,
}

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("PartyLink")
}

fn read_settings() -> Settings {
    std::fs::read(data_dir().join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Written through a temp file and rename, so a crash can't leave a half-written file.
/// The pairing code is deliberately never saved.
fn save_settings(settings: &Settings) {
    let dir = data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(settings) {
        let temp = dir.join("settings.json.tmp");
        if std::fs::write(&temp, bytes).is_ok() {
            let _ = std::fs::rename(temp, dir.join("settings.json"));
        }
    }
}

#[derive(Default)]
struct CheckState {
    running: bool,
    items: Vec<Item>,
}

#[derive(Default)]
struct TaskState {
    running: Option<String>,
    message: String,
    error: bool,
}

#[derive(Default)]
struct ViewUi {
    connecting: bool,
    error: String,
    session: Option<Session>,
    muted: bool,
    deafened: bool,
}

/// Options that come from the command line.
#[derive(Default)]
pub struct Launch {
    pub connect: Option<(String, String)>,
    pub no_audio: bool,
}

pub struct App {
    settings: Mutex<Settings>,
    check: Mutex<CheckState>,
    task: Mutex<TaskState>,
    host: Mutex<Option<Arc<HostState>>>,
    view: Mutex<ViewUi>,
    windows: Mutex<Vec<String>>,
    mic_test: Mutex<Option<(Arc<AtomicBool>, Buffer)>>,
    mic_name: Mutex<Option<String>>,
    output_name: Mutex<Option<String>>,
    no_audio: bool,
    should_exit: AtomicBool,
}

impl App {
    fn new(launch: &Launch) -> Arc<App> {
        let settings = read_settings();
        let app = Arc::new(App {
            settings: Mutex::new(settings.clone()),
            check: Mutex::default(),
            task: Mutex::default(),
            host: Mutex::new(None),
            view: Mutex::default(),
            windows: Mutex::default(),
            mic_test: Mutex::new(None),
            mic_name: Mutex::new(audio::default_input().map(|d| audio::device_name(&d))),
            output_name: Mutex::new(audio::default_output().map(|d| audio::device_name(&d))),
            no_audio: launch.no_audio,
            should_exit: AtomicBool::new(false),
        });
        if settings.setup_done && settings.role.as_deref() == Some("host") {
            app.start_host();
        }
        if let Some((ip, code)) = &launch.connect {
            app.connect(ip.clone(), code.clone());
        }
        app
    }

    fn role(&self) -> Option<Role> {
        self.settings.lock().unwrap().role.as_deref().and_then(Role::parse)
    }

    fn start_host(self: &Arc<Self>) {
        let mut slot = self.host.lock().unwrap();
        if slot.is_none() {
            let query = self.settings.lock().unwrap().window.clone().unwrap_or_else(|| DEFAULT_WINDOW.into());
            *slot = Some(host::start(&query));
        }
    }

    fn run_check(self: &Arc<Self>, host_ip: Option<String>) {
        let Some(role) = self.role() else { return };
        {
            let mut check = self.check.lock().unwrap();
            if check.running {
                return;
            }
            check.running = true;
        }
        let hosting = self.host.lock().unwrap().is_some();
        let app = self.clone();
        thread::spawn(move || {
            let items = setup::check_items(role, host_ip.as_deref(), false, hosting);
            let mut check = app.check.lock().unwrap();
            check.items = items;
            check.running = false;
        });
    }

    /// Runs one setup step in the background, then re-checks.
    fn run_action(self: &Arc<Self>, action: &str) {
        match action {
            "ldplayer" => return setup::open_ldplayer_page(),
            "network" => return setup::open_network_settings(),
            "sound" => return setup::open_sound_settings(),
            "install" | "firewall" | "cable" => {}
            _ => return,
        }
        {
            let mut task = self.task.lock().unwrap();
            if task.running.is_some() {
                return;
            }
            task.running = Some(action.to_string());
            task.message.clear();
            task.error = false;
        }
        let app = self.clone();
        let action = action.to_string();
        thread::spawn(move || {
            let result = match action.as_str() {
                "install" => setup::install_program().map(|installed| {
                    // Keep going from the installed copy, so the firewall rule points at it.
                    if std::env::current_exe().ok().as_deref() != Some(installed.as_path())
                        && Command::new(&installed).spawn().is_ok()
                    {
                        app.should_exit.store(true, Ordering::SeqCst);
                    }
                    "PartyLink is installed.".to_string()
                }),
                "firewall" => std::env::current_exe()
                    .map_err(|e| e.to_string())
                    .and_then(|exe| setup::add_firewall_rule(&exe))
                    .map(|()| "Firewall rule added.".to_string()),
                _ => setup::install_cable(false),
            };
            {
                let mut task = app.task.lock().unwrap();
                task.running = None;
                task.error = result.is_err();
                task.message = result.unwrap_or_else(|e| capitalise(&e));
            }
            app.run_check(None);
        });
    }

    fn connect(self: &Arc<Self>, ip: String, code: String) {
        {
            let mut view = self.view.lock().unwrap();
            if view.connecting {
                return;
            }
            view.connecting = true;
            view.error.clear();
            view.session = None;
        }
        let app = self.clone();
        thread::spawn(move || {
            let result = view::connect(&ip, &code, !app.no_audio);
            let mut view = app.view.lock().unwrap();
            view.connecting = false;
            match result {
                Ok(session) => {
                    session.state.mic.set_muted(view.muted || view.deafened);
                    session.state.party.set_muted(view.deafened);
                    view.session = Some(session);
                    let mut settings = app.settings.lock().unwrap();
                    settings.last_ip = ip.trim().to_string();
                    save_settings(&settings);
                }
                Err(e) => view.error = e,
            }
        });
    }

    fn stop_mic_test(&self) {
        if let Some((stop, _)) = self.mic_test.lock().unwrap().take() {
            stop.store(true, Ordering::SeqCst);
        }
    }

    fn start_mic_test(&self) {
        let mut slot = self.mic_test.lock().unwrap();
        if slot.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let buffer = Buffer::default();
        *slot = Some((stop.clone(), buffer.clone()));
        // Only loudness is read for the meter; the audio is thrown away.
        thread::spawn(move || {
            let Some(device) = audio::default_input() else { return };
            let Ok(_stream) = audio::start_capture(&device, buffer.clone(), false) else { return };
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(50));
                buffer.take_all();
            }
        });
    }

    /// Handles a command from the UI. The UI is our own page and can't navigate anywhere else.
    fn command(self: &Arc<Self>, message: &Value) {
        let text = |key: &str| message.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        match message.get("t").and_then(Value::as_str).unwrap_or("") {
            "input" => {
                if let Some(input) = parse_input(message)
                    && let Some(session) = &self.view.lock().unwrap().session
                {
                    session.send(input);
                }
            }
            "setRole" => {
                let role = text("role");
                if Role::parse(&role).is_none() {
                    return;
                }
                let changed = {
                    let mut settings = self.settings.lock().unwrap();
                    let changed = settings.role.as_deref().is_some_and(|old| old != role);
                    settings.role = Some(role);
                    save_settings(&settings);
                    changed
                };
                self.check.lock().unwrap().items.clear();
                // A host can't stop listening cleanly mid-run, so a role change starts fresh.
                if changed && self.host.lock().unwrap().is_some() {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = Command::new(exe).spawn();
                    }
                    self.should_exit.store(true, Ordering::SeqCst);
                    return;
                }
                self.run_check(None);
            }
            "check" => {
                let ip = text("ip");
                self.run_check((!ip.is_empty()).then_some(ip));
            }
            "action" => self.run_action(&text("action")),
            "finishSetup" => {
                let is_host = {
                    let mut settings = self.settings.lock().unwrap();
                    settings.setup_done = true;
                    save_settings(&settings);
                    settings.role.as_deref() == Some("host")
                };
                self.stop_mic_test();
                if is_host {
                    self.start_host();
                }
            }
            "reopenSetup" => {
                let mut settings = self.settings.lock().unwrap();
                settings.setup_done = false;
                save_settings(&settings);
            }
            "connect" => self.connect(text("ip"), text("code")),
            "disconnect" => {
                let mut view = self.view.lock().unwrap();
                view.session = None;
                view.error.clear();
            }
            "mute" | "deafen" => {
                let mut view = self.view.lock().unwrap();
                if message.get("t").and_then(Value::as_str) == Some("mute") {
                    view.muted = !view.muted;
                    // Unmuting while deafened also undeafens, the way voice apps usually behave.
                    if !view.muted {
                        view.deafened = false;
                    }
                } else {
                    view.deafened = !view.deafened;
                }
                if let Some(session) = &view.session {
                    session.state.mic.set_muted(view.muted || view.deafened);
                    session.state.party.set_muted(view.deafened);
                }
            }
            "micTest" => {
                if message.get("on").and_then(Value::as_bool).unwrap_or(false) {
                    self.start_mic_test();
                } else {
                    self.stop_mic_test();
                }
            }
            "testSound" => {
                thread::spawn(play_test_sound);
            }
            "listWindows" => {
                let app = self.clone();
                thread::spawn(move || *app.windows.lock().unwrap() = host::shareable_windows());
            }
            "shareWindow" => {
                let query = text("query");
                if query.is_empty() {
                    return;
                }
                if let Some(host) = &*self.host.lock().unwrap() {
                    *host.query.lock().unwrap() = query.clone();
                }
                let mut settings = self.settings.lock().unwrap();
                settings.window = Some(query);
                save_settings(&settings);
            }
            _ => {}
        }
    }

    fn state_json(&self) -> Value {
        let settings = self.settings.lock().unwrap().clone();
        let check = self.check.lock().unwrap();
        let task = self.task.lock().unwrap();
        let test_level = self.mic_test.lock().unwrap().as_ref().map(|(_, buffer)| buffer.take_level());
        let host = self.host.lock().unwrap().as_ref().map(|host| {
            json!({
                "code": host.code,
                "ip": host.ip,
                "query": *host.query.lock().unwrap(),
                "viewer": *host.viewer.lock().unwrap(),
                "windowTitle": *host.window_title.lock().unwrap(),
                "partyDevice": *host.party_device.lock().unwrap(),
                "voiceDevice": *host.voice_device.lock().unwrap(),
                "error": *host.error.lock().unwrap(),
                "notes": *host.notes.lock().unwrap(),
            })
        });
        let mut view_ui = self.view.lock().unwrap();
        // A session that dropped by itself becomes a message on the connect screen.
        if view_ui.session.as_ref().is_some_and(|s| !s.state.connected.load(Ordering::SeqCst)) {
            let status = view_ui.session.as_ref().map(|s| s.state.status.lock().unwrap().clone()).unwrap_or_default();
            view_ui.session = None;
            view_ui.error = if status.is_empty() || status.starts_with("Waiting") {
                "The connection to the other PC ended.".into()
            } else {
                status
            };
        }
        let session = view_ui.session.as_ref().map(|session| {
            let state = &session.state;
            json!({
                "host": state.host,
                "serial": state.frame.lock().unwrap().0,
                "status": *state.status.lock().unwrap(),
                "micLevel": state.mic.take_level(),
                "partyLevel": state.party.take_level(),
                "micName": *state.mic_name.lock().unwrap(),
                "outputName": *state.output_name.lock().unwrap(),
                "audioNote": *state.audio_note.lock().unwrap(),
            })
        });
        json!({
            "role": settings.role,
            "setupDone": settings.setup_done,
            "lastIp": settings.last_ip,
            "ip": host::local_ip().map(|ip| ip.to_string()),
            "check": { "running": check.running, "items": check.items },
            "task": { "running": task.running, "message": task.message, "error": task.error },
            "host": host,
            "windows": *self.windows.lock().unwrap(),
            "view": {
                "connecting": view_ui.connecting,
                "error": view_ui.error,
                "session": session,
                "muted": view_ui.muted,
                "deafened": view_ui.deafened,
            },
            "testLevel": test_level,
            "micName": *self.mic_name.lock().unwrap(),
            "outputName": *self.output_name.lock().unwrap(),
        })
    }

    fn serve(&self, request: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
        let respond = |status: u16, kind: &str, body: Cow<'static, [u8]>| {
            Response::builder()
                .status(status)
                .header("Content-Type", kind)
                .header("Cache-Control", "no-store")
                .body(body)
                .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[][..])))
        };
        match request.uri().path() {
            "/" => respond(200, "text/html; charset=utf-8", Cow::Borrowed(include_bytes!("ui.html"))),
            "/state" => respond(200, "application/json", Cow::Owned(self.state_json().to_string().into_bytes())),
            "/frame" => {
                let since = request
                    .uri()
                    .query()
                    .and_then(|q| q.strip_prefix("since="))
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let frame = self.view.lock().unwrap().session.as_ref().map(|s| s.state.frame.lock().unwrap().clone());
                match frame {
                    Some((serial, bytes)) if serial > since => respond(200, "image/jpeg", Cow::Owned(bytes.to_vec())),
                    _ => respond(204, "text/plain", Cow::Borrowed(&[][..])),
                }
            }
            _ => respond(404, "text/plain", Cow::Borrowed(&[][..])),
        }
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Turns a UI input message into a protocol event, clamping anything out of range.
fn parse_input(message: &Value) -> Option<Input> {
    let number = |key: &str| message.get(key).and_then(Value::as_f64);
    let down = message.get("down").and_then(Value::as_bool).unwrap_or(false);
    match message.get("k")?.as_str()? {
        "move" => Some(Input::Move {
            x: number("x").filter(|v| v.is_finite())?.clamp(0.0, 1.0) as f32,
            y: number("y").filter(|v| v.is_finite())?.clamp(0.0, 1.0) as f32,
        }),
        "button" => {
            let button = match number("button")? as u8 {
                0 => proto::BUTTON_LEFT,
                1 => proto::BUTTON_MIDDLE,
                2 => proto::BUTTON_RIGHT,
                _ => return None,
            };
            Some(Input::Button { button, down })
        }
        "scroll" => Some(Input::Scroll { lines: number("lines").filter(|v| v.is_finite())?.clamp(-10.0, 10.0) as f32 }),
        "key" => {
            let vk = number("vk")? as u16;
            (1..=0xFE).contains(&vk).then_some(Input::Key { vk, down })
        }
        "char" => {
            let c = number("c")? as u32;
            (c >= 0x20 && c != 0x7F && char::from_u32(c).is_some()).then_some(Input::Char(c))
        }
        _ => None,
    }
}

/// Plays a short two-note chime so the user can hear which output is in use.
fn play_test_sound() {
    let Some(device) = audio::default_output() else { return };
    let buffer = Buffer::default();
    let Ok(_stream) = audio::start_playback(&device, buffer.clone()) else { return };
    let rate = audio::RATE as f32;
    let tone = |freq: f32, seconds: f32| -> Vec<f32> {
        let len = (rate * seconds) as usize;
        (0..len)
            .map(|i| {
                let t = i as f32 / rate;
                let fade = (1.0 - i as f32 / len as f32).powi(2);
                (t * freq * std::f32::consts::TAU).sin() * 0.2 * fade
            })
            .collect()
    };
    for freq in [660.0, 880.0] {
        buffer.push(&tone(freq, 0.15));
        thread::sleep(Duration::from_millis(150));
    }
    thread::sleep(Duration::from_millis(300));
}

fn show_error_box(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::{PCWSTR, w};
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(None, PCWSTR::from_raw(wide.as_ptr()), w!("PartyLink"), MB_OK | MB_ICONERROR);
    }
}

pub fn run(launch: Launch) -> Result<(), String> {
    if wry::webview_version().is_err() {
        let message = "PartyLink needs the Microsoft Edge WebView2 Runtime, which isn't installed or isn't working on this PC. Search for \"WebView2 Runtime download\" on Microsoft's site, install it, then open PartyLink again.";
        show_error_box(message);
        return Err(message.into());
    }
    let result = run_window(launch);
    if let Err(message) = &result {
        show_error_box(message);
    }
    result
}

fn run_window(launch: Launch) -> Result<(), String> {
    let app = App::new(&launch);
    let event_loop = EventLoopBuilder::new().build();
    let window = WindowBuilder::new()
        .with_title("PartyLink")
        .with_inner_size(LogicalSize::new(1180.0, 740.0))
        .with_min_inner_size(LogicalSize::new(900.0, 580.0))
        .with_window_icon(Icon::from_resource(1, None).ok())
        .build(&event_loop)
        .map_err(|e| format!("Couldn't open the window: {e}"))?;

    // A dark title bar that matches the app, while keeping Windows' own resize and snap behaviour.
    let hwnd = HWND(window.hwnd() as *mut std::ffi::c_void);
    let dark: i32 = 1;
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&dark as *const i32).cast(), 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, (&CAPTION_COLOR as *const u32).cast(), 4);
    }

    let _ = std::fs::create_dir_all(data_dir());
    let mut context = WebContext::new(Some(data_dir().join("webview")));
    let serving = app.clone();
    let commands = app.clone();
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_background_color((23, 24, 28, 255))
        .with_browser_accelerator_keys(false)
        .with_default_context_menus(false)
        .with_custom_protocol("pl".into(), move |_id, request| serving.serve(&request))
        .with_url(UI_ORIGIN)
        // This window only ever shows PartyLink's own page.
        .with_navigation_handler(|url| url.starts_with(UI_ORIGIN))
        .with_new_window_req_handler(|_url, _features| wry::NewWindowResponse::Deny)
        .with_ipc_handler(move |request: Request<String>| {
            if let Ok(message) = serde_json::from_str::<Value>(request.body()) {
                commands.command(&message);
            }
        })
        .build(&window)
        .map_err(|e| format!("Couldn't start the app's view: {e}"))?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(std::time::Instant::now() + Duration::from_millis(250));
        if app.should_exit.load(Ordering::SeqCst) {
            *control_flow = ControlFlow::Exit;
        }
        if let Event::WindowEvent { event, .. } = event {
            match event {
                WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
                WindowEvent::Resized(size) => {
                    let _ = webview.set_bounds(Rect {
                        position: wry::dpi::PhysicalPosition::new(0, 0).into(),
                        size: wry::dpi::PhysicalSize::new(size.width, size.height).into(),
                    });
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_inputs_are_validated() {
        assert_eq!(
            parse_input(&json!({"k": "move", "x": 2.0, "y": -1.0})),
            Some(Input::Move { x: 1.0, y: 0.0 })
        );
        assert_eq!(
            parse_input(&json!({"k": "button", "button": 2, "down": true})),
            Some(Input::Button { button: proto::BUTTON_RIGHT, down: true })
        );
        assert_eq!(parse_input(&json!({"k": "button", "button": 9})), None);
        assert_eq!(parse_input(&json!({"k": "key", "vk": 13, "down": true})), Some(Input::Key { vk: 13, down: true }));
        assert_eq!(parse_input(&json!({"k": "key", "vk": 0})), None);
        assert_eq!(parse_input(&json!({"k": "char", "c": 97})), Some(Input::Char(97)));
        assert_eq!(parse_input(&json!({"k": "char", "c": 13})), None);
        assert_eq!(parse_input(&json!({"k": "scroll", "lines": 99})), Some(Input::Scroll { lines: 10.0 }));
        assert_eq!(parse_input(&json!({"k": "nope"})), None);
        assert_eq!(parse_input(&json!({})), None);
    }

    #[test]
    fn settings_never_contain_a_pairing_code() {
        let text = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!text.contains("code"));
    }

    #[test]
    fn old_settings_files_still_load() {
        let settings: Settings = serde_json::from_str(r#"{"role":"host"}"#).unwrap();
        assert_eq!(settings.role.as_deref(), Some("host"));
        assert!(!settings.setup_done);
    }
}
