//! The wire protocol shared by host and viewer.
//!
//! Every connection starts with a pairing handshake: the viewer sends `MAGIC` followed by the
//! 6-digit code, and the host answers one byte (`1` accepted, `0` refused). After that, both sides
//! exchange messages framed as `[type: u8][length: u32 little-endian][payload]`.

use std::io::{self, Read, Write};
use std::net::TcpStream;

pub const CONTROL_PORT: u16 = 47811;
pub const AUDIO_PORT: u16 = 47812;
pub const MAGIC: &[u8; 5] = b"PLNK1";
pub const CODE_LEN: usize = 6;

/// Largest payload either side will accept, so a bad peer can't make us allocate gigabytes.
const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

pub const MSG_FRAME: u8 = 1;
pub const MSG_INPUT: u8 = 2;
pub const MSG_STATUS: u8 = 3;
pub const MSG_AUDIO: u8 = 4;

pub fn write_msg(stream: &mut TcpStream, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)
}

pub fn read_msg(stream: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// Viewer side of the handshake. Returns an error if the host refused the code.
pub fn send_pairing(stream: &mut TcpStream, code: &str) -> io::Result<()> {
    stream.write_all(MAGIC)?;
    stream.write_all(code.as_bytes())?;
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer)?;
    if answer[0] == 1 {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "the host refused that code"))
    }
}

/// Host side of the handshake. Reads the viewer's code and answers it.
pub fn check_pairing(stream: &mut TcpStream, code: &str) -> io::Result<bool> {
    let mut received = [0u8; MAGIC.len() + CODE_LEN];
    stream.read_exact(&mut received)?;
    let accepted = &received[..MAGIC.len()] == MAGIC
        && constant_time_eq(&received[MAGIC.len()..], code.as_bytes());
    stream.write_all(&[accepted as u8])?;
    Ok(accepted)
}

/// Compares without stopping at the first difference, so timing doesn't reveal how many digits matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A 6-digit pairing code from the OS random source.
pub fn new_code() -> String {
    format!("{:06}", rand::random_range(0..1_000_000u32))
}

/// Input events sent from the viewer to the host. Positions are fractions (0.0–1.0) of the shared
/// window, so the host maps them onto wherever that window currently is on its screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    Move { x: f32, y: f32 },
    Button { button: u8, down: bool },
    Scroll { lines: f32 },
    Key { vk: u16, down: bool },
    Char(u32),
}

pub const BUTTON_LEFT: u8 = 0;
pub const BUTTON_RIGHT: u8 = 1;
pub const BUTTON_MIDDLE: u8 = 2;

impl Input {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9);
        match *self {
            Input::Move { x, y } => {
                out.push(0);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            Input::Button { button, down } => out.extend_from_slice(&[1, button, down as u8]),
            Input::Scroll { lines } => {
                out.push(2);
                out.extend_from_slice(&lines.to_le_bytes());
            }
            Input::Key { vk, down } => {
                out.push(3);
                out.extend_from_slice(&vk.to_le_bytes());
                out.push(down as u8);
            }
            Input::Char(c) => {
                out.push(4);
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Input> {
        let f32_at = |i: usize| Some(f32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?));
        let input = match *bytes.first()? {
            0 => Input::Move { x: f32_at(1)?, y: f32_at(5)? },
            1 => Input::Button { button: *bytes.get(1)?, down: *bytes.get(2)? != 0 },
            2 => Input::Scroll { lines: f32_at(1)? },
            3 => Input::Key {
                vk: u16::from_le_bytes(bytes.get(1..3)?.try_into().ok()?),
                down: *bytes.get(3)? != 0,
            },
            4 => Input::Char(u32::from_le_bytes(bytes.get(1..5)?.try_into().ok()?)),
            _ => return None,
        };
        // Reject NaN or out-of-range positions rather than clicking somewhere unexpected.
        match input {
            Input::Move { x, y } if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) => None,
            Input::Scroll { lines } if !lines.is_finite() => None,
            _ => Some(input),
        }
    }
}

/// Audio travels as mono 48 kHz signed 16-bit samples.
pub fn encode_audio(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes())
        .collect()
}

pub fn decode_audio(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / i16::MAX as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_round_trip() {
        let inputs = [
            Input::Move { x: 0.25, y: 1.0 },
            Input::Button { button: BUTTON_RIGHT, down: true },
            Input::Scroll { lines: -2.0 },
            Input::Key { vk: 0x0D, down: false },
            Input::Char('é' as u32),
        ];
        for input in inputs {
            assert_eq!(Input::decode(&input.encode()), Some(input));
        }
    }

    #[test]
    fn bad_inputs_are_rejected() {
        assert_eq!(Input::decode(&[]), None);
        assert_eq!(Input::decode(&[9]), None);
        assert_eq!(Input::decode(&Input::Move { x: 1.5, y: 0.5 }.encode()), None);
        assert_eq!(Input::decode(&Input::Move { x: f32::NAN, y: 0.5 }.encode()), None);
        assert_eq!(Input::decode(&[0, 1, 2]), None);
    }

    #[test]
    fn codes_compare_exactly() {
        assert!(constant_time_eq(b"123456", b"123456"));
        assert!(!constant_time_eq(b"123456", b"123457"));
        assert!(!constant_time_eq(b"12345", b"123456"));
        let code = new_code();
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn audio_round_trips_closely() {
        let samples = [0.0, 0.5, -0.5, 1.0, -1.0, 2.0];
        let decoded = decode_audio(&encode_audio(&samples));
        for (a, b) in samples.iter().zip(&decoded) {
            assert!((a.clamp(-1.0, 1.0) - b).abs() < 0.001);
        }
    }
}
