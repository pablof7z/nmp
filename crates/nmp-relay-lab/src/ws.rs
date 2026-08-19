//! The websocket server half, written directly onto the TCP socket.
//!
//! This is the layer a relay library would own, and owning it is the point.
//! A scenario in this crate can write half a frame, write octets that are not
//! a frame at all, stop writing without closing, or answer the upgrade with a
//! login page -- none of which is expressible above a library that owns the
//! socket. RFC 6455 is implemented only as far as NIP-01 traffic needs it,
//! and every unhandled case is recorded as a fault rather than skipped: a
//! silently dropped inbound frame would turn a red scenario green, which is
//! the single failure mode this whole crate exists to prevent.

use base64::Engine as _;
use sha1::{Digest, Sha1};

/// RFC 6455 §1.3. Note the final segment: `95CA-C5AB0DC85B11`, not
/// `95CA-5AB0DC85B11F`. Transposing it produces a perfectly well-formed
/// accept key that every client rejects, and the symptom is a relay that
/// completes the HTTP exchange and then sees an immediate EOF.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key`.
#[must_use]
pub fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// The `Sec-WebSocket-Key` header value in a request head, if there is one.
#[must_use]
pub fn websocket_key(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n") {
        // The request line (`GET / HTTP/1.1`) carries no colon. Skipping it
        // rather than propagating is the whole of this function: an early
        // `?` here returns `None` for EVERY request, and the symptom is a
        // relay that accepts connections and never speaks.
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("sec-websocket-key") {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// True iff this request head asks to become a websocket.
#[must_use]
pub fn is_upgrade(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    text.contains("upgrade: websocket") || text.contains("sec-websocket-key")
}

/// Where the request headers end, if they are complete.
#[must_use]
pub fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// The `101 Switching Protocols` response for a client key.
#[must_use]
pub fn handshake_response(client_key: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(client_key)
    )
    .into_bytes()
}

/// A frame's opcode, as far as this crate distinguishes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opcode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
    Other(u8),
}

impl Opcode {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0x0 => Self::Continuation,
            0x1 => Self::Text,
            0x2 => Self::Binary,
            0x8 => Self::Close,
            0x9 => Self::Ping,
            0xa => Self::Pong,
            other => Self::Other(other),
        }
    }
}

/// One decoded inbound frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub opcode: Opcode,
    pub fin: bool,
    pub payload: Vec<u8>,
}

/// Incremental decoder for the client-to-server byte stream.
///
/// `&mut` and per-connection by construction: it is a stateful reassembler
/// and two connections' bytes must never share one.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Reassembly buffer for a fragmented message, with the opcode the first
    /// fragment declared. NMP's own client does not fragment, but a decoder
    /// that assumes so would silently lose whole REQs if it ever started.
    partial: Option<(Opcode, Vec<u8>)>,
}

/// What the decoder produced, or the reason it cannot honestly continue.
#[derive(Debug)]
pub enum Decoded {
    /// A complete message (continuations already reassembled).
    Message(Frame),
    /// Not enough bytes yet.
    Incomplete,
    /// Something this decoder cannot account for. The caller records it as a
    /// fault; it never silently skips.
    Fault(String),
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Take the next complete message off the buffer.
    pub fn take_message(&mut self) -> Decoded {
        let Some(raw) = self.take_raw_frame() else {
            return Decoded::Incomplete;
        };
        let raw = match raw {
            Ok(frame) => frame,
            Err(fault) => return Decoded::Fault(fault),
        };

        match (&raw.opcode, raw.fin) {
            // Control frames are never fragmented and never joined.
            (Opcode::Close | Opcode::Ping | Opcode::Pong, _) => Decoded::Message(raw),
            (Opcode::Continuation, _) => match self.partial.take() {
                Some((opcode, mut acc)) => {
                    acc.extend_from_slice(&raw.payload);
                    if raw.fin {
                        Decoded::Message(Frame {
                            opcode,
                            fin: true,
                            payload: acc,
                        })
                    } else {
                        self.partial = Some((opcode, acc));
                        Decoded::Incomplete
                    }
                }
                None => Decoded::Fault("continuation frame with nothing to continue".to_string()),
            },
            (_, true) => Decoded::Message(raw),
            (opcode, false) => {
                self.partial = Some((opcode.clone(), raw.payload));
                Decoded::Incomplete
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn take_raw_frame(&mut self) -> Option<Result<Frame, String>> {
        if self.buf.len() < 2 {
            return None;
        }
        let fin = self.buf[0] & 0x80 != 0;
        let opcode = Opcode::from_bits(self.buf[0] & 0x0f);
        let masked = self.buf[1] & 0x80 != 0;
        let short = (self.buf[1] & 0x7f) as usize;

        let (len, mut offset) = match short {
            126 => {
                if self.buf.len() < 4 {
                    return None;
                }
                (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4)
            }
            127 => {
                if self.buf.len() < 10 {
                    return None;
                }
                let mut be = [0u8; 8];
                be.copy_from_slice(&self.buf[2..10]);
                (u64::from_be_bytes(be) as usize, 10)
            }
            n => (n, 2),
        };

        let mask = if masked {
            if self.buf.len() < offset + 4 {
                return None;
            }
            let mask = [
                self.buf[offset],
                self.buf[offset + 1],
                self.buf[offset + 2],
                self.buf[offset + 3],
            ];
            offset += 4;
            Some(mask)
        } else {
            // RFC 6455 §5.1: a client MUST mask. An unmasked client frame is
            // a protocol violation, not something to decode anyway.
            None
        };

        let total = offset + len;
        if self.buf.len() < total {
            return None;
        }
        let mut payload = self.buf[offset..total].to_vec();
        self.buf.drain(..total);
        match mask {
            Some(mask) => {
                for (i, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[i % 4];
                }
            }
            None => {
                return Some(Err(
                    "client sent an UNMASKED frame; RFC 6455 requires masking".to_string()
                ))
            }
        }
        Some(Ok(Frame {
            opcode,
            fin,
            payload,
        }))
    }
}

/// Encode one server-to-client frame. Server frames are never masked.
#[must_use]
pub fn encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        frame.push(len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

/// A complete server-to-client TEXT frame.
#[must_use]
pub fn text_frame(payload: &str) -> Vec<u8> {
    encode(0x1, payload.as_bytes())
}

/// A server-to-client CLOSE frame.
#[must_use]
pub fn close_frame(code: u16, reason: &str) -> Vec<u8> {
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(reason.as_bytes());
    encode(0x8, &payload)
}

/// A server-to-client PONG carrying the ping's payload, as RFC 6455 requires.
#[must_use]
pub fn pong_frame(payload: &[u8]) -> Vec<u8> {
    encode(0xa, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked_text(payload: &str) -> Vec<u8> {
        let mask = [0xa1u8, 0x0b, 0xc3, 0x5d];
        let bytes = payload.as_bytes();
        let mut frame = vec![0x81];
        if bytes.len() < 126 {
            frame.push(0x80 | bytes.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        frame
    }

    /// The RFC 6455 §1.3 worked example, and it earns its place.
    ///
    /// This test caught a transposed character in [`WS_GUID`] that no amount
    /// of cross-checking the ARITHMETIC would have found: sha1 and base64 are
    /// easy to verify against three implementations, and all three agree with
    /// each other while being fed the same wrong input. Only a constant with
    /// an external oracle catches a wrong constant.
    ///
    /// The end-to-end falsifier is that NMP's own `tungstenite` client
    /// rejects a wrong accept key, but its symptom is remote from its cause:
    /// the relay completes the HTTP exchange, logs nothing wrong, and then
    /// reads EOF.
    #[test]
    fn the_handshake_accept_key_matches_the_rfc_worked_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// Fed one byte at a time -- every reassembly boundary exercised -- the
    /// decoder must still recover both whole messages, in order.
    #[test]
    fn masked_client_frames_decode_across_arbitrary_byte_boundaries() {
        let mut decoder = Decoder::default();
        let mut stream = masked_text(r#"["REQ","a",{"kinds":[1]}]"#);
        stream.extend(masked_text(r#"["CLOSE","a"]"#));

        let mut messages = Vec::new();
        for byte in stream {
            decoder.push(&[byte]);
            loop {
                match decoder.take_message() {
                    Decoded::Message(frame) => {
                        messages.push(String::from_utf8(frame.payload).expect("text"));
                    }
                    Decoded::Incomplete => break,
                    Decoded::Fault(fault) => panic!("unexpected fault: {fault}"),
                }
            }
        }
        assert_eq!(
            messages,
            vec![
                r#"["REQ","a",{"kinds":[1]}]"#.to_string(),
                r#"["CLOSE","a"]"#.to_string()
            ]
        );
    }

    /// A fragmented message must be rejoined, not lost and not split into two
    /// half-messages that decode as neither.
    #[test]
    fn a_fragmented_text_message_is_reassembled() {
        let mask = [0u8; 4];
        let mut decoder = Decoder::default();
        // First fragment: text, FIN clear.
        let mut first = vec![0x01, 0x80 | 3];
        first.extend_from_slice(&mask);
        first.extend_from_slice(b"[\"R");
        // Continuation: opcode 0x0, FIN set.
        let tail = b"EQ\",\"a\",{}]";
        let mut rest = vec![0x80, 0x80 | tail.len() as u8];
        rest.extend_from_slice(&mask);
        rest.extend_from_slice(tail);

        decoder.push(&first);
        assert!(matches!(decoder.take_message(), Decoded::Incomplete));
        decoder.push(&rest);
        match decoder.take_message() {
            Decoded::Message(frame) => {
                assert_eq!(String::from_utf8(frame.payload).unwrap(), r#"["REQ","a",{}]"#);
            }
            other => panic!("fragmented message was not reassembled: {other:?}"),
        }
    }

    /// An unmasked client frame is an RFC violation; reporting it as a fault
    /// is what keeps a scenario from counting frames off a corrupt stream.
    #[test]
    fn an_unmasked_client_frame_is_a_fault_not_a_message() {
        let mut decoder = Decoder::default();
        decoder.push(&[0x81, 0x02, b'h', b'i']);
        assert!(matches!(decoder.take_message(), Decoded::Fault(_)));
    }

    /// Round trip through the server encoder at each of the three length
    /// forms, since a wrong length byte desynchronises the client forever.
    #[test]
    fn server_frames_encode_at_every_length_form() {
        for len in [10usize, 200, 70_000] {
            let payload = "x".repeat(len);
            let frame = text_frame(&payload);
            let header = match len {
                0..=125 => 2,
                126..=65535 => 4,
                _ => 10,
            };
            assert_eq!(frame.len(), header + len, "length form for {len}");
            assert_eq!(frame[0], 0x81);
            assert_eq!(&frame[header..], payload.as_bytes());
        }
    }
}
