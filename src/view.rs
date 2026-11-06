//! Viewer mode: runs on your main PC.
//!
//! Holds the connection to the host: receives its window as JPEG frames for the UI to draw, sends
//! your mouse and keyboard to it, plays the party audio, and sends your microphone.

use crate::audio::{self, Buffer};
use crate::host::run_audio_link;
use crate::proto::{self, Input};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What the UI reads while connected.
pub struct ViewState {
    pub host: String,
    pub connected: AtomicBool,
    /// The newest frame: a counter that increases with each frame, and its JPEG bytes.
    pub frame: Mutex<(u64, Arc<Vec<u8>>)>,
    pub status: Mutex<String>,
    pub mic: Buffer,
    pub party: Buffer,
    pub mic_name: Mutex<Option<String>>,
    pub output_name: Mutex<Option<String>>,
    pub audio_note: Mutex<Option<String>>,
}

pub struct Session {
    pub state: Arc<ViewState>,
    input: Sender<Input>,
    control: TcpStream,
}

impl Session {
    pub fn send(&self, input: Input) {
        let _ = self.input.send(input);
    }

    pub fn disconnect(&self) {
        self.state.connected.store(false, Ordering::SeqCst);
        let _ = self.control.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Connects a socket and completes pairing, with plain-language errors.
fn open(address: &SocketAddr, code: &str) -> Result<TcpStream, String> {
    let mut stream = TcpStream::connect_timeout(address, CONNECT_TIMEOUT).map_err(|e| {
        format!("Couldn't reach the other PC ({e}). Is PartyLink hosting there, and is its network set to Private?")
    })?;
    let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT));
    let _ = stream.set_nodelay(true);
    proto::send_pairing(&mut stream, code).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            "That code isn't right. Check the code shown on the other PC, then wait a few seconds before trying again.".to_string()
        } else {
            format!("The other PC didn't answer ({e}).")
        }
    })?;
    let _ = stream.set_read_timeout(None);
    Ok(stream)
}

pub fn connect(host: &str, code: &str, with_audio: bool) -> Result<Session, String> {
    let host = host.trim();
    let address_for = |port: u16| -> Result<SocketAddr, String> {
        (host, port)
            .to_socket_addrs()
            .map_err(|_| format!("\"{host}\" doesn't look like an IP address."))?
            .next()
            .ok_or_else(|| format!("\"{host}\" doesn't look like an IP address."))
    };
    let control = open(&address_for(proto::CONTROL_PORT)?, code)?;

    let state = Arc::new(ViewState {
        host: host.to_string(),
        connected: AtomicBool::new(true),
        frame: Mutex::new((0, Arc::new(Vec::new()))),
        status: Mutex::new("Waiting for the first picture…".into()),
        mic: Buffer::default(),
        party: Buffer::default(),
        mic_name: Mutex::new(None),
        output_name: Mutex::new(None),
        audio_note: Mutex::new(None),
    });

    {
        let mut reader = control.try_clone().map_err(|e| e.to_string())?;
        let state = state.clone();
        thread::spawn(move || receive(&mut reader, &state));
    }

    let (input, inputs) = mpsc::channel::<Input>();
    {
        let mut writer = control.try_clone().map_err(|e| e.to_string())?;
        let state = state.clone();
        thread::spawn(move || {
            for event in inputs {
                if proto::write_msg(&mut writer, proto::MSG_INPUT, &event.encode()).is_err() {
                    state.connected.store(false, Ordering::SeqCst);
                    break;
                }
            }
        });
    }

    if with_audio {
        let audio_state = state.clone();
        // Audio streams must stay on the thread that created them.
        thread::spawn(move || {
            let _streams = start_audio(&audio_state);
            while audio_state.connected.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
        });
        match address_for(proto::AUDIO_PORT).and_then(|address| open(&address, code)) {
            Ok(stream) => {
                let state = state.clone();
                thread::spawn(move || run_audio_link(stream, &state.mic, &state.party, &state.connected));
            }
            Err(e) => *state.audio_note.lock().unwrap() = Some(format!("Audio didn't connect: {e}")),
        }
    }

    Ok(Session { state, input, control })
}

fn start_audio(state: &ViewState) -> Vec<cpal::Stream> {
    let mut streams = Vec::new();
    let note = |text: String| *state.audio_note.lock().unwrap() = Some(text);
    match audio::default_output() {
        Some(device) => match audio::start_playback(&device, state.party.clone()) {
            Ok(stream) => {
                *state.output_name.lock().unwrap() = Some(audio::device_name(&device));
                streams.push(stream);
            }
            Err(e) => note(format!("Party audio can't play: {e}.")),
        },
        None => note("No speakers or headset found.".into()),
    }
    match audio::default_input() {
        Some(device) => match audio::start_capture(&device, state.mic.clone(), false) {
            Ok(stream) => {
                *state.mic_name.lock().unwrap() = Some(audio::device_name(&device));
                streams.push(stream);
            }
            Err(e) => note(format!("Your mic isn't available: {e}.")),
        },
        None => note("No microphone found.".into()),
    }
    streams
}

fn receive(reader: &mut TcpStream, state: &ViewState) {
    while let Ok((kind, mut payload)) = proto::read_msg(reader) {
        match kind {
            proto::MSG_FRAME if payload.len() > 4 => {
                // The first 4 bytes are the size; the UI reads that from the JPEG itself.
                payload.drain(..4);
                let mut frame = state.frame.lock().unwrap();
                frame.0 += 1;
                frame.1 = Arc::new(payload);
                state.status.lock().unwrap().clear();
            }
            proto::MSG_STATUS => {
                *state.status.lock().unwrap() = String::from_utf8_lossy(&payload).into_owned();
            }
            _ => {}
        }
    }
    state.connected.store(false, Ordering::SeqCst);
}
