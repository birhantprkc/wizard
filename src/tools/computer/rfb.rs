//! A small RFB (VNC) client: enough of RFC 6143 to watch a remote desktop
//! and drive its pointer and keyboard.
//!
//! The `computer` tool's VM backend uses it for screenshots and input, and
//! Wizard GUI's live screen panel compiles this same file (by `#[path]`) to
//! show the VM and forward the user's mouse and keyboard when they take
//! control. It depends on nothing but `std` so both can.
//!
//! Scope, on purpose: protocol 3.3 to 3.8, the "None" security type only (the
//! VM's server listens on 127.0.0.1), a 32-bit true-colour pixel format the
//! client picks, and the Raw, CopyRect and DesktopSize encodings. The
//! framebuffer is kept as tightly packed RGBA.

use std::io::{self, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Left button bit in a pointer event's button mask.
pub const BUTTON_LEFT: u8 = 1;
/// Middle button bit.
pub const BUTTON_MIDDLE: u8 = 1 << 1;
/// Right button bit.
pub const BUTTON_RIGHT: u8 = 1 << 2;
/// Wheel up: press and release this "button" once per notch.
pub const WHEEL_UP: u8 = 1 << 3;
/// Wheel down.
pub const WHEEL_DOWN: u8 = 1 << 4;
/// Wheel left.
pub const WHEEL_LEFT: u8 = 1 << 5;
/// Wheel right.
pub const WHEEL_RIGHT: u8 = 1 << 6;

const ENCODING_RAW: i32 = 0;
const ENCODING_COPY_RECT: i32 = 1;
const ENCODING_DESKTOP_SIZE: i32 = -223;

fn protocol_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// What one server message did to the client's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A framebuffer update finished; these rectangles changed.
    Updated(Vec<Rect>),
    /// The desktop changed size. The framebuffer is cleared.
    Resized { width: u16, height: u16 },
    /// The server rang the bell.
    Bell,
    /// The server's clipboard changed.
    CutText(String),
    /// A message with nothing for the caller (a colour map entry).
    Other,
}

/// A rectangle of the framebuffer, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// The write half of a connection. Cheap to clone and safe to use from
/// another thread while the owner of the [`Client`] reads updates, which is
/// how the GUI forwards input while its reader thread paints frames.
#[derive(Clone)]
pub struct Input {
    stream: Arc<Mutex<TcpStream>>,
}

impl Input {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let mut stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
        stream.write_all(bytes)?;
        stream.flush()
    }

    /// Move the pointer to `(x, y)` with `buttons` held (a mask of the
    /// `BUTTON_*` and `WHEEL_*` bits).
    pub fn pointer(&self, x: u16, y: u16, buttons: u8) -> io::Result<()> {
        let [x0, x1] = x.to_be_bytes();
        let [y0, y1] = y.to_be_bytes();
        self.send(&[5, buttons, x0, x1, y0, y1])
    }

    /// Press (`down`) or release an X keysym.
    pub fn key(&self, keysym: u32, down: bool) -> io::Result<()> {
        let [k0, k1, k2, k3] = keysym.to_be_bytes();
        self.send(&[4, u8::from(down), 0, 0, k0, k1, k2, k3])
    }

    /// Ask for the region `rect`; `incremental` asks only for what changed
    /// since the last update.
    pub fn request_update(&self, incremental: bool, rect: Rect) -> io::Result<()> {
        let mut msg = vec![3, u8::from(incremental)];
        for v in [rect.x, rect.y, rect.width, rect.height] {
            msg.extend_from_slice(&v.to_be_bytes());
        }
        self.send(&msg)
    }

    /// Press and release `button` at `(x, y)` `count` times.
    pub fn click(&self, x: u16, y: u16, button: u8, count: u32) -> io::Result<()> {
        for _ in 0..count.max(1) {
            self.pointer(x, y, button)?;
            self.pointer(x, y, 0)?;
        }
        Ok(())
    }

    /// Type `text`, one keysym per character.
    pub fn type_text(&self, text: &str) -> io::Result<()> {
        for c in text.chars() {
            let sym = keysym_for_char(c);
            self.key(sym, true)?;
            self.key(sym, false)?;
        }
        Ok(())
    }

    /// Press a chord like `ctrl+c` or `Return`: modifiers down in order, the
    /// key down and up, modifiers up in reverse.
    pub fn chord(&self, chord: &str) -> io::Result<()> {
        let (mods, key) = parse_chord(chord).map_err(protocol_error)?;
        for m in &mods {
            self.key(*m, true)?;
        }
        self.key(key, true)?;
        self.key(key, false)?;
        for m in mods.iter().rev() {
            self.key(*m, false)?;
        }
        Ok(())
    }
}

/// A connected, initialised RFB session.
pub struct Client {
    reader: BufReader<TcpStream>,
    input: Input,
    width: u16,
    height: u16,
    name: String,
    /// Tightly packed RGBA, `width * height * 4` bytes.
    framebuffer: Vec<u8>,
}

impl Client {
    /// Connect to `address` (`host:port`), run the handshake, and set the
    /// pixel format and encodings. `timeout` bounds the connect and every
    /// later read.
    pub fn connect(address: &str, timeout: Duration) -> io::Result<Self> {
        let addr = address
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| protocol_error(format!("{address} did not resolve")))?;
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        Self::handshake(stream)
    }

    fn handshake(stream: TcpStream) -> io::Result<Self> {
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream.try_clone()?);

        let mut version = [0u8; 12];
        reader.read_exact(&mut version)?;
        let minor = parse_version(&version)?;
        let minor = minor.min(8);
        writer.write_all(format!("RFB 003.{minor:03}\n").as_bytes())?;

        if minor >= 7 {
            let count = read_u8(&mut reader)?;
            if count == 0 {
                let reason = read_string(&mut reader)?;
                return Err(protocol_error(format!("VNC server refused: {reason}")));
            }
            let mut types = vec![0u8; usize::from(count)];
            reader.read_exact(&mut types)?;
            if !types.contains(&1) {
                return Err(protocol_error(format!(
                    "VNC server wants authentication (security types {types:?}); only servers \
                     without a password are supported, so bind it to 127.0.0.1 instead"
                )));
            }
            writer.write_all(&[1])?;
        } else {
            let kind = read_u32(&mut reader)?;
            if kind == 0 {
                let reason = read_string(&mut reader)?;
                return Err(protocol_error(format!("VNC server refused: {reason}")));
            }
            if kind != 1 {
                return Err(protocol_error(format!(
                    "VNC server wants security type {kind}; only servers without a password \
                     are supported"
                )));
            }
        }
        // 3.8 always reports the result; 3.7 and earlier skip it for None.
        if minor >= 8 {
            let result = read_u32(&mut reader)?;
            if result != 0 {
                let reason = read_string(&mut reader).unwrap_or_default();
                return Err(protocol_error(format!("VNC security failed: {reason}")));
            }
        }

        // ClientInit: share the desktop with other viewers, so the GUI panel
        // and the agent can both be connected.
        writer.write_all(&[1])?;

        let width = read_u16(&mut reader)?;
        let height = read_u16(&mut reader)?;
        let mut server_format = [0u8; 16];
        reader.read_exact(&mut server_format)?;
        let name = read_string(&mut reader)?;

        // SetPixelFormat: 32 bpp, depth 24, little endian, true colour, red
        // in the low byte, so each pixel lands in memory as R, G, B, X.
        let mut set_format = vec![0u8, 0, 0, 0];
        set_format.extend_from_slice(&[32, 24, 0, 1]);
        for max in [255u16, 255, 255] {
            set_format.extend_from_slice(&max.to_be_bytes());
        }
        set_format.extend_from_slice(&[0, 8, 16, 0, 0, 0]);
        writer.write_all(&set_format)?;

        // SetEncodings.
        let encodings = [ENCODING_COPY_RECT, ENCODING_RAW, ENCODING_DESKTOP_SIZE];
        let mut set_encodings = vec![2u8, 0];
        set_encodings.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
        for e in encodings {
            set_encodings.extend_from_slice(&e.to_be_bytes());
        }
        writer.write_all(&set_encodings)?;
        writer.flush()?;

        Ok(Self {
            reader,
            input: Input {
                stream: Arc::new(Mutex::new(writer)),
            },
            width,
            height,
            name,
            framebuffer: vec![0; usize::from(width) * usize::from(height) * 4],
        })
    }

    /// Desktop width in pixels.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Desktop height in pixels.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// The name the server gave its desktop.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The whole desktop as tightly packed RGBA.
    pub fn framebuffer(&self) -> &[u8] {
        &self.framebuffer
    }

    /// A handle for sending input, usable from another thread.
    pub fn input(&self) -> Input {
        self.input.clone()
    }

    /// The whole desktop as a rectangle.
    pub fn full(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }

    /// Change how long a read may block before it fails with `WouldBlock` or
    /// `TimedOut`.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.reader.get_ref().set_read_timeout(timeout)
    }

    /// Fetch the whole desktop: a non-incremental request, then messages until
    /// the update that answers it.
    pub fn refresh(&mut self) -> io::Result<()> {
        self.input.request_update(false, self.full())?;
        loop {
            match self.read_event()? {
                Event::Updated(_) => return Ok(()),
                Event::Resized { .. } => {
                    self.input.request_update(false, self.full())?;
                }
                Event::Bell | Event::CutText(_) | Event::Other => {}
            }
        }
    }

    /// Read and apply one server message.
    pub fn read_event(&mut self) -> io::Result<Event> {
        let kind = read_u8(&mut self.reader)?;
        match kind {
            0 => self.read_update(),
            1 => {
                let mut header = [0u8; 5];
                self.reader.read_exact(&mut header)?;
                let count = u16::from_be_bytes([header[3], header[4]]);
                skip(&mut self.reader, usize::from(count) * 6)?;
                Ok(Event::Other)
            }
            2 => Ok(Event::Bell),
            3 => {
                skip(&mut self.reader, 3)?;
                let text = read_string(&mut self.reader)?;
                Ok(Event::CutText(text))
            }
            other => Err(protocol_error(format!(
                "unsupported VNC server message type {other}"
            ))),
        }
    }

    fn read_update(&mut self) -> io::Result<Event> {
        skip(&mut self.reader, 1)?;
        let count = read_u16(&mut self.reader)?;
        let mut rects = Vec::with_capacity(usize::from(count));
        let mut resized = None;
        for _ in 0..count {
            let rect = Rect {
                x: read_u16(&mut self.reader)?,
                y: read_u16(&mut self.reader)?,
                width: read_u16(&mut self.reader)?,
                height: read_u16(&mut self.reader)?,
            };
            let encoding = read_u32(&mut self.reader)? as i32;
            match encoding {
                ENCODING_RAW => {
                    self.check_bounds(rect)?;
                    let row = usize::from(rect.width) * 4;
                    let stride = usize::from(self.width) * 4;
                    for dy in 0..usize::from(rect.height) {
                        let start = (usize::from(rect.y) + dy) * stride + usize::from(rect.x) * 4;
                        self.reader
                            .read_exact(&mut self.framebuffer[start..start + row])?;
                    }
                    // The X byte is padding; make the buffer honest RGBA.
                    for dy in 0..usize::from(rect.height) {
                        let start = (usize::from(rect.y) + dy) * stride + usize::from(rect.x) * 4;
                        let (pixels, _) = self.framebuffer[start..start + row].as_chunks_mut::<4>();
                        for px in pixels {
                            px[3] = 255;
                        }
                    }
                    rects.push(rect);
                }
                ENCODING_COPY_RECT => {
                    let src_x = read_u16(&mut self.reader)?;
                    let src_y = read_u16(&mut self.reader)?;
                    self.check_bounds(rect)?;
                    self.check_bounds(Rect {
                        x: src_x,
                        y: src_y,
                        ..rect
                    })?;
                    self.copy_rect(src_x, src_y, rect);
                    rects.push(rect);
                }
                ENCODING_DESKTOP_SIZE => {
                    self.width = rect.width;
                    self.height = rect.height;
                    self.framebuffer =
                        vec![0; usize::from(rect.width) * usize::from(rect.height) * 4];
                    resized = Some((rect.width, rect.height));
                }
                other => {
                    return Err(protocol_error(format!(
                        "VNC server sent encoding {other}, which was not requested"
                    )));
                }
            }
        }
        Ok(match resized {
            Some((width, height)) => Event::Resized { width, height },
            None => Event::Updated(rects),
        })
    }

    fn check_bounds(&self, rect: Rect) -> io::Result<()> {
        let fits = u32::from(rect.x) + u32::from(rect.width) <= u32::from(self.width)
            && u32::from(rect.y) + u32::from(rect.height) <= u32::from(self.height);
        if fits {
            Ok(())
        } else {
            Err(protocol_error(format!(
                "VNC rectangle {rect:?} is outside the {}x{} desktop",
                self.width, self.height
            )))
        }
    }

    fn copy_rect(&mut self, src_x: u16, src_y: u16, dst: Rect) {
        let stride = usize::from(self.width) * 4;
        let row = usize::from(dst.width) * 4;
        // Through a scratch copy: source and destination may overlap.
        let mut scratch = Vec::with_capacity(row * usize::from(dst.height));
        for dy in 0..usize::from(dst.height) {
            let start = (usize::from(src_y) + dy) * stride + usize::from(src_x) * 4;
            scratch.extend_from_slice(&self.framebuffer[start..start + row]);
        }
        for dy in 0..usize::from(dst.height) {
            let start = (usize::from(dst.y) + dy) * stride + usize::from(dst.x) * 4;
            self.framebuffer[start..start + row]
                .copy_from_slice(&scratch[dy * row..(dy + 1) * row]);
        }
    }
}

fn parse_version(version: &[u8; 12]) -> io::Result<u32> {
    let text = std::str::from_utf8(version).map_err(|_| protocol_error("not a VNC server"))?;
    let rest = text
        .strip_prefix("RFB ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .ok_or_else(|| protocol_error(format!("not a VNC server: {text:?}")))?;
    let (major, minor) = rest
        .split_once('.')
        .ok_or_else(|| protocol_error(format!("bad RFB version {rest:?}")))?;
    let major: u32 = major
        .parse()
        .map_err(|_| protocol_error(format!("bad RFB version {rest:?}")))?;
    let minor: u32 = minor
        .parse()
        .map_err(|_| protocol_error(format!("bad RFB version {rest:?}")))?;
    if major != 3 || minor < 3 {
        return Err(protocol_error(format!("unsupported RFB version {rest}")));
    }
    Ok(minor)
}

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// A u32 length then that many bytes, capped so a hostile length cannot
/// allocate the machine.
fn read_string(r: &mut impl Read) -> io::Result<String> {
    let len = read_u32(r)? as usize;
    if len > 1 << 20 {
        return Err(protocol_error(format!("VNC string of {len} bytes")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn skip(r: &mut impl Read, n: usize) -> io::Result<()> {
    let copied = io::copy(&mut r.take(n as u64), &mut io::sink())?;
    if copied as usize == n {
        Ok(())
    } else {
        Err(io::ErrorKind::UnexpectedEof.into())
    }
}

/// The X keysym that types `c`.
pub fn keysym_for_char(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        ' '..='~' => c as u32,
        // Latin-1 keysyms equal their code points; everything else uses the
        // Unicode keysym range.
        '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 | c as u32,
    }
}

/// The keysym for a modifier name (`ctrl`, `shift`, `alt`, `super`, ...).
pub fn modifier_keysym(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => 0xffe3,
        "shift" => 0xffe1,
        "alt" | "option" | "opt" => 0xffe9,
        "meta" | "super" | "win" | "cmd" | "command" => 0xffeb,
        _ => return None,
    })
}

/// The keysym for a key name (`Return`, `Tab`, `F5`, `a`, `7`, ...),
/// case-insensitive.
pub fn key_keysym(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "return" | "enter" => 0xff0d,
        "tab" => 0xff09,
        "space" | "spacebar" => 0x20,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "escape" | "esc" => 0xff1b,
        "up" => 0xff52,
        "down" => 0xff54,
        "left" => 0xff51,
        "right" => 0xff53,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "pgup" | "prior" => 0xff55,
        "pagedown" | "pgdn" | "next" => 0xff56,
        "insert" | "ins" => 0xff63,
        "capslock" => 0xffe5,
        "minus" => 0x2d,
        "equal" => 0x3d,
        "comma" => 0x2c,
        "period" | "dot" => 0x2e,
        "slash" => 0x2f,
        "backslash" => 0x5c,
        "semicolon" => 0x3b,
        "apostrophe" => 0x27,
        "grave" => 0x60,
        "leftbracket" => 0x5b,
        "rightbracket" => 0x5d,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && (' '..='~').contains(&c)
    {
        // A single printable character types itself. Letters are sent
        // lowercase: shift is a modifier in the chord, not a different key.
        return Some(c.to_ascii_lowercase() as u32);
    }
    if let Some(rest) = lower.strip_prefix('f')
        && let Ok(n) = rest.parse::<u32>()
        && (1..=12).contains(&n)
    {
        return Some(0xffbe + n - 1);
    }
    None
}

/// Split a chord like `ctrl+shift+t` into modifier keysyms and the key's.
pub fn parse_chord(chord: &str) -> Result<(Vec<u32>, u32), String> {
    let mut mods = Vec::new();
    let mut key = None;
    // A chord ending in "+" means the plus key itself ("ctrl++").
    let (body, plus) = match chord.strip_suffix("++") {
        Some(body) => (body, true),
        None => (chord, chord == "+"),
    };
    for token in body.split('+').map(str::trim).filter(|t| !t.is_empty()) {
        if let Some(sym) = modifier_keysym(token) {
            mods.push(sym);
        } else if let Some(sym) = key_keysym(token) {
            if key.replace(sym).is_some() {
                return Err(format!(
                    "chord '{chord}' has more than one non-modifier key"
                ));
            }
        } else {
            return Err(format!("unknown key '{token}' in chord '{chord}'"));
        }
    }
    if plus && key.replace(u32::from(b'+')).is_some() {
        return Err(format!(
            "chord '{chord}' has more than one non-modifier key"
        ));
    }
    let key = key.ok_or_else(|| format!("chord '{chord}' has no main key"))?;
    Ok((mods, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A scripted server: speaks 3.8, offers None, serves a 4x2 desktop, and
    /// answers the first update request with two raw pixels and a copy.
    fn fake_server() -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"RFB 003.008\n").unwrap();
            let mut v = [0u8; 12];
            s.read_exact(&mut v).unwrap();
            assert_eq!(&v, b"RFB 003.008\n");
            s.write_all(&[1, 1]).unwrap();
            let mut pick = [0u8; 1];
            s.read_exact(&mut pick).unwrap();
            assert_eq!(pick[0], 1);
            s.write_all(&0u32.to_be_bytes()).unwrap();
            let mut shared = [0u8; 1];
            s.read_exact(&mut shared).unwrap();
            let mut init = Vec::new();
            init.extend_from_slice(&4u16.to_be_bytes());
            init.extend_from_slice(&2u16.to_be_bytes());
            init.extend_from_slice(&[0u8; 16]);
            init.extend_from_slice(&4u32.to_be_bytes());
            init.extend_from_slice(b"test");
            s.write_all(&init).unwrap();
            // SetPixelFormat (20) + SetEncodings (4 + 3*4) + update request (10).
            let mut client = vec![0u8; 20 + 16 + 10];
            s.read_exact(&mut client).unwrap();
            let mut update = vec![0u8, 0];
            update.extend_from_slice(&2u16.to_be_bytes());
            // Raw 2x1 at (0,0): red, green.
            for v in [0u16, 0, 2, 1] {
                update.extend_from_slice(&v.to_be_bytes());
            }
            update.extend_from_slice(&0i32.to_be_bytes());
            update.extend_from_slice(&[255, 0, 0, 0, 0, 255, 0, 0]);
            // CopyRect 2x1 to (2,1) from (0,0).
            for v in [2u16, 1, 2, 1] {
                update.extend_from_slice(&v.to_be_bytes());
            }
            update.extend_from_slice(&1i32.to_be_bytes());
            update.extend_from_slice(&0u16.to_be_bytes());
            update.extend_from_slice(&0u16.to_be_bytes());
            s.write_all(&update).unwrap();
            // Then read whatever input the test sends until it hangs up.
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest);
            [client, rest].concat()
        });
        (addr, handle)
    }

    #[test]
    fn handshakes_reads_raw_and_copyrect_and_sends_input() {
        let (addr, server) = fake_server();
        let mut client = Client::connect(&addr, Duration::from_secs(5)).expect("connect");
        assert_eq!((client.width(), client.height()), (4, 2));
        assert_eq!(client.name(), "test");
        client.refresh().expect("first frame");
        let fb = client.framebuffer();
        assert_eq!(&fb[0..8], &[255, 0, 0, 255, 0, 255, 0, 255]);
        // Row 1 starts at byte 16; pixels 2 and 3 are the copy.
        assert_eq!(&fb[16 + 8..16 + 16], &[255, 0, 0, 255, 0, 255, 0, 255]);

        let input = client.input();
        input.pointer(3, 1, BUTTON_LEFT).unwrap();
        input.key(0xff0d, true).unwrap();
        drop(input);
        drop(client);
        let sent = server.join().unwrap();

        // SetPixelFormat asks for 32bpp true colour with red in the low byte.
        assert_eq!(sent[0], 0);
        assert_eq!(&sent[4..8], &[32, 24, 0, 1]);
        assert_eq!(&sent[14..17], &[0, 8, 16]);
        // SetEncodings lists three.
        assert_eq!(&sent[20..24], &[2, 0, 0, 3]);
        // The refresh was non-incremental and covered the desktop.
        assert_eq!(&sent[36..46], &[3, 0, 0, 0, 0, 0, 0, 4, 0, 2]);
        assert_eq!(&sent[46..52], &[5, 1, 0, 3, 0, 1]);
        assert_eq!(&sent[52..60], &[4, 1, 0, 0, 0, 0, 0xff, 0x0d]);
    }

    #[test]
    fn refuses_a_server_that_wants_a_password() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"RFB 003.008\n").unwrap();
            let mut v = [0u8; 12];
            s.read_exact(&mut v).unwrap();
            s.write_all(&[1, 2]).unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest);
        });
        let err = Client::connect(&addr, Duration::from_secs(5))
            .err()
            .expect("password servers are refused");
        assert!(err.to_string().contains("authentication"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn chords_map_to_keysyms() {
        assert_eq!(parse_chord("ctrl+c").unwrap(), (vec![0xffe3], 0x63));
        assert_eq!(
            parse_chord("cmd+shift+T").unwrap(),
            (vec![0xffeb, 0xffe1], 0x74)
        );
        assert_eq!(parse_chord("Return").unwrap(), (vec![], 0xff0d));
        assert_eq!(parse_chord("F12").unwrap(), (vec![], 0xffc9));
        assert_eq!(parse_chord("ctrl++").unwrap(), (vec![0xffe3], 0x2b));
        assert!(parse_chord("ctrl").is_err());
        assert!(parse_chord("a+b").is_err());
        assert!(parse_chord("ctrl+nope").is_err());
    }

    #[test]
    fn characters_map_to_keysyms() {
        assert_eq!(keysym_for_char('a'), 0x61);
        assert_eq!(keysym_for_char('A'), 0x41);
        assert_eq!(keysym_for_char('\n'), 0xff0d);
        assert_eq!(keysym_for_char('é'), 0xe9);
        assert_eq!(keysym_for_char('→'), 0x0100_2192);
    }

    #[test]
    fn versions_parse_and_old_ones_are_refused() {
        assert_eq!(parse_version(b"RFB 003.008\n").unwrap(), 8);
        assert_eq!(parse_version(b"RFB 003.889\n").unwrap(), 889);
        assert!(parse_version(b"RFB 002.000\n").is_err());
        assert!(parse_version(b"HTTP/1.1 200").is_err());
    }
}
