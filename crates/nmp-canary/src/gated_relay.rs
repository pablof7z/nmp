//! A NIP-42 read-gating relay, in-process, dependency-free.
//!
//! ## Why this is here and not in a test harness
//!
//! The engine's AUTH read path has never been exercised against a relay that
//! GATES READS the way `strfry` does. A gating relay answers a `REQ` it will
//! not serve with two frames:
//!
//! ```text
//! ["AUTH", "<challenge>"]
//! ["CLOSED", "<sub>", "auth-required: we only serve authenticated clients"]
//! ```
//!
//! Both are ordinary NIP-42. The second one is a DEMAND for authentication --
//! NIP-42 says the client should authenticate and re-issue its `REQ` -- not a
//! refusal of an AUTH the client already offered.
//!
//! ## Why it is hand-rolled
//!
//! `nmp-canary`'s dependency list is part of its evidence (see the crate's
//! `Cargo.toml`): an app names `nmp` plus one line per capability. A relay is
//! not a capability an app uses, so it costs no dependency line. The RFC 6455
//! server handshake needs SHA-1 and base64 and nothing else, and both are
//! forty lines.
//!
//! It speaks exactly enough of NIP-01/NIP-42 to gate a read, and records every
//! client frame it received so a scenario can state -- rather than infer --
//! whether an `["AUTH", <event>]` ever reached the socket.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// When the relay issues its NIP-42 challenge. Both are relays that exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Challenge {
    /// Challenge the moment the socket is up, before the client asks for
    /// anything.
    OnConnect,
    /// Say nothing until an unauthenticated client sends a `REQ`, then answer
    /// it with `["AUTH", challenge]` followed by
    /// `["CLOSED", sub, "auth-required: ..."]`. This is `strfry`'s shape, and
    /// it is the one the engine's own `on_relay_connected` comment (#1889)
    /// describes as the reason a protected session sends its `REQ` before it
    /// is authenticated.
    OnRequest,
    /// Demand authentication and never issue a challenge. A relay in this
    /// state is broken -- there is nothing for a client to sign -- and it is
    /// here to bound what the engine does about it. Whatever that is, it must
    /// not be an unbounded `REQ` loop.
    Never,
}

/// A running relay. Dropping it stops accepting; the listener thread is
/// detached and the process exits at the end of a scenario either way.
pub struct GatedRelay {
    url: String,
    log: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl GatedRelay {
    /// Bind an ephemeral port on loopback and serve until dropped.
    /// `accept_auth` false makes the relay answer the client's kind:22242
    /// with `OK false` -- the relay's own refusal, which is a different fact
    /// from the app's policy or signer refusing.
    pub fn start(challenge: Challenge, accept_auth: bool) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_log = Arc::clone(&log);
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let session_log = Arc::clone(&thread_log);
                let session_stop = Arc::clone(&thread_stop);
                std::thread::spawn(move || {
                    let _ = serve(stream, challenge, accept_auth, &session_log, &session_stop);
                });
            }
        });
        Ok(Self {
            url: format!("ws://127.0.0.1:{port}"),
            log,
            stop,
        })
    }

    /// The `ws://` URL an engine connects to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Every client frame this relay received, in arrival order.
    #[must_use]
    pub fn client_frames(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Whether an `["AUTH", <event>]` ever reached the socket. This is the
    /// single fact the read-gate scenario turns on.
    #[must_use]
    pub fn saw_client_auth(&self) -> bool {
        self.client_frames()
            .iter()
            .any(|frame| frame.trim_start().starts_with("[\"AUTH\""))
    }

    /// How many `REQ` frames the client sent. A client that authenticates
    /// after a gated close must re-issue its `REQ`, so a working read path
    /// sends at least two.
    #[must_use]
    pub fn req_count(&self) -> usize {
        self.client_frames()
            .iter()
            .filter(|frame| frame.trim_start().starts_with("[\"REQ\""))
            .count()
    }
}

impl Drop for GatedRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

const CHALLENGE: &str = "canary-read-gate-challenge";

fn serve(
    mut stream: TcpStream,
    challenge: Challenge,
    accept_auth: bool,
    log: &Arc<Mutex<Vec<String>>>,
    stop: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let key = read_handshake(&mut stream)?;
    let accept = base64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    stream.write_all(
        format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .as_bytes(),
    )?;

    if challenge == Challenge::OnConnect {
        send_text(&mut stream, &format!("[\"AUTH\",\"{CHALLENGE}\"]"))?;
    }

    let mut authenticated = false;
    let mut buffered = VecDeque::new();
    while !stop.load(Ordering::SeqCst) {
        let Some(frame) = read_text(&mut stream, &mut buffered)? else {
            return Ok(());
        };
        log.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(frame.clone());

        if frame.trim_start().starts_with("[\"AUTH\"") {
            // Correlate the OK by the event id, which is the value of the
            // `"id"` member of the AUTH event. Nothing here verifies the
            // signature: the point under test is whether the frame arrives.
            let id = json_string_member(&frame, "id").unwrap_or_default();
            if accept_auth {
                authenticated = true;
                send_text(&mut stream, &format!("[\"OK\",\"{id}\",true,\"\"]"))?;
            } else {
                send_text(
                    &mut stream,
                    &format!(
                        "[\"OK\",\"{id}\",false,\"restricted: \
                         this identity is not on the allow list\"]"
                    ),
                )?;
            }
            continue;
        }

        if frame.trim_start().starts_with("[\"REQ\"") {
            let sub = json_nth_string(&frame, 1).unwrap_or_default();
            if authenticated {
                send_text(&mut stream, &format!("[\"EOSE\",\"{sub}\"]"))?;
            } else {
                // NIP-42's order for a gated read: challenge first, then say
                // why this subscription is closing. A client that has not
                // been challenged yet has nothing to sign, so a relay that
                // waits for the REQ must send both.
                if challenge == Challenge::OnRequest {
                    send_text(&mut stream, &format!("[\"AUTH\",\"{CHALLENGE}\"]"))?;
                }

                send_text(
                    &mut stream,
                    &format!(
                        "[\"CLOSED\",\"{sub}\",\"auth-required: \
                         this relay serves reads to authenticated clients only\"]"
                    ),
                )?;
            }
        }
    }
    Ok(())
}

fn read_handshake(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        request.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&request).to_string();
    Ok(text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("Sec-WebSocket-Key")
                .then(|| value.trim().to_string())
        })
        .unwrap_or_default())
}

/// One text frame, or `None` at end of stream / on a close frame. Control
/// frames other than close are answered inline and never returned.
fn read_text(
    stream: &mut TcpStream,
    pending: &mut VecDeque<String>,
) -> std::io::Result<Option<String>> {
    loop {
        if let Some(ready) = pending.pop_front() {
            return Ok(Some(ready));
        }
        let mut header = [0u8; 2];
        if read_exact_or_eof(stream, &mut header)?.is_none() {
            return Ok(None);
        }
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut length = u64::from(header[1] & 0x7f);
        if length == 126 {
            let mut extended = [0u8; 2];
            if read_exact_or_eof(stream, &mut extended)?.is_none() {
                return Ok(None);
            }
            length = u64::from(u16::from_be_bytes(extended));
        } else if length == 127 {
            let mut extended = [0u8; 8];
            if read_exact_or_eof(stream, &mut extended)?.is_none() {
                return Ok(None);
            }
            length = u64::from_be_bytes(extended);
        }
        let mut mask = [0u8; 4];
        if masked && read_exact_or_eof(stream, &mut mask)?.is_none() {
            return Ok(None);
        }
        let mut payload = vec![0u8; usize::try_from(length).unwrap_or(0)];
        if !payload.is_empty() && read_exact_or_eof(stream, &mut payload)?.is_none() {
            return Ok(None);
        }
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        match opcode {
            0x1 => pending.push_back(String::from_utf8_lossy(&payload).to_string()),
            0x8 => return Ok(None),
            0x9 => write_frame(stream, 0xa, &payload)?,
            _ => {}
        }
    }
}

fn read_exact_or_eof(stream: &mut TcpStream, buffer: &mut [u8]) -> std::io::Result<Option<()>> {
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..])? {
            0 => return Ok(None),
            read => filled += read,
        }
    }
    Ok(Some(()))
}

fn send_text(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    write_frame(stream, 0x1, text.as_bytes())
}

fn write_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = vec![0x80 | opcode];
    let length = payload.len();
    if length < 126 {
        frame.push(u8::try_from(length).expect("length < 126"));
    } else if let Ok(short) = u16::try_from(length) {
        frame.push(126);
        frame.extend_from_slice(&short.to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()
}

/// The value of a top-level `"name":"value"` member, good enough for the two
/// shapes this relay reads back.
fn json_string_member(text: &str, name: &str) -> Option<String> {
    let needle = format!("\"{name}\":\"");
    let start = text.find(&needle)? + needle.len();
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

/// The nth element of a JSON array whose elements up to `n` are strings --
/// exactly the `["REQ","<sub>",{...}]` shape.
fn json_nth_string(text: &str, index: usize) -> Option<String> {
    let mut cursor = text.find('[')? + 1;
    for position in 0..=index {
        let rest = &text[cursor..];
        let open = rest.find('"')? + cursor + 1;
        let close = text[open..].find('"')? + open;
        if position == index {
            return Some(text[open..close].to_string());
        }
        cursor = close + 1;
    }
    None
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let block = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for slot in 0..4 {
            if slot <= chunk.len() {
                let index = (block >> (18 - 6 * slot)) & 0x3f;
                out.push(char::from(ALPHABET[index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn sha1(message: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0];
    let mut padded = message.to_vec();
    let bits = (message.len() as u64) * 8;
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bits.to_be_bytes());

    for block in padded.chunks(64) {
        let mut words = [0u32; 80];
        for (index, word) in block.chunks(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.iter().enumerate() {
            let (mix, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(mix)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut digest = [0u8; 20];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}
