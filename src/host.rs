//! Host mode: runs on the PC that has LDPlayer.
//!
//! It shares one window (LDPlayer by default) with a paired viewer, carries out the viewer's clicks
//! and typing on that window, sends what the PC plays (the party) to the viewer, and plays the
//! viewer's microphone into VB-Audio Virtual Cable so LDPlayer can use it as its mic.

use crate::audio::{self, Buffer};
use crate::proto::{self, Input};
use std::collections::HashSet;
use std::io::Cursor;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
use xcap::Window;

const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const MAX_FRAME_WIDTH: u32 = 1280;
const JPEG_QUALITY: u8 = 70;
const CABLE_NAME: &str = "CABLE Input";

/// The shared window's position on this PC's screen, in physical pixels.
#[derive(Clone, Copy, Default)]
struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// Everything the host's screen needs to show. Shared between the worker threads and the UI.
pub struct HostState {
    pub code: String,
    pub ip: Option<String>,
    pub query: Mutex<String>,
    pub viewer: Mutex<Option<String>>,
    pub window_title: Mutex<Option<String>>,
    pub party_device: Mutex<Option<String>>,
    pub voice_device: Mutex<Option<String>>,
    pub error: Mutex<Option<String>>,
    pub notes: Mutex<Vec<String>>,
}

impl HostState {
    fn note(&self, text: impl Into<String>) {
        let mut notes = self.notes.lock().unwrap();
        notes.push(text.into());
        let excess = notes.len().saturating_sub(50);
        notes.drain(..excess);
    }
}

/// Starts hosting in the background. The host runs until the program exits.
pub fn start(window_query: &str) -> Arc<HostState> {
    // Without this, Windows would scale coordinates on high-DPI screens and clicks would land off.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let state = Arc::new(HostState {
        code: proto::new_code(),
        ip: local_ip().map(|ip| ip.to_string()),
        query: Mutex::new(window_query.to_string()),
        viewer: Mutex::new(None),
        window_title: Mutex::new(None),
        party_device: Mutex::new(None),
        voice_device: Mutex::new(None),
        error: Mutex::new(None),
        notes: Mutex::new(Vec::new()),
    });

    // Audio runs for as long as the host does; the network side only forwards it while paired.
    let party_audio = Buffer::default();
    let viewer_mic = Buffer::default();
    {
        let state = state.clone();
        let (party, mic) = (party_audio.clone(), viewer_mic.clone());
        // Audio streams must stay on the thread that created them, so this thread just holds them.
        thread::spawn(move || {
            let _streams = start_host_audio(&state, party, mic);
            loop {
                thread::park();
            }
        });
    }

    let busy = Arc::new(AtomicBool::new(false));
    {
        let state = state.clone();
        let busy = busy.clone();
        thread::spawn(move || audio_listener(&state, busy, party_audio, viewer_mic));
    }
    {
        let state = state.clone();
        thread::spawn(move || control_listener(&state, busy));
    }
    state
}

/// The command-line host: starts hosting and prints what happens.
pub fn run(window_query: &str) -> Result<(), String> {
    let state = start(window_query);
    println!("PartyLink host");
    println!("Pairing code:  {}", state.code);
    println!("This PC's IP:  {}", state.ip.as_deref().unwrap_or("unknown (run ipconfig)"));
    println!("Sharing the window whose title contains \"{window_query}\".");
    let mut printed = 0;
    loop {
        {
            let notes = state.notes.lock().unwrap();
            for note in notes.iter().skip(printed) {
                println!("{note}");
            }
            printed = notes.len();
        }
        if let Some(error) = state.error.lock().unwrap().clone() {
            return Err(error);
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn control_listener(state: &HostState, busy: Arc<AtomicBool>) {
    let listener = match TcpListener::bind(("0.0.0.0", proto::CONTROL_PORT)) {
        Ok(listener) => listener,
        Err(e) => {
            *state.error.lock().unwrap() = Some(format!(
                "Couldn't listen on port {} ({e}). PartyLink may already be running as the Host.",
                proto::CONTROL_PORT
            ));
            return;
        }
    };
    state.note("Waiting for a viewer to connect.");
    let mut failures = 0u32;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        match proto::check_pairing(&mut stream, &state.code) {
            Ok(true) if !busy.swap(true, Ordering::SeqCst) => {
                failures = 0;
                state.note(format!("Viewer connected from {peer}."));
                *state.viewer.lock().unwrap() = Some(peer);
                let _ = stream.set_read_timeout(None);
                let _ = stream.set_nodelay(true);
                serve_viewer(stream, state);
                *state.viewer.lock().unwrap() = None;
                busy.store(false, Ordering::SeqCst);
                state.note("Viewer disconnected. Waiting for a viewer to connect.");
            }
            Ok(true) => {
                let _ = proto::write_msg(&mut stream, proto::MSG_STATUS, b"Another viewer is already connected.");
            }
            // A connection that sends no code (like the checker's reachability test) isn't a guess.
            Err(_) => {}
            Ok(false) => {
                failures += 1;
                state.note(format!("Refused a connection from {peer} (wrong code)."));
                // Slow down guessing: every wrong code costs time, and repeated ones cost more.
                thread::sleep(Duration::from_secs(if failures >= 5 { 30 } else { 2 }));
            }
        }
    }
}

fn start_host_audio(state: &HostState, party_audio: Buffer, viewer_mic: Buffer) -> Vec<cpal::Stream> {
    let mut streams = Vec::new();
    match audio::default_output() {
        Some(device) => match audio::start_capture(&device, party_audio, true) {
            Ok(stream) => {
                *state.party_device.lock().unwrap() = Some(audio::device_name(&device));
                streams.push(stream);
            }
            Err(e) => state.note(format!("Party audio isn't available: {e}.")),
        },
        None => state.note("Party audio isn't available: no output device found."),
    }
    match audio::find_output(CABLE_NAME) {
        Some(device) => match audio::start_playback(&device, viewer_mic) {
            Ok(stream) => {
                *state.voice_device.lock().unwrap() = Some(audio::device_name(&device));
                streams.push(stream);
            }
            Err(e) => state.note(format!("The viewer's voice can't reach LDPlayer: {e}.")),
        },
        None => state.note("VB-Cable isn't installed, so the viewer's voice can't reach LDPlayer."),
    }
    streams
}

fn serve_viewer(stream: TcpStream, state: &HostState) {
    let connected = Arc::new(AtomicBool::new(true));
    let rect = Arc::new(Mutex::new(Rect::default()));
    let Ok(mut writer) = stream.try_clone() else { return };
    thread::scope(|scope| {
        let sender = {
            let connected = connected.clone();
            let rect = rect.clone();
            scope.spawn(move || send_frames(&mut writer, state, &rect, &connected))
        };

        let mut reader = stream;
        let mut held = Held::default();
        while connected.load(Ordering::SeqCst) {
            match proto::read_msg(&mut reader) {
                Ok((proto::MSG_INPUT, payload)) => {
                    if let Some(input) = Input::decode(&payload) {
                        held.track(input);
                        let target = *rect.lock().unwrap();
                        inject(input, target);
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        connected.store(false, Ordering::SeqCst);
        // If the viewer vanishes mid-press, don't leave a button or key stuck down on this PC.
        for input in held.releases() {
            inject(input, Rect { w: 1, h: 1, ..Rect::default() });
        }
        let _ = reader.shutdown(std::net::Shutdown::Both);
        let _ = sender.join();
    });
}

/// The mouse buttons and keys the viewer currently has held down.
#[derive(Default)]
struct Held {
    buttons: HashSet<u8>,
    keys: HashSet<u16>,
}

impl Held {
    fn track(&mut self, input: Input) {
        match input {
            Input::Button { button, down: true } => {
                self.buttons.insert(button);
            }
            Input::Button { button, down: false } => {
                self.buttons.remove(&button);
            }
            Input::Key { vk, down: true } => {
                self.keys.insert(vk);
            }
            Input::Key { vk, down: false } => {
                self.keys.remove(&vk);
            }
            _ => {}
        }
    }

    fn releases(&self) -> Vec<Input> {
        let buttons = self.buttons.iter().map(|&button| Input::Button { button, down: false });
        let keys = self.keys.iter().map(|&vk| Input::Key { vk, down: false });
        buttons.chain(keys).collect()
    }
}

/// The titles of windows that can be shared, for the host's window picker.
pub fn shareable_windows() -> Vec<String> {
    let mut titles: Vec<String> = Window::all()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| !w.is_minimized().unwrap_or(true))
        .filter_map(|w| w.title().ok())
        .filter(|title| !title.trim().is_empty())
        .collect();
    titles.sort();
    titles.dedup();
    titles
}

/// Prints every window the host could share, so the user can pick the right `--window` text.
pub fn list_windows() -> Result<(), String> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let windows = Window::all().map_err(|e| format!("couldn't list windows: {e}"))?;
    println!("{:<10} {:>11}  {:<24} Title", "State", "Size", "App");
    for w in windows {
        let title = w.title().unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let state = if w.is_minimized().unwrap_or(false) { "minimized" } else { "visible" };
        let size = format!("{}x{}", w.width().unwrap_or(0), w.height().unwrap_or(0));
        println!("{state:<10} {size:>11}  {:<24} {title}", w.app_name().unwrap_or_default());
    }
    Ok(())
}

/// Finds the window to share: the largest visible one whose title or app name contains `query`.
pub fn find_window(query: &str) -> Option<Window> {
    let query = query.to_lowercase();
    Window::all()
        .ok()?
        .into_iter()
        .filter(|w| !w.is_minimized().unwrap_or(true))
        .filter(|w| {
            let title = w.title().unwrap_or_default().to_lowercase();
            let app = w.app_name().unwrap_or_default().to_lowercase();
            title.contains(&query) || app.contains(&query)
        })
        .max_by_key(|w| w.width().unwrap_or(0) as u64 * w.height().unwrap_or(0) as u64)
}

fn send_frames(stream: &mut TcpStream, state: &HostState, rect: &Mutex<Rect>, connected: &AtomicBool) {
    let mut last_status = String::new();
    let mut window: Option<Window> = None;
    let mut sharing = String::new();
    while connected.load(Ordering::SeqCst) {
        let started = Instant::now();
        let query = state.query.lock().unwrap().clone();
        if query != sharing {
            sharing = query.clone();
            window = None;
        }
        if window.as_ref().is_none_or(|w| w.is_minimized().unwrap_or(true)) {
            window = find_window(&query);
        }
        *state.window_title.lock().unwrap() = window.as_ref().and_then(|w| w.title().ok());
        let status = match &window {
            Some(w) => match capture(w) {
                Ok((frame, area)) => {
                    *rect.lock().unwrap() = area;
                    if proto::write_msg(stream, proto::MSG_FRAME, &frame).is_err() {
                        break;
                    }
                    String::new()
                }
                Err(e) => {
                    window = None;
                    e
                }
            },
            None => format!("Waiting for a window named \"{query}\" on the other PC. Is it open and not minimized?"),
        };
        if status != last_status {
            // An empty status tells the viewer to clear the previous message.
            if proto::write_msg(stream, proto::MSG_STATUS, status.as_bytes()).is_err() {
                break;
            }
            last_status = status;
        }
        let elapsed = started.elapsed();
        if elapsed < FRAME_INTERVAL {
            thread::sleep(FRAME_INTERVAL - elapsed);
        }
    }
    *state.window_title.lock().unwrap() = None;
    connected.store(false, Ordering::SeqCst);
}

/// Captures the window and returns the encoded frame plus where the window is on screen.
fn capture(window: &Window) -> Result<(Vec<u8>, Rect), String> {
    let area = Rect {
        x: window.x().map_err(|e| e.to_string())?,
        y: window.y().map_err(|e| e.to_string())?,
        w: window.width().map_err(|e| e.to_string())?,
        h: window.height().map_err(|e| e.to_string())?,
    };
    let image = window.capture_image().map_err(|e| format!("Couldn't capture the window: {e}"))?;
    let rgb = image::DynamicImage::ImageRgba8(image).to_rgb8();
    Ok((encode_frame(rgb)?, area))
}

/// Shrinks the image to the maximum frame width and encodes it as `[width u16][height u16][JPEG bytes]`.
fn encode_frame(mut rgb: image::RgbImage) -> Result<Vec<u8>, String> {
    if rgb.width() > MAX_FRAME_WIDTH {
        let height = (rgb.height() as u64 * MAX_FRAME_WIDTH as u64 / rgb.width() as u64) as u32;
        rgb = image::imageops::resize(&rgb, MAX_FRAME_WIDTH, height.max(1), image::imageops::FilterType::Triangle);
    }
    let mut out = Vec::with_capacity(4 + 200_000);
    out.extend_from_slice(&(rgb.width() as u16).to_le_bytes());
    out.extend_from_slice(&(rgb.height() as u16).to_le_bytes());
    let mut cursor = Cursor::new(&mut out);
    cursor.set_position(4);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|e| format!("Couldn't encode the frame: {e}"))?;
    Ok(out)
}

fn inject(input: Input, target: Rect) {
    if target.w == 0 || target.h == 0 {
        return;
    }
    match input {
        Input::Move { x, y } => {
            let px = target.x + (x * (target.w.saturating_sub(1)) as f32).round() as i32;
            let py = target.y + (y * (target.h.saturating_sub(1)) as f32).round() as i32;
            unsafe {
                let _ = SetCursorPos(px, py);
            }
        }
        Input::Button { button, down } => {
            let flags = match (button, down) {
                (proto::BUTTON_LEFT, true) => MOUSEEVENTF_LEFTDOWN,
                (proto::BUTTON_LEFT, false) => MOUSEEVENTF_LEFTUP,
                (proto::BUTTON_RIGHT, true) => MOUSEEVENTF_RIGHTDOWN,
                (proto::BUTTON_RIGHT, false) => MOUSEEVENTF_RIGHTUP,
                (proto::BUTTON_MIDDLE, true) => MOUSEEVENTF_MIDDLEDOWN,
                (proto::BUTTON_MIDDLE, false) => MOUSEEVENTF_MIDDLEUP,
                _ => return,
            };
            send(&[mouse(flags, 0)]);
        }
        Input::Scroll { lines } => {
            // One wheel notch is 120 units.
            let delta = (lines * 120.0).round().clamp(-2400.0, 2400.0) as i32;
            if delta != 0 {
                send(&[mouse(MOUSEEVENTF_WHEEL, delta)]);
            }
        }
        Input::Key { vk, down } => {
            let flags = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
            send(&[key(VIRTUAL_KEY(vk), 0, flags)]);
        }
        Input::Char(c) => {
            let Some(ch) = char::from_u32(c) else { return };
            let mut units = [0u16; 2];
            let mut events = Vec::new();
            for &unit in ch.encode_utf16(&mut units).iter() {
                events.push(key(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE));
                events.push(key(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
            send(&events);
        }
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn send(events: &[INPUT]) {
    unsafe {
        SendInput(events, std::mem::size_of::<INPUT>() as i32);
    }
}

fn audio_listener(state: &HostState, busy: Arc<AtomicBool>, party_audio: Buffer, viewer_mic: Buffer) {
    let listener = match TcpListener::bind(("0.0.0.0", proto::AUDIO_PORT)) {
        Ok(listener) => listener,
        Err(e) => {
            state.note(format!("Audio couldn't listen on port {} ({e}).", proto::AUDIO_PORT));
            return;
        }
    };
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        // Audio is only accepted alongside a paired control connection.
        match proto::check_pairing(&mut stream, &state.code) {
            Ok(true) if busy.load(Ordering::SeqCst) => {}
            Err(_) => continue,
            _ => {
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        }
        let _ = stream.set_read_timeout(None);
        let _ = stream.set_nodelay(true);
        party_audio.take_all();
        viewer_mic.clear();
        run_audio_link(stream, &party_audio, &viewer_mic, &busy);
        viewer_mic.clear();
    }
}

/// Sends captured audio every 20 ms and feeds received audio into `incoming`, until either side
/// disconnects or the control connection ends.
pub fn run_audio_link(stream: TcpStream, outgoing: &Buffer, incoming: &Buffer, alive: &AtomicBool) {
    let Ok(mut reader) = stream.try_clone() else { return };
    let incoming = incoming.clone();
    let receiver = thread::spawn(move || {
        while let Ok((kind, payload)) = proto::read_msg(&mut reader) {
            if kind == proto::MSG_AUDIO {
                incoming.push(&proto::decode_audio(&payload));
            }
        }
    });
    let mut writer = stream;
    while alive.load(Ordering::SeqCst) && !receiver.is_finished() {
        let samples = outgoing.take_all();
        if !samples.is_empty() && proto::write_msg(&mut writer, proto::MSG_AUDIO, &proto::encode_audio(&samples)).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = writer.shutdown(std::net::Shutdown::Both);
    let _ = receiver.join();
}

/// This PC's LAN address, found by asking the OS which interface would route to the internet.
/// No packet is sent: connecting a UDP socket only picks a route.
pub fn local_ip() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_decode_the_way_the_viewer_reads_them() {
        let frame = encode_frame(image::RgbImage::from_pixel(2560, 1440, image::Rgb([200, 40, 40]))).unwrap();
        assert_eq!(u16::from_le_bytes([frame[0], frame[1]]), MAX_FRAME_WIDTH as u16);
        assert_eq!(u16::from_le_bytes([frame[2], frame[3]]), 720);
        let decoded = image::load_from_memory_with_format(&frame[4..], image::ImageFormat::Jpeg).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (MAX_FRAME_WIDTH, 720));
        let pixel = decoded.get_pixel(640, 360);
        assert!(pixel[0] > 170 && pixel[1] < 80 && pixel[2] < 80, "colour changed: {pixel:?}");
    }

    #[test]
    fn held_inputs_are_released_once() {
        let mut held = Held::default();
        held.track(Input::Button { button: proto::BUTTON_LEFT, down: true });
        held.track(Input::Key { vk: 0x11, down: true });
        held.track(Input::Key { vk: 0x41, down: true });
        held.track(Input::Key { vk: 0x41, down: false });
        let releases = held.releases();
        assert_eq!(releases.len(), 2);
        assert!(releases.contains(&Input::Button { button: proto::BUTTON_LEFT, down: false }));
        assert!(releases.contains(&Input::Key { vk: 0x11, down: false }));
    }
}
